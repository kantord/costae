use std::collections::HashMap;
use std::sync::Arc;

use crate::display_manager::DisplayManager;
use crate::layout::PanelSpecData;

/// A rasterized panel frame ready to be committed to a display.
///
/// Pixel data is `Arc<Vec<u8>>` so the pipeline, the command channel, and
/// the presenter's coalescing buffer share one allocation via ref-count.
/// X11's existing `Panel::bgrx` is already this type, so the pipeline can
/// clone the Arc directly with no byte copy.
#[derive(Clone)]
pub struct PanelFrame {
    pub pixels: Arc<Vec<u8>>,
    pub width: u32,
    pub height: u32,
}

/// The typed vocabulary the pipeline speaks to the presenter.
///
/// Lifecycle variants (`Create`, `Move`, `Resize`, `Delete`) are ordered and
/// discrete; the presenter applies them immediately. `UpdatePicture` is
/// latest-wins per panel id — the presenter coalesces multiple updates and
/// commits only the most recent one when the backend is ready.
/// `RenderAll` tells the presenter to render every live panel and flush.
/// `UpdateOutputMap` delivers updated monitor geometry to the X11 backend.
/// `Shutdown` cleanly stops the presenter thread.
pub enum PanelCommand {
    Create(PanelSpecData),
    Move(PanelSpecData),
    Resize(PanelSpecData),
    Delete { id: String },
    UpdatePicture { id: String, frame: PanelFrame },
    RenderAll,
    Shutdown,
}

/// Events the presenter thread sends back to the pipeline.
pub enum PresenterEvent {
    /// The pipeline should re-render all panels and flush.
    NeedsRender,
    /// Wayland: the compositor's output geometry changed.
    OutputsChanged { screen_width: u32, screen_height: u32 },
    /// A click event, routed back for hit-testing in the pipeline.
    Click { panel_id: String, x: f32, y: f32, phys_width: u32, phys_height: u32, dpr: f32 },
}

/// Owns the window state (one `DM::Panel` per live panel id) and pending
/// pixel updates. Does NOT own the `DisplayManager` — callers pass `&mut DM`
/// into `apply`/`flush_pixels`.
pub struct Presenter<DM: DisplayManager> {
    pub panels: HashMap<String, DM::Panel>,
    pub pending_pixels: HashMap<String, PanelFrame>,
}

/// Bundles `dm: DM` and `presenter: Presenter<DM>` so they travel together
/// as one owned unit. Lives on a dedicated thread; the main `App` interacts
/// with it only through `PanelCommand` / `PresenterEvent` mpsc channels.
pub struct PresentationThread<DM: DisplayManager> {
    pub dm: DM,
    pub presenter: Presenter<DM>,
}

impl<DM: DisplayManager> PresentationThread<DM> {
    pub fn new(dm: DM) -> Self {
        Self { dm, presenter: Presenter::new() }
    }
}

impl<DM: DisplayManager> Default for Presenter<DM> {
    fn default() -> Self {
        Self { panels: HashMap::new(), pending_pixels: HashMap::new() }
    }
}

impl<DM: DisplayManager> Presenter<DM> {
    pub fn new() -> Self { Self::default() }

    pub fn apply(&mut self, cmd: PanelCommand, dm: &mut DM) -> anyhow::Result<()> {
        match cmd {
            PanelCommand::Create(spec) => {
                let id = spec.id.clone();
                let panel = dm.create_window(&spec)?;
                self.panels.insert(id, panel);
            }
            PanelCommand::Move(spec) => {
                if let Some(panel) = self.panels.get_mut(&spec.id) {
                    dm.update_position(panel, &spec)?;
                }
            }
            PanelCommand::Resize(spec) => {
                if let Some(panel) = self.panels.get_mut(&spec.id) {
                    dm.update_dimensions(panel, &spec)?;
                }
            }
            PanelCommand::Delete { id } => {
                self.pending_pixels.remove(&id);
                if let Some(panel) = self.panels.remove(&id) {
                    dm.delete_window(panel)?;
                }
            }
            PanelCommand::UpdatePicture { id, frame } => {
                self.pending_pixels.insert(id, frame);
            }
            // Thread-level commands handled by the presenter thread loop, not here.
            PanelCommand::RenderAll
            | PanelCommand::Shutdown => {}
        }
        Ok(())
    }

    /// Commit all coalesced pixel updates to the backend. Each pending entry
    /// is fed to `DM::update_image` for the matching panel; entries for
    /// unknown panel ids are dropped silently (likely a Delete arrived
    /// between the UpdatePicture and this flush).
    pub fn flush_pixels(&mut self, dm: &mut DM) {
        for (id, frame) in self.pending_pixels.drain() {
            let Some(panel) = self.panels.get_mut(&id) else { continue; };
            if let Err(e) = dm.update_image(panel, &frame.pixels[..]) {
                tracing::error!(panel = %id, error = %e, "presenter flush_pixels failed");
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
        fn create_window(&mut self, spec: &PanelSpecData) -> anyhow::Result<u32> {
            self.next_id += 1;
            self.calls.push(format!("create:{}:{}", spec.id, self.next_id));
            Ok(self.next_id)
        }
        fn update_position(&mut self, panel: &mut u32, spec: &PanelSpecData) -> anyhow::Result<()> {
            self.calls.push(format!("move:{}:{}", spec.id, panel)); Ok(())
        }
        fn update_dimensions(&mut self, panel: &mut u32, spec: &PanelSpecData) -> anyhow::Result<()> {
            self.calls.push(format!("resize:{}:{}", spec.id, panel)); Ok(())
        }
        fn update_image(&mut self, panel: &mut u32, _bgrx: &[u8]) -> anyhow::Result<()> {
            self.calls.push(format!("image:{}", panel)); Ok(())
        }
        fn delete_window(&mut self, panel: u32) -> anyhow::Result<()> {
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
        let f1 = PanelFrame { pixels: Arc::new(vec![0u8; 4]), width: 1, height: 1 };
        let f2 = PanelFrame { pixels: Arc::new(vec![1u8; 4]), width: 1, height: 1 };
        p.apply(PanelCommand::UpdatePicture { id: "p1".to_string(), frame: f1 }, &mut dm).unwrap();
        p.apply(PanelCommand::UpdatePicture { id: "p1".to_string(), frame: f2 }, &mut dm).unwrap();
        assert_eq!(p.pending_pixels.len(), 1, "second UpdatePicture must replace first for same id");
    }

    #[test]
    fn presenter_flush_pixels_calls_update_image_and_clears_pending() {
        let mut p: Presenter<MockDM> = Presenter::new();
        let mut dm = MockDM::new();
        p.apply(PanelCommand::Create(spec("p1")), &mut dm).unwrap();
        let frame = PanelFrame { pixels: Arc::new(vec![42u8; 4]), width: 1, height: 1 };
        p.apply(PanelCommand::UpdatePicture { id: "p1".to_string(), frame }, &mut dm).unwrap();
        assert_eq!(p.pending_pixels.len(), 1, "UpdatePicture must populate pending_pixels");

        p.flush_pixels(&mut dm);
        assert!(p.pending_pixels.is_empty(), "flush_pixels must drain pending_pixels");
        assert!(dm.calls.iter().any(|c| c.starts_with("image:")),
            "flush_pixels must call dm.update_image; got {:?}", dm.calls);
    }

    #[test]
    fn presenter_flush_pixels_drops_entries_for_deleted_panels() {
        let mut p: Presenter<MockDM> = Presenter::new();
        let mut dm = MockDM::new();
        p.apply(PanelCommand::Create(spec("p1")), &mut dm).unwrap();
        let frame = PanelFrame { pixels: Arc::new(vec![42u8; 4]), width: 1, height: 1 };
        // UpdatePicture first, then Delete — pending entry should be gone after Delete.
        p.apply(PanelCommand::UpdatePicture { id: "p1".to_string(), frame }, &mut dm).unwrap();
        p.apply(PanelCommand::Delete { id: "p1".to_string() }, &mut dm).unwrap();
        assert!(p.pending_pixels.is_empty(), "Delete must invalidate pending pixel entry for that id");

        // flush_pixels is a no-op on the update_image path now.
        let calls_before = dm.calls.len();
        p.flush_pixels(&mut dm);
        assert_eq!(dm.calls.len(), calls_before, "flush_pixels must not call update_image for a deleted panel");
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
