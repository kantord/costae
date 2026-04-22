use crate::display_manager::DisplayManager;
use crate::layout::PanelSpecData;
use crate::managed_set::Lifecycle;
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

impl<DM: DisplayManager> Lifecycle for PanelSpec<DM>
where
    DM::Error: std::fmt::Display,
{
    type Key = String;
    type State = DM::Panel;
    type Context = DM;
    type Output = ();
    type Error = anyhow::Error;

    fn key(&self) -> String {
        self.0.id.clone()
    }

    fn enter(self, ctx: &mut DM, _output: &mut ()) -> Result<DM::Panel, anyhow::Error> {
        ctx.create_window(&self.0).map_err(|e| anyhow::anyhow!("{e}"))
    }

    fn reconcile_self(self, state: &mut DM::Panel, ctx: &mut DM, _output: &mut ()) -> Result<(), anyhow::Error> {
        ctx.update_dimensions(state, &self.0).map_err(|e| anyhow::anyhow!("{e}"))?;
        ctx.update_position(state, &self.0).map_err(|e| anyhow::anyhow!("{e}"))
    }

    fn exit(state: DM::Panel, ctx: &mut DM) -> Result<(), anyhow::Error> {
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
    //          and returns the panel produced by the DM.
    // -----------------------------------------------------------------------
    #[test]
    fn panel_spec_enter_calls_create_window_and_returns_panel() {
        let mut dm = MockDM::new();
        let spec: PanelSpec<MockDM> = PanelSpec(make_spec_data("p1"), std::marker::PhantomData);

        let panel = <PanelSpec<MockDM> as Lifecycle>::enter(spec, &mut dm, &mut ())
            .expect("enter should succeed");

        assert!(dm.calls.contains(&"create_window"), "enter must call create_window; got: {:?}", dm.calls);
        assert_eq!(panel, 1u32, "enter must return the panel produced by create_window");
    }

    // -----------------------------------------------------------------------
    // Claim 2: PanelSpec::reconcile_self calls ctx.update_dimensions AND
    //          ctx.update_position (both must appear in the call log).
    // -----------------------------------------------------------------------
    #[test]
    fn panel_spec_reconcile_self_calls_update_dimensions_and_update_position() {
        let mut dm = MockDM::new();
        let spec: PanelSpec<MockDM> = PanelSpec(make_spec_data("p2"), std::marker::PhantomData);
        let mut panel: u32 = 42;

        <PanelSpec<MockDM> as Lifecycle>::reconcile_self(spec, &mut panel, &mut dm, &mut ())
            .expect("reconcile_self should succeed");

        assert!(dm.calls.contains(&"update_dimensions"), "reconcile_self must call update_dimensions; got: {:?}", dm.calls);
        assert!(dm.calls.contains(&"update_position"), "reconcile_self must call update_position; got: {:?}", dm.calls);
    }

    // -----------------------------------------------------------------------
    // Claim 3: PanelSpec::exit calls ctx.delete_window(panel).
    // -----------------------------------------------------------------------
    #[test]
    fn panel_spec_exit_calls_delete_window() {
        let mut dm = MockDM::new();
        let panel: u32 = 7;

        <PanelSpec<MockDM> as Lifecycle>::exit(panel, &mut dm)
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
