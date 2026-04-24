use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc::Receiver;

use crate::display_manager::DisplayManager;
use crate::layout::PanelSpecData;

/// A rasterized panel frame ready to be committed to a display.
///
/// Pixel data is held behind `Arc<[u8]>` so the pipeline, the command
/// channel, and the presenter's coalescing buffer can share one allocation.
#[derive(Clone)]
pub struct PanelFrame {
    pub pixels: Arc<[u8]>,
    pub width: u32,
    pub height: u32,
}

/// The typed vocabulary the pipeline speaks to the presenter.
///
/// Lifecycle variants (`Create`, `Move`, `Resize`, `Delete`) are ordered and
/// discrete; the presenter applies them immediately. `UpdatePicture` is
/// latest-wins per panel id — the presenter coalesces multiple updates and
/// commits only the most recent one when the backend is ready.
pub enum PanelCommand {
    Create(PanelSpecData),
    Move(PanelSpecData),
    Resize(PanelSpecData),
    Delete { id: String },
    UpdatePicture { id: String, frame: PanelFrame },
}

/// Owns the window state (one `DM::Panel` per live panel id) and pending
/// pixel updates. Does NOT own the `DisplayManager` — callers pass `&mut DM`
/// into `apply`/`drain`, which keeps the App free to access other DM fields
/// (render state, conn handles) alongside the presenter in the same tick.
pub struct Presenter<DM: DisplayManager> {
    pub panels: HashMap<String, DM::Panel>,
    pub pending_pixels: HashMap<String, PanelFrame>,
}

impl<DM: DisplayManager> Default for Presenter<DM> {
    fn default() -> Self {
        Self { panels: HashMap::new(), pending_pixels: HashMap::new() }
    }
}

impl<DM: DisplayManager> Presenter<DM>
where DM::Error: std::fmt::Display
{
    pub fn new() -> Self { Self::default() }

    pub fn apply(&mut self, cmd: PanelCommand, dm: &mut DM) -> anyhow::Result<()> {
        match cmd {
            PanelCommand::Create(spec) => {
                let id = spec.id.clone();
                let panel = dm.create_window(&spec).map_err(|e| anyhow::anyhow!("{e}"))?;
                self.panels.insert(id, panel);
            }
            PanelCommand::Move(spec) => {
                if let Some(panel) = self.panels.get_mut(&spec.id) {
                    dm.update_position(panel, &spec).map_err(|e| anyhow::anyhow!("{e}"))?;
                }
            }
            PanelCommand::Resize(spec) => {
                if let Some(panel) = self.panels.get_mut(&spec.id) {
                    dm.update_dimensions(panel, &spec).map_err(|e| anyhow::anyhow!("{e}"))?;
                }
            }
            PanelCommand::Delete { id } => {
                self.pending_pixels.remove(&id);
                if let Some(panel) = self.panels.remove(&id) {
                    dm.delete_window(panel).map_err(|e| anyhow::anyhow!("{e}"))?;
                }
            }
            PanelCommand::UpdatePicture { id, frame } => {
                self.pending_pixels.insert(id, frame);
            }
        }
        Ok(())
    }

    /// Drain all pending commands and apply them. Per-command errors are
    /// logged so one bad command does not abort the rest.
    pub fn drain(&mut self, rx: &Receiver<PanelCommand>, dm: &mut DM) {
        while let Ok(cmd) = rx.try_recv() {
            if let Err(e) = self.apply(cmd, dm) {
                tracing::error!(error = %e, "presenter apply failed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockDM {
        calls: Vec<String>,
        next_id: u32,
    }

    impl MockDM {
        fn new() -> Self { MockDM { calls: Vec::new(), next_id: 0 } }
    }

    impl DisplayManager for MockDM {
        type Panel = u32;
        type Error = String;
        fn create_window(&mut self, spec: &PanelSpecData) -> Result<u32, String> {
            self.next_id += 1;
            self.calls.push(format!("create:{}:{}", spec.id, self.next_id));
            Ok(self.next_id)
        }
        fn update_position(&mut self, panel: &mut u32, spec: &PanelSpecData) -> Result<(), String> {
            self.calls.push(format!("move:{}:{}", spec.id, panel)); Ok(())
        }
        fn update_dimensions(&mut self, panel: &mut u32, spec: &PanelSpecData) -> Result<(), String> {
            self.calls.push(format!("resize:{}:{}", spec.id, panel)); Ok(())
        }
        fn update_image(&mut self, panel: &mut u32, _bgrx: &[u8]) -> Result<(), String> {
            self.calls.push(format!("image:{}", panel)); Ok(())
        }
        fn delete_window(&mut self, panel: u32) -> Result<(), String> {
            self.calls.push(format!("delete:{}", panel)); Ok(())
        }
    }

    fn spec(id: &str) -> PanelSpecData {
        PanelSpecData {
            id: id.to_string(),
            anchor: None,
            width: 100, height: 30,
            x: 0, y: 0,
            outer_gap: 0,
            output: None,
            above: false,
            content: serde_json::Value::Null,
        }
    }

    #[test]
    fn presenter_create_calls_dm_create_window_and_tracks_panel() {
        let mut p: Presenter<MockDM> = Presenter::new();
        let mut dm = MockDM::new();
        p.apply(PanelCommand::Create(spec("p1")), &mut dm).unwrap();
        assert!(p.panels.contains_key("p1"), "panel id must be tracked after Create");
        assert!(dm.calls.iter().any(|c| c.starts_with("create:p1")), "dm.calls: {:?}", dm.calls);
    }

    #[test]
    fn presenter_delete_removes_panel_and_calls_dm_delete_window() {
        let mut p: Presenter<MockDM> = Presenter::new();
        let mut dm = MockDM::new();
        p.apply(PanelCommand::Create(spec("p1")), &mut dm).unwrap();
        p.apply(PanelCommand::Delete { id: "p1".to_string() }, &mut dm).unwrap();
        assert!(!p.panels.contains_key("p1"), "panel id must be removed after Delete");
        assert!(dm.calls.iter().any(|c| c.starts_with("delete:")), "dm.calls: {:?}", dm.calls);
    }

    #[test]
    fn presenter_update_picture_coalesces_per_panel_id() {
        let mut p: Presenter<MockDM> = Presenter::new();
        let mut dm = MockDM::new();
        p.apply(PanelCommand::Create(spec("p1")), &mut dm).unwrap();
        let f1 = PanelFrame { pixels: Arc::from(vec![0u8; 4]), width: 1, height: 1 };
        let f2 = PanelFrame { pixels: Arc::from(vec![1u8; 4]), width: 1, height: 1 };
        p.apply(PanelCommand::UpdatePicture { id: "p1".to_string(), frame: f1 }, &mut dm).unwrap();
        p.apply(PanelCommand::UpdatePicture { id: "p1".to_string(), frame: f2 }, &mut dm).unwrap();
        assert_eq!(p.pending_pixels.len(), 1, "second UpdatePicture must replace first for same id");
    }

    #[test]
    fn presenter_move_and_resize_only_affect_matching_id() {
        let mut p: Presenter<MockDM> = Presenter::new();
        let mut dm = MockDM::new();
        p.apply(PanelCommand::Create(spec("p1")), &mut dm).unwrap();
        p.apply(PanelCommand::Move(spec("p2")), &mut dm).unwrap(); // unknown id: no-op
        p.apply(PanelCommand::Resize(spec("p1")), &mut dm).unwrap();
        assert!(dm.calls.iter().any(|c| c.starts_with("resize:p1")), "Resize on known id must call dm");
        assert!(!dm.calls.iter().any(|c| c.starts_with("move:")), "Move on unknown id must be a no-op");
    }
}
