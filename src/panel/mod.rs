use crate::layout::{PanelSpec, PanelSpecData};
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
// Lifecycle impl for PanelSpec — Wayland variant is a no-op placeholder;
// real Wayland panels are managed directly by WaylandTickState in main.rs.
// ---------------------------------------------------------------------------

impl Lifecycle for PanelSpec {
    type Key = String;
    type State = PanelState;
    type Context = PanelContext;
    type Output = ();
    type Error = anyhow::Error;

    fn key(&self) -> String {
        match self {
            PanelSpec::X11(data) | PanelSpec::Wayland(data) => data.id.clone(),
        }
    }

    fn enter(self, ctx: &Self::Context, output: &mut ()) -> Result<Self::State, Self::Error> {
        match (self, ctx) {
            (PanelSpec::X11(data), PanelContext::X11(x11_ctx)) => {
                <PanelSpecData as Lifecycle>::enter(data, x11_ctx, output)
                    .map(PanelState::X11)
            }
            (PanelSpec::Wayland(_), PanelContext::Wayland(_)) => {
                Ok(PanelState::Wayland(WaylandPanel))
            }
            _ => Err(anyhow::anyhow!("context/spec mismatch")),
        }
    }

    fn reconcile_self(self, state: &mut Self::State, ctx: &Self::Context, output: &mut ()) -> Result<(), Self::Error> {
        match (self, state, ctx) {
            (PanelSpec::X11(data), PanelState::X11(panel), PanelContext::X11(x11_ctx)) => {
                <PanelSpecData as Lifecycle>::reconcile_self(data, panel, x11_ctx, output)
            }
            (PanelSpec::Wayland(_), PanelState::Wayland(_), PanelContext::Wayland(_)) => Ok(()),
            _ => Err(anyhow::anyhow!("context/spec/state mismatch during reconcile")),
        }
    }

    fn exit(state: Self::State, ctx: &Self::Context) -> Result<(), Self::Error> {
        match (state, ctx) {
            (PanelState::X11(panel), PanelContext::X11(x11_ctx)) => {
                <PanelSpecData as Lifecycle>::exit(panel, x11_ctx)
            }
            (PanelState::Wayland(_), PanelContext::Wayland(_)) => Ok(()),
            _ => Err(anyhow::anyhow!("context/state mismatch during exit")),
        }
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
        }
    }
}
