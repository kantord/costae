use std::os::fd::{AsFd, AsRawFd, RawFd};
use std::sync::Arc;

use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor,
    delegate_layer,
    delegate_output,
    delegate_registry,
    delegate_seat,
    delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{Capability, SeatHandler, SeatState},
    shell::{
        WaylandSurface,
        wlr_layer::{Anchor, Layer, LayerShell, LayerShellHandler, LayerSurface, LayerSurfaceConfigure},
    },
    shm::{Shm, ShmHandler, slot::SlotPool},
};
use wayland_client::{
    backend::ObjectId,
    globals::registry_queue_init,
    protocol::{wl_output, wl_seat, wl_shm, wl_surface},
    Connection, EventQueue, Proxy, QueueHandle,
};

use crate::layout::{PanelAnchor, PanelSpecData};
use super::{DispatchError, DisplayServer, WindowEvent};

// ---------------------------------------------------------------------------
// Public error type
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum WaylandConnectError {
    #[error("failed to connect to Wayland display: {0}")]
    Connect(String),
    #[error("failed to bind compositor: {0}")]
    BindCompositor(String),
    #[error("failed to bind shm: {0}")]
    BindShm(String),
    #[error("failed to bind layer shell: {0}")]
    BindLayerShell(String),
}

// ---------------------------------------------------------------------------
// Wayland panel: owns a layer surface and its SHM pool
// ---------------------------------------------------------------------------

pub struct WaylandPanel {
    pub layer_surface: LayerSurface,
    pool: SlotPool,
    pub surface_id: ObjectId,
    pub configured: bool,
    pub width: u32,
    pub height: u32,
    pub anchor: Option<PanelAnchor>,
    pub raw_layout: Option<serde_json::Value>,
}

impl WaylandPanel {
    /// Update the panel from a new spec. Resizes the layer surface if the fixed dimension changed,
    /// which will cause the compositor to send a new configure before the next render.
    pub fn update_spec(&mut self, data: &PanelSpecData) {
        self.raw_layout = Some(data.content.clone());
        // Only the fixed axis needs to be re-set; the span axis stays 0 (compositor-controlled).
        let fixed_changed = match &self.anchor {
            Some(PanelAnchor::Left) | Some(PanelAnchor::Right) => data.width != self.width,
            Some(PanelAnchor::Top) | Some(PanelAnchor::Bottom) => data.height != self.height,
            None => data.width != self.width || data.height != self.height,
        };
        if fixed_changed {
            self.width = data.width;
            self.height = data.height;
            let (set_w, set_h) = match &self.anchor {
                Some(PanelAnchor::Left) | Some(PanelAnchor::Right) => (data.width, 0),
                Some(PanelAnchor::Top) | Some(PanelAnchor::Bottom) => (0, data.height),
                None => (data.width, data.height),
            };
            self.layer_surface.set_size(set_w, set_h);
            self.layer_surface.wl_surface().commit();
            self.configured = false;
        }
    }

    /// Paint a BGRX frame onto this panel's layer surface.
    pub fn render(&mut self, bgrx: &[u8]) {
        let stride = self.width as i32 * 4;
        let Ok((buffer, canvas)) = self.pool.create_buffer(
            self.width as i32,
            self.height as i32,
            stride,
            wl_shm::Format::Xrgb8888,
        ) else {
            tracing::error!("failed to create Wayland SHM buffer");
            return;
        };
        // SlotPool rounds slot size up to 64 bytes; copy only the actual pixel data.
        canvas[..bgrx.len()].copy_from_slice(bgrx);
        let wl_surf = self.layer_surface.wl_surface();
        if buffer.attach_to(wl_surf).is_err() {
            tracing::error!("failed to attach buffer to surface");
            return;
        }
        wl_surf.damage_buffer(0, 0, self.width as i32, self.height as i32);
        wl_surf.commit();
    }
}

// ---------------------------------------------------------------------------
// Internal sctk dispatch state
// ---------------------------------------------------------------------------

#[allow(dead_code)]
pub(crate) struct WaylandState {
    pub(crate) registry_state: RegistryState,
    pub(crate) compositor_state: CompositorState,
    pub(crate) output_state: OutputState,
    pub(crate) shm: Shm,
    pub(crate) layer_shell: LayerShell,
    pub(crate) seat_state: SeatState,
    pub(crate) pending_events: Vec<WindowEvent>,
    /// (surface_id, new_size) pairs from configure events received since last take.
    /// new_size of (0, 0) means "use your set_size value".
    pub(crate) pending_configures: Vec<(ObjectId, (u32, u32))>,
}

// ---------------------------------------------------------------------------
// Public struct
// ---------------------------------------------------------------------------

#[allow(dead_code)]
pub struct WaylandDisplayServer {
    conn: Arc<Connection>,
    event_queue: EventQueue<WaylandState>,
    state: WaylandState,
}

impl std::fmt::Debug for WaylandDisplayServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WaylandDisplayServer").finish_non_exhaustive()
    }
}

impl WaylandDisplayServer {
    pub fn connect() -> Result<Self, WaylandConnectError> {
        let conn = Connection::connect_to_env()
            .map_err(|e| WaylandConnectError::Connect(e.to_string()))?;
        let conn = Arc::new(conn);

        let (globals, event_queue) = registry_queue_init::<WaylandState>(&conn)
            .map_err(|e| WaylandConnectError::Connect(e.to_string()))?;

        let qh = event_queue.handle();

        let compositor_state = CompositorState::bind(&globals, &qh)
            .map_err(|e| WaylandConnectError::BindCompositor(e.to_string()))?;
        let output_state = OutputState::new(&globals, &qh);
        let shm = Shm::bind(&globals, &qh)
            .map_err(|e| WaylandConnectError::BindShm(e.to_string()))?;
        let layer_shell = LayerShell::bind(&globals, &qh)
            .map_err(|e| WaylandConnectError::BindLayerShell(e.to_string()))?;
        let seat_state = SeatState::new(&globals, &qh);

        let state = WaylandState {
            registry_state: RegistryState::new(&globals),
            compositor_state,
            output_state,
            shm,
            layer_shell,
            seat_state,
            pending_events: Vec::new(),
            pending_configures: Vec::new(),
        };

        let mut server = Self { conn, event_queue, state };
        // Roundtrip so output geometry events are processed before the caller
        // queries output dimensions.
        server.event_queue.roundtrip(&mut server.state)
            .map_err(|e| WaylandConnectError::Connect(e.to_string()))?;
        Ok(server)
    }

    /// Returns the logical size of the first known output, falling back to the
    /// current physical mode if xdg-output logical size is unavailable.
    pub fn primary_output_size(&self) -> Option<(u32, u32)> {
        let output = self.state.output_state.outputs().next()?;
        let info = self.state.output_state.info(&output)?;
        if let Some((w, h)) = info.logical_size {
            if w > 0 && h > 0 {
                return Some((w as u32, h as u32));
            }
        }
        info.modes.iter()
            .find(|m| m.current)
            .map(|m| (m.dimensions.0 as u32, m.dimensions.1 as u32))
    }

    /// Create a Wayland layer-shell panel for the given spec.
    /// The surface won't render until the compositor sends a configure and
    /// `WaylandPanel::render` is called with pixel data.
    pub fn create_panel(&mut self, data: &PanelSpecData) -> Result<WaylandPanel, anyhow::Error> {
        let qh = self.event_queue.handle();
        let wl_surface = self.state.compositor_state.create_surface(&qh);

        let anchor = anchor_for_panel(data.anchor.as_ref());
        let layer = if data.above { Layer::Top } else { Layer::Bottom };

        let layer_surface = self.state.layer_shell.create_layer_surface(
            &qh,
            wl_surface,
            layer,
            Some("costae"),
            None,
        );

        // For panels that span the full perpendicular axis (composite anchor), set the spanned
        // dimension to 0 so the compositor fills it. The actual dimension arrives in the configure.
        let (set_w, set_h) = match data.anchor {
            Some(PanelAnchor::Left) | Some(PanelAnchor::Right) => (data.width, 0),
            Some(PanelAnchor::Top) | Some(PanelAnchor::Bottom) => (0, data.height),
            None => (data.width, data.height),
        };
        layer_surface.set_size(set_w, set_h);
        if !anchor.is_empty() {
            layer_surface.set_anchor(anchor);
            let exclusive_zone = match data.anchor {
                Some(PanelAnchor::Left) | Some(PanelAnchor::Right) => data.width as i32,
                Some(PanelAnchor::Top) | Some(PanelAnchor::Bottom) => data.height as i32,
                None => 0,
            };
            if exclusive_zone > 0 {
                layer_surface.set_exclusive_zone(exclusive_zone);
            }
        }
        layer_surface.wl_surface().commit();

        let surface_id = layer_surface.wl_surface().id();

        // 3× frame size: supports one buffer in-flight with the compositor + one being prepared.
        let pool_size = (data.width as usize) * (data.height as usize) * 4 * 3;
        let pool = SlotPool::new(pool_size.max(4096 * 3), &self.state.shm)
            .map_err(|e| anyhow::anyhow!("SlotPool::new: {e}"))?;

        self.conn.flush().map_err(|e| anyhow::anyhow!("flush after create_panel: {e}"))?;

        Ok(WaylandPanel {
            layer_surface,
            pool,
            surface_id,
            configured: false,
            width: data.width,
            height: data.height,
            anchor: data.anchor.clone(),
            raw_layout: Some(data.content.clone()),
        })
    }

    /// Drain and return (surface_id, new_size) pairs from configure events since the last call.
    /// A new_size of (0, 0) means the compositor accepted the set_size value as-is.
    pub fn take_pending_configures(&mut self) -> Vec<(ObjectId, (u32, u32))> {
        std::mem::take(&mut self.state.pending_configures)
    }

    pub fn flush(&self) {
        let _ = self.conn.flush();
    }
}

// ---------------------------------------------------------------------------
// Pure helper — testable without a live Wayland connection
// ---------------------------------------------------------------------------

pub fn build_dispatch_result(
    dispatch_ok: bool,
    flush_ok: bool,
    pending: Vec<WindowEvent>,
) -> Result<Vec<WindowEvent>, DispatchError> {
    if !dispatch_ok || !flush_ok {
        return Err(DispatchError::ConnectionLost);
    }
    Ok(pending)
}

fn anchor_for_panel(anchor: Option<&PanelAnchor>) -> Anchor {
    // Use composite anchors so the compositor stretches the panel across the full perpendicular
    // axis (layer-shell spec: anchoring both opposite edges makes the surface span between them).
    match anchor {
        Some(PanelAnchor::Left)   => Anchor::LEFT   | Anchor::TOP | Anchor::BOTTOM,
        Some(PanelAnchor::Right)  => Anchor::RIGHT  | Anchor::TOP | Anchor::BOTTOM,
        Some(PanelAnchor::Top)    => Anchor::TOP    | Anchor::LEFT | Anchor::RIGHT,
        Some(PanelAnchor::Bottom) => Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT,
        None => Anchor::empty(),
    }
}

// ---------------------------------------------------------------------------
// DisplayServer impl
// ---------------------------------------------------------------------------

impl DisplayServer for WaylandDisplayServer {
    fn as_raw_fd(&self) -> RawFd {
        self.conn.as_fd().as_raw_fd()
    }

    fn dispatch(&mut self) -> Result<Vec<WindowEvent>, DispatchError> {
        // Flush outgoing requests, then do a non-blocking read of any incoming events.
        // dispatch_pending alone only processes already-buffered events; without the read
        // step configure/close events from the compositor are never received.
        let _ = self.event_queue.flush();
        if let Some(guard) = self.event_queue.prepare_read() {
            let _ = guard.read();
        }
        let dispatch_ok = self.event_queue.dispatch_pending(&mut self.state).is_ok();
        let flush_ok = self.event_queue.flush().is_ok();
        build_dispatch_result(dispatch_ok, flush_ok, std::mem::take(&mut self.state.pending_events))
    }
}

// ---------------------------------------------------------------------------
// sctk handler implementations
// ---------------------------------------------------------------------------

impl CompositorHandler for WaylandState {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for WaylandState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
        self.pending_events.push(WindowEvent::OutputsChanged);
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
        self.pending_events.push(WindowEvent::OutputsChanged);
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
        self.pending_events.push(WindowEvent::OutputsChanged);
    }
}

impl LayerShellHandler for WaylandState {
    fn closed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _layer: &LayerSurface,
    ) {
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        // ack_configure is called automatically by delegate_layer!
        self.pending_configures.push((layer.wl_surface().id(), configure.new_size));
    }
}

impl SeatHandler for WaylandState {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: wl_seat::WlSeat,
    ) {
    }

    fn new_capability(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: wl_seat::WlSeat,
        _capability: Capability,
    ) {
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: wl_seat::WlSeat,
        _capability: Capability,
    ) {
    }

    fn remove_seat(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: wl_seat::WlSeat,
    ) {
    }
}

impl ShmHandler for WaylandState {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for WaylandState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    registry_handlers!(OutputState, SeatState);
}

delegate_compositor!(WaylandState);
delegate_output!(WaylandState);
delegate_layer!(WaylandState);
delegate_seat!(WaylandState);
delegate_shm!(WaylandState);
delegate_registry!(WaylandState);
