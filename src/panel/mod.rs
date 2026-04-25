use std::sync::mpsc::Sender;

use crate::layout::PanelSpecData;
use crate::managed_set::Lifecycle;
use crate::presentation::PanelCommand;
use crate::x11::panel::Panel;
pub use crate::x11::panel::X11PanelContext;

// ---------------------------------------------------------------------------
// Stub Wayland types — kept public for downstream code that still references
// them. The real Wayland panel state lives in windowing::wayland.
// ---------------------------------------------------------------------------

pub struct WaylandPanelContext;

impl WaylandPanelContext {
    pub fn test_stub() -> Self { WaylandPanelContext }
}

pub struct WaylandPanel;

pub enum PanelContext {
    X11(X11PanelContext),
    Wayland(WaylandPanelContext),
}

pub enum PanelState {
    X11(Panel),
    Wayland(WaylandPanel),
}

// ---------------------------------------------------------------------------
// PanelSpec — pipeline-side tracker of desired panels. Emits typed
// PanelCommand messages on lifecycle transitions; does NOT call DisplayManager
// methods directly. The presenter (src/presentation) applies the commands to
// an actual backend.
// ---------------------------------------------------------------------------

pub struct PanelSpec(pub PanelSpecData);

impl std::fmt::Display for PanelSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0.id)
    }
}

impl Lifecycle for PanelSpec {
    type Key = String;
    /// The pipeline tracks the last-reconciled spec so reconcile_self can diff
    /// and emit Move/Resize commands only when something actually changed.
    type State = PanelSpecData;
    type Context = ();
    type Output = Sender<PanelCommand>;
    type Error = anyhow::Error;

    fn key(&self) -> String {
        self.0.id.clone()
    }

    fn enter(self, _ctx: &mut (), output: &mut Sender<PanelCommand>) -> Result<PanelSpecData, anyhow::Error> {
        let _ = output.send(PanelCommand::Create(self.0.clone()));
        Ok(self.0)
    }

    fn reconcile_self(self, state: &mut PanelSpecData, _ctx: &mut (), output: &mut Sender<PanelCommand>) -> Result<(), anyhow::Error> {
        let resized = state.width != self.0.width || state.height != self.0.height;
        let moved = state.x != self.0.x
            || state.y != self.0.y
            || state.anchor != self.0.anchor
            || state.output != self.0.output
            || state.outer_gap != self.0.outer_gap;
        if resized {
            let _ = output.send(PanelCommand::Resize(self.0.clone()));
        }
        if moved {
            let _ = output.send(PanelCommand::Move(self.0.clone()));
        }
        *state = self.0;
        Ok(())
    }

    fn exit(state: PanelSpecData, _ctx: &mut (), output: &mut Sender<PanelCommand>) -> Result<(), anyhow::Error> {
        let _ = output.send(PanelCommand::Delete { id: state.id });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::layout::PanelSpecData;
    use crate::managed_set::Lifecycle;
    use crate::presentation::PanelCommand;
    use super::PanelSpec;

    fn make_spec_data(id: &str) -> PanelSpecData {
        PanelSpecData {
            id: id.to_string(),
            anchor: None,
            width: 100,
            height: 30,
            x: 0,
            y: 0,
            outer_gap: 0,
            output: None,
            above: false,
            content: serde_json::Value::Null,
        }
    }

    #[test]
    fn panel_spec_enter_emits_create_command_and_returns_state() {
        let (mut tx, rx) = std::sync::mpsc::channel::<PanelCommand>();
        let spec = PanelSpec(make_spec_data("p1"));
        let state = <PanelSpec as Lifecycle>::enter(spec, &mut (), &mut tx).expect("enter should succeed");
        assert_eq!(state.id, "p1", "enter returns the spec data as state");
        let cmds: Vec<PanelCommand> = rx.try_iter().collect();
        assert!(matches!(cmds.as_slice(), [PanelCommand::Create(s)] if s.id == "p1"),
            "enter must emit exactly one Create command; got {} commands", cmds.len());
    }

    #[test]
    fn panel_spec_reconcile_self_emits_nothing_when_unchanged() {
        let (mut tx, rx) = std::sync::mpsc::channel::<PanelCommand>();
        let mut state = make_spec_data("p1");
        let spec = PanelSpec(make_spec_data("p1"));
        <PanelSpec as Lifecycle>::reconcile_self(spec, &mut state, &mut (), &mut tx).unwrap();
        let cmds: Vec<PanelCommand> = rx.try_iter().collect();
        assert!(cmds.is_empty(), "reconcile_self must emit no commands when nothing changed; got {}", cmds.len());
    }

    #[test]
    fn panel_spec_reconcile_self_emits_resize_when_dimensions_change() {
        let (mut tx, rx) = std::sync::mpsc::channel::<PanelCommand>();
        let mut state = make_spec_data("p1");
        let mut next = make_spec_data("p1");
        next.width = 200;
        let spec = PanelSpec(next);
        <PanelSpec as Lifecycle>::reconcile_self(spec, &mut state, &mut (), &mut tx).unwrap();
        let cmds: Vec<PanelCommand> = rx.try_iter().collect();
        assert!(cmds.iter().any(|c| matches!(c, PanelCommand::Resize(s) if s.id == "p1")),
            "reconcile_self must emit Resize when dimensions change");
        assert!(!cmds.iter().any(|c| matches!(c, PanelCommand::Move(_))),
            "reconcile_self must NOT emit Move when only dimensions change");
    }

    #[test]
    fn panel_spec_reconcile_self_emits_move_when_position_changes() {
        let (mut tx, rx) = std::sync::mpsc::channel::<PanelCommand>();
        let mut state = make_spec_data("p1");
        let mut next = make_spec_data("p1");
        next.x = 50;
        let spec = PanelSpec(next);
        <PanelSpec as Lifecycle>::reconcile_self(spec, &mut state, &mut (), &mut tx).unwrap();
        let cmds: Vec<PanelCommand> = rx.try_iter().collect();
        assert!(cmds.iter().any(|c| matches!(c, PanelCommand::Move(s) if s.id == "p1")),
            "reconcile_self must emit Move when position changes");
        assert!(!cmds.iter().any(|c| matches!(c, PanelCommand::Resize(_))),
            "reconcile_self must NOT emit Resize when only position changes");
    }

    #[test]
    fn panel_spec_reconcile_self_emits_nothing_when_only_content_changes() {
        let (mut tx, rx) = std::sync::mpsc::channel::<PanelCommand>();
        let mut state = make_spec_data("p1");
        let mut next = make_spec_data("p1");
        next.content = serde_json::json!({"type": "text", "text": "hello"});
        let spec = PanelSpec(next);
        <PanelSpec as Lifecycle>::reconcile_self(spec, &mut state, &mut (), &mut tx).unwrap();
        let cmds: Vec<PanelCommand> = rx.try_iter().collect();
        assert!(cmds.is_empty(),
            "reconcile_self must emit no commands on content-only change — pipeline re-renders all panels after every reconcile");
    }

    #[test]
    fn panel_spec_exit_emits_delete_with_id() {
        let (mut tx, rx) = std::sync::mpsc::channel::<PanelCommand>();
        let state = make_spec_data("p1");
        <PanelSpec as Lifecycle>::exit(state, &mut (), &mut tx).unwrap();
        let cmds: Vec<PanelCommand> = rx.try_iter().collect();
        assert!(matches!(cmds.as_slice(), [PanelCommand::Delete { id }] if id == "p1"),
            "exit must emit exactly one Delete command carrying the id");
    }
}

impl X11PanelContext {
    pub fn test_stub() -> Self {
        use std::collections::HashMap;
        let (conn, screen_num) = x11rb::rust_connection::RustConnection::connect(None)
            .expect("X11PanelContext::test_stub requires an X11 display");
        let screen = conn.setup().roots[screen_num].clone();
        use x11rb::connection::Connection as _;
        use x11rb::protocol::xproto::ConnectionExt as XprotoConnExt;
        let strut_atom = XprotoConnExt::intern_atom(&conn, false, b"_NET_WM_STRUT_PARTIAL")
            .expect("intern_atom").reply().expect("intern_atom reply").atom;
        let strut_legacy_atom = XprotoConnExt::intern_atom(&conn, false, b"_NET_WM_STRUT")
            .expect("intern_atom").reply().expect("intern_atom reply").atom;
        X11PanelContext {
            conn: std::sync::Arc::new(conn),
            root: screen.root,
            depth: screen.root_depth,
            root_visual: screen.root_visual,
            black_pixel: screen.black_pixel,
            dpr: 1.0,
            mon_x: 0,
            mon_y: 0,
            mon_width: screen.width_in_pixels as u32,
            mon_height: screen.height_in_pixels as u32,
            xrootpmap_atom: None,
            strut_atom,
            strut_legacy_atom,
            output_map: std::sync::Arc::new(HashMap::new()),
            dpi: 96.0,
            output_name: String::new(),
            screen_width_logical: 1920,
            screen_height_logical: 1080,
        }
    }
}
