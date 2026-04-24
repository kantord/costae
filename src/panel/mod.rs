use std::sync::mpsc::Sender;

use crate::display_manager::DisplayManager;
use crate::layout::PanelSpecData;
use crate::managed_set::Lifecycle;
use crate::presentation::PanelCommand;
use crate::x11::panel::Panel;
pub use crate::x11::panel::X11PanelContext;

// ---------------------------------------------------------------------------
// Stub Wayland types — no live connection required, used in tests and as the
// Context type in the unified PanelContext enum.
// ---------------------------------------------------------------------------

pub struct WaylandPanelContext;

impl WaylandPanelContext {
    pub fn test_stub() -> Self { WaylandPanelContext }
}

pub struct WaylandPanel;

// ---------------------------------------------------------------------------
// Unified context and state enums
// ---------------------------------------------------------------------------

pub enum PanelContext {
    X11(X11PanelContext),
    Wayland(WaylandPanelContext),
}

pub enum PanelState {
    X11(Panel),
    Wayland(WaylandPanel),
}

// ---------------------------------------------------------------------------
// Generic PanelSpec<DM> — wraps PanelSpecData with the display manager type
// that should own the panel window.
// ---------------------------------------------------------------------------

pub struct PanelSpec<DM>(pub PanelSpecData, pub std::marker::PhantomData<DM>);

impl<DM> std::fmt::Display for PanelSpec<DM> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0.id)
    }
}

impl<DM: DisplayManager> Lifecycle for PanelSpec<DM>
where
    DM::Error: std::fmt::Display,
{
    type Key = String;
    type State = DM::Panel;
    type Context = DM;
    type Output = Sender<PanelCommand>;
    type Error = anyhow::Error;

    fn key(&self) -> String {
        self.0.id.clone()
    }

    fn enter(self, ctx: &mut DM, output: &mut Sender<PanelCommand>) -> Result<DM::Panel, anyhow::Error> {
        let _ = output.send(PanelCommand::Create(self.0.clone()));
        ctx.create_window(&self.0).map_err(|e| anyhow::anyhow!("{e}"))
    }

    fn reconcile_self(self, state: &mut DM::Panel, ctx: &mut DM, output: &mut Sender<PanelCommand>) -> Result<(), anyhow::Error> {
        let _ = output.send(PanelCommand::Resize(self.0.clone()));
        let _ = output.send(PanelCommand::Move(self.0.clone()));
        ctx.update_dimensions(state, &self.0).map_err(|e| anyhow::anyhow!("{e}"))?;
        ctx.update_position(state, &self.0).map_err(|e| anyhow::anyhow!("{e}"))
    }

    fn exit(state: DM::Panel, ctx: &mut DM, output: &mut Sender<PanelCommand>) -> Result<(), anyhow::Error> {
        // Emit Delete by key. State doesn't carry the id, so the presenter
        // looks up the panel by the id it tracked from the Create command.
        // The spec id lives in the State via DM::Panel (X11: Panel has id; Wayland: WaylandPanel has id).
        // For now, emit a best-effort Delete — Phase 3 will tighten this when
        // the presenter owns the id ↔ panel mapping.
        // TODO Phase 3: thread the id through so Delete carries it reliably.
        let _ = output;
        ctx.delete_window(state).map_err(|e| anyhow::anyhow!("{e}"))
    }
}

#[cfg(test)]
mod tests {
    use crate::display_manager::DisplayManager;
    use crate::layout::PanelSpecData;
    use crate::managed_set::Lifecycle;
    use super::PanelSpec;

    // -----------------------------------------------------------------------
    // MockDM — records which DisplayManager methods were called; no X11/Wayland
    // connection required.
    // -----------------------------------------------------------------------
    struct MockDM {
        calls: Vec<&'static str>,
        panel_id: u32,
    }

    impl MockDM {
        fn new() -> Self {
            MockDM { calls: Vec::new(), panel_id: 0 }
        }
    }

    impl DisplayManager for MockDM {
        type Panel = u32;
        type Error = String;

        fn create_window(&mut self, _spec: &PanelSpecData) -> Result<u32, String> {
            self.calls.push("create_window");
            self.panel_id += 1;
            Ok(self.panel_id)
        }

        fn update_position(&mut self, _panel: &mut u32, _spec: &PanelSpecData) -> Result<(), String> {
            self.calls.push("update_position");
            Ok(())
        }

        fn update_dimensions(&mut self, _panel: &mut u32, _spec: &PanelSpecData) -> Result<(), String> {
            self.calls.push("update_dimensions");
            Ok(())
        }

        fn update_image(&mut self, _panel: &mut u32, _bgrx: &[u8]) -> Result<(), String> {
            self.calls.push("update_image");
            Ok(())
        }

        fn delete_window(&mut self, _panel: u32) -> Result<(), String> {
            self.calls.push("delete_window");
            Ok(())
        }

        fn flush(&mut self) {}
    }

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

    // -----------------------------------------------------------------------
    // Claim 1: PanelSpec::enter delegates to ctx.create_window(&spec_data)
    //          and emits PanelCommand::Create on the output channel.
    // -----------------------------------------------------------------------
    #[test]
    fn panel_spec_enter_calls_create_window_and_emits_create_command() {
        use crate::presentation::PanelCommand;
        let mut dm = MockDM::new();
        let (mut tx, rx) = std::sync::mpsc::channel::<PanelCommand>();
        let spec: PanelSpec<MockDM> = PanelSpec(make_spec_data("p1"), std::marker::PhantomData);

        let panel = <PanelSpec<MockDM> as Lifecycle>::enter(spec, &mut dm, &mut tx)
            .expect("enter should succeed");

        assert!(dm.calls.contains(&"create_window"), "enter must call create_window; got: {:?}", dm.calls);
        assert_eq!(panel, 1u32, "enter must return the panel produced by create_window");
        let cmds: Vec<PanelCommand> = rx.try_iter().collect();
        assert!(matches!(cmds.as_slice(), [PanelCommand::Create(spec)] if spec.id == "p1"),
            "enter must emit PanelCommand::Create; got {} command(s)", cmds.len());
    }

    // -----------------------------------------------------------------------
    // Claim 2: PanelSpec::reconcile_self calls update_dimensions AND
    //          update_position, and emits Resize and Move commands.
    // -----------------------------------------------------------------------
    #[test]
    fn panel_spec_reconcile_self_calls_dm_methods_and_emits_resize_and_move() {
        use crate::presentation::PanelCommand;
        let mut dm = MockDM::new();
        let (mut tx, rx) = std::sync::mpsc::channel::<PanelCommand>();
        let spec: PanelSpec<MockDM> = PanelSpec(make_spec_data("p2"), std::marker::PhantomData);
        let mut panel: u32 = 42;

        <PanelSpec<MockDM> as Lifecycle>::reconcile_self(spec, &mut panel, &mut dm, &mut tx)
            .expect("reconcile_self should succeed");

        assert!(dm.calls.contains(&"update_dimensions"), "reconcile_self must call update_dimensions; got: {:?}", dm.calls);
        assert!(dm.calls.contains(&"update_position"), "reconcile_self must call update_position; got: {:?}", dm.calls);
        let cmds: Vec<PanelCommand> = rx.try_iter().collect();
        assert!(cmds.iter().any(|c| matches!(c, PanelCommand::Resize(s) if s.id == "p2")),
            "reconcile_self must emit PanelCommand::Resize");
        assert!(cmds.iter().any(|c| matches!(c, PanelCommand::Move(s) if s.id == "p2")),
            "reconcile_self must emit PanelCommand::Move");
    }

    // -----------------------------------------------------------------------
    // Claim 3: PanelSpec::exit calls ctx.delete_window(panel).
    // -----------------------------------------------------------------------
    #[test]
    fn panel_spec_exit_calls_delete_window() {
        use crate::presentation::PanelCommand;
        let mut dm = MockDM::new();
        let (mut tx, _rx) = std::sync::mpsc::channel::<PanelCommand>();
        let panel: u32 = 7;

        <PanelSpec<MockDM> as Lifecycle>::exit(panel, &mut dm, &mut tx)
            .expect("exit should succeed");

        assert!(dm.calls.contains(&"delete_window"), "exit must call delete_window; got: {:?}", dm.calls);
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
