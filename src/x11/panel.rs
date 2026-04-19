use std::collections::HashMap;
use std::sync::Arc;

use x11rb::{
    connection::Connection,
    protocol::xproto::*,
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};

use crate::layout::{PanelSpecData, PanelAnchor};

const XRESOURCES_PROP_MAX_LEN: u32 = 65536;
const MM_PER_INCH: f32 = 25.4;
const FALLBACK_DPI: f32 = 96.0;
use crate::managed_set::Lifecycle;
use crate::render::{RenderCache, render_frame, preload_layout_images, init_global_ctx};
use crate::x11::{x11_bgrx_to_rgba, inject_root_bg, solid_color_rgba, strut_partial_values_for_anchor};

/// A live X11 panel window, created from a `PanelSpec` at runtime.
pub struct Panel {
    pub id: String,
    pub win_id: u32,
    pub gc: u32,
    pub win_x: i16,
    pub win_y: i16,
    pub phys_width: u32,
    pub phys_height: u32,
    /// Per-panel wallpaper snapshot (RGBA). Re-injected as "root-bg" before each render
    /// so every panel sees the correct region of the wallpaper behind it.
    pub root_bg_rgba: Vec<u8>,
    pub raw_layout: Option<serde_json::Value>,
    pub render_cache: RenderCache,
    pub bgrx: Arc<Vec<u8>>,
}

pub fn sample_root_bg(
    conn: &RustConnection,
    root: Window,
    win_x: i16,
    win_y: i16,
    width: u32,
    height: u32,
    xrootpmap_atom: Option<u32>,
) -> Option<Vec<u8>> {
    let t = std::time::Instant::now();

    // Tier 1: _XROOTPMAP_ID pixmap (set by feh/nitrogen for wallpapers).
    if let Some(atom) = xrootpmap_atom {
        let pixmap = conn
            .get_property(false, root, atom, AtomEnum::ANY, 0, 1).ok()
            .and_then(|c| c.reply().ok())
            .filter(|p| p.value.len() >= 4)
            .and_then(|p| p.value[..4].try_into().ok().map(u32::from_ne_bytes));
        if let Some(pixmap_id) = pixmap {
            if let Some(img) = conn.get_image(ImageFormat::Z_PIXMAP, pixmap_id, win_x, win_y, width as u16, height as u16, !0)
                .ok().and_then(|c| c.reply().ok())
            {
                tracing::debug!(elapsed_ms = t.elapsed().as_millis(), "root bg from _XROOTPMAP_ID pixmap");
                return Some(x11_bgrx_to_rgba(&img.data));
            }
        }
    }

    // Tier 2: solid color from the pixel just to the right of the panel.
    if let Some(img) = conn.get_image(
        ImageFormat::Z_PIXMAP, root,
        win_x + width as i16, win_y,
        1, 1, !0,
    ).ok().and_then(|c| c.reply().ok()) {
        if img.data.len() >= 4 {
            let pixel = ((img.data[2] as u32) << 16)
                | ((img.data[1] as u32) << 8)
                | (img.data[0] as u32);
            tracing::debug!(pixel = format!("{:#06x}", pixel), elapsed_ms = t.elapsed().as_millis(), "root bg from adjacent pixel");
            return Some(solid_color_rgba(pixel, width, height));
        }
    }

    // Tier 3: GetImage on root window — last resort.
    match conn.get_image(ImageFormat::Z_PIXMAP, root, win_x, win_y, width as u16, height as u16, !0) {
        Err(e) => { tracing::warn!(error = ?e, "root bg send error"); None }
        Ok(cookie) => match cookie.reply() {
            Err(e) => { tracing::warn!(error = ?e, "root bg reply error"); None }
            Ok(img) => {
                tracing::debug!(elapsed_ms = t.elapsed().as_millis(), "root bg from root window (fallback)");
                Some(x11_bgrx_to_rgba(&img.data))
            }
        }
    }
}

pub fn i3_dpi(conn: &RustConnection, root: Window, screen: &Screen) -> f32 {
    let from_xresources = (|| -> Option<f32> {
        let atom = conn.intern_atom(false, b"RESOURCE_MANAGER").ok()?.reply().ok()?.atom;
        let prop = conn
            .get_property(false, root, atom, AtomEnum::ANY, 0, XRESOURCES_PROP_MAX_LEN)
            .ok()?
            .reply()
            .ok()?;
        let data = String::from_utf8_lossy(&prop.value).into_owned();
        for line in data.lines() {
            if let Some(val) = line.strip_prefix("Xft.dpi:") {
                return val.trim().parse::<f32>().ok();
            }
        }
        None
    })();
    if let Some(dpi) = from_xresources {
        tracing::info!(dpi, "DPI detected (from Xft.dpi)");
        return dpi;
    }
    if screen.height_in_millimeters > 0 {
        let dpi = screen.height_in_pixels as f32 * MM_PER_INCH / screen.height_in_millimeters as f32;
        tracing::info!(dpi, "DPI detected (from screen physical dimensions)");
        return dpi;
    }
    tracing::warn!("DPI fallback to {FALLBACK_DPI}");
    FALLBACK_DPI
}

fn create_panel(
    spec: &PanelSpecData,
    ctx: &PanelContext,
) -> anyhow::Result<Panel> {
    let phys_width = (spec.width as f32 * ctx.dpr).round() as u32;
    let phys_height = (spec.height as f32 * ctx.dpr).round() as u32;

    let (mon_x, mon_y, mon_width, mon_height) = spec.output.as_ref()
        .and_then(|name| ctx.output_map.get(name).copied())
        .unwrap_or((ctx.mon_x, ctx.mon_y, ctx.mon_width, ctx.mon_height));

    let (win_x, win_y) = match &spec.anchor {
        Some(PanelAnchor::Left) | Some(PanelAnchor::Top) => (mon_x, mon_y),
        Some(PanelAnchor::Right) => (mon_x + mon_width as i16 - phys_width as i16, mon_y),
        Some(PanelAnchor::Bottom) => (mon_x, mon_y + mon_height as i16 - phys_height as i16),
        None => (
            mon_x + (spec.x as f32 * ctx.dpr).round() as i16,
            mon_y + (spec.y as f32 * ctx.dpr).round() as i16,
        ),
    };

    let win_id = ctx.conn.generate_id()?;
    ctx.conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        win_id,
        ctx.root,
        win_x,
        win_y,
        phys_width as u16,
        phys_height as u16,
        0,
        WindowClass::INPUT_OUTPUT,
        ctx.root_visual,
        &CreateWindowAux::new()
            .background_pixel(ctx.black_pixel)
            .override_redirect(1)
            .event_mask(EventMask::EXPOSURE | EventMask::BUTTON_PRESS),
    )?;

    let root_bg_rgba = sample_root_bg(&ctx.conn, ctx.root, win_x, win_y, phys_width, phys_height, ctx.xrootpmap_atom)
        .unwrap_or_default();
    inject_root_bg(root_bg_rgba.clone(), phys_width, phys_height);

    ctx.conn.map_window(win_id)?;
    let stack_mode = if spec.above { StackMode::ABOVE } else { StackMode::BELOW };
    ctx.conn.configure_window(win_id, &ConfigureWindowAux::new().stack_mode(stack_mode))?;

    if let Some(anchor) = spec.anchor.clone() {
        let strut_vals = strut_partial_values_for_anchor(
            anchor, mon_x, mon_y, mon_width, mon_height, phys_width, phys_height,
        );
        ctx.conn.change_property32(PropMode::REPLACE, win_id, ctx.strut_atom, AtomEnum::CARDINAL, &strut_vals)?;
        ctx.conn.change_property32(PropMode::REPLACE, win_id, ctx.strut_legacy_atom, AtomEnum::CARDINAL, &strut_vals[..4])?;
    }

    let gc = ctx.conn.generate_id()?;
    ctx.conn.create_gc(gc, win_id, &CreateGCAux::new())?;

    ctx.conn.flush()?;

    let bgrx = Arc::new(render_frame(None, phys_width, phys_height, ctx.dpr));
    ctx.conn.put_image(ImageFormat::Z_PIXMAP, win_id, gc, phys_width as u16, phys_height as u16, 0, 0, 0, ctx.depth, &bgrx[..])?;
    ctx.conn.flush()?;

    Ok(Panel {
        id: spec.id.clone(),
        win_id,
        gc,
        win_x,
        win_y,
        phys_width,
        phys_height,
        root_bg_rgba,
        raw_layout: None,
        render_cache: RenderCache::new(30),
        bgrx,
    })
}

pub struct X11PanelContext {
    pub conn: Arc<RustConnection>,
    pub root: u32,
    pub depth: u8,
    pub root_visual: u32,
    pub black_pixel: u32,
    pub dpr: f32,
    pub mon_x: i16,
    pub mon_y: i16,
    pub mon_width: u32,
    pub mon_height: u32,
    pub xrootpmap_atom: Option<u32>,
    pub strut_atom: u32,
    pub strut_legacy_atom: u32,
    pub output_map: Arc<HashMap<String, (i16, i16, u32, u32)>>,
}

/// Backward-compatible alias so callers that import `x11::panel::PanelContext` still compile.
pub type PanelContext = X11PanelContext;

impl Lifecycle for PanelSpecData {
    type Key = String;
    type State = Panel;
    type Context = X11PanelContext;
    type Output = ();
    type Error = anyhow::Error;

    fn key(&self) -> String {
        self.id.clone()
    }

    fn enter(self, ctx: &Self::Context, _output: &()) -> Result<Self::State, Self::Error> {
        init_global_ctx();
        let mut panel = create_panel(&self, ctx).map_err(|e| {
            tracing::error!(panel = %self.id, error = %e, "panel create failed");
            e
        })?;
        tracing::info!(panel = %self.id, "panel created");
        if !self.content.is_null() {
            preload_layout_images(&self.content);
            panel.raw_layout = Some(self.content);
        }
        Ok(panel)
    }

    fn reconcile_self(self, state: &mut Self::State, _ctx: &Self::Context, _output: &()) -> Result<(), Self::Error> {
        if !self.content.is_null() {
            preload_layout_images(&self.content);
            state.raw_layout = Some(self.content);
            state.render_cache = RenderCache::new(30);
        }
        Ok(())
    }

    fn exit(state: Self::State, ctx: &Self::Context) -> Result<(), Self::Error> {
        tracing::info!(panel = %state.id, "panel destroyed");
        let _ = ctx.conn.free_gc(state.gc);
        let _ = ctx.conn.destroy_window(state.win_id);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Claim A: PanelContext struct shape check (compile-time)
// ---------------------------------------------------------------------------
// This function is never called at runtime; it exists only to assert that
// `PanelContext` has exactly the fields listed in the spec.  The test module
// below will fail to compile until `PanelContext` is defined with all fields.
#[cfg(test)]
#[allow(dead_code)]
fn _check_panel_context_fields(ctx: PanelContext) {
    let _ = ctx.conn;
    let _ = ctx.root;
    let _ = ctx.depth;
    let _ = ctx.root_visual;
    let _ = ctx.black_pixel;
    let _ = ctx.dpr;
    let _ = ctx.mon_x;
    let _ = ctx.mon_y;
    let _ = ctx.mon_width;
    let _ = ctx.mon_height;
    let _ = ctx.xrootpmap_atom;
    let _ = ctx.strut_atom;
    let _ = ctx.strut_legacy_atom;
    let _ = ctx.output_map;
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    // ---------------------------------------------------------------------------
    // X11 Lifecycle helpers (Claim A / B / C)
    // ---------------------------------------------------------------------------

    /// Build a minimal PanelContext by connecting to X11.
    /// Returns `None` if no display is available.
    fn make_panel_ctx() -> Option<super::PanelContext> {
        use x11rb::rust_connection::RustConnection;
        use x11rb::connection::Connection as _;
        use x11rb::protocol::xproto::ConnectionExt as XprotoConnExt;

        let (conn, screen_num) = RustConnection::connect(None).ok()?;
        let screen = conn.setup().roots[screen_num].clone();
        let depth = screen.root_depth;
        let root_visual = screen.root_visual;
        let black_pixel = screen.black_pixel;
        let root = screen.root;

        let strut_atom = XprotoConnExt::intern_atom(&conn, false, b"_NET_WM_STRUT_PARTIAL")
            .ok()?.reply().ok()?.atom;
        let strut_legacy_atom = XprotoConnExt::intern_atom(&conn, false, b"_NET_WM_STRUT")
            .ok()?.reply().ok()?.atom;
        let xrootpmap_atom = XprotoConnExt::intern_atom(&conn, false, b"_XROOTPMAP_ID").ok()
            .and_then(|c: x11rb::cookie::Cookie<'_, _, x11rb::protocol::xproto::InternAtomReply>| c.reply().ok())
            .map(|r| r.atom);

        // Use screen pixel dimensions as monitor size.
        let mon_width = screen.width_in_pixels as u32;
        let mon_height = screen.height_in_pixels as u32;

        Some(super::PanelContext {
            conn: Arc::new(conn),
            root,
            depth,
            root_visual,
            black_pixel,
            dpr: 1.0,
            mon_x: 0,
            mon_y: 0,
            mon_width,
            mon_height,
            xrootpmap_atom,
            strut_atom,
            strut_legacy_atom,
            output_map: Arc::new(HashMap::new()),
        })
    }

    /// Build a minimal PanelSpecData with the given id/dimensions.
    fn make_spec(id: &str, width: u32, height: u32) -> crate::layout::PanelSpecData {
        crate::layout::PanelSpecData {
            id: id.to_string(),
            anchor: None,
            width,
            height,
            x: 0,
            y: 0,
            outer_gap: 0,
            output: None,
            above: false,
            content: serde_json::Value::Null,
        }
    }

    // ---------------------------------------------------------------------------
    // Claim A: enter creates an X11 window (phys_width > 0 and phys_height > 0).
    // ---------------------------------------------------------------------------
    #[test]
    fn lifecycle_enter_creates_x11_window() {
        use crate::managed_set::Lifecycle;

        let ctx = match make_panel_ctx() {
            Some(c) => c,
            None => {
                println!("SKIP: no X11 display available");
                return;
            }
        };

        let spec = make_spec("test-enter", 200, 30);
        let panel = <crate::layout::PanelSpecData as Lifecycle>::enter(spec, &ctx, &())
            .expect("enter should succeed when X11 is available");

        assert!(panel.phys_width > 0, "phys_width must be > 0");
        assert!(panel.phys_height > 0, "phys_height must be > 0");

        // Cleanup
        let _ = <crate::layout::PanelSpecData as Lifecycle>::exit(panel, &ctx);
    }

    // ---------------------------------------------------------------------------
    // Claim B: exit destroys the X11 window (get_geometry returns an error).
    // ---------------------------------------------------------------------------
    #[test]
    fn lifecycle_exit_destroys_x11_window() {
        use crate::managed_set::Lifecycle;
        use x11rb::connection::Connection as _;
        use x11rb::protocol::xproto::ConnectionExt as XprotoExt;

        let ctx = match make_panel_ctx() {
            Some(c) => c,
            None => {
                println!("SKIP: no X11 display available");
                return;
            }
        };

        let spec = make_spec("test-exit", 200, 30);
        let panel = <crate::layout::PanelSpecData as Lifecycle>::enter(spec, &ctx, &())
            .expect("enter must succeed for exit test");

        let win_id = panel.win_id;

        // Sanity: window should exist before exit.
        ctx.conn.flush().ok();
        let before = XprotoExt::get_geometry(&*ctx.conn, win_id)
            .ok()
            .and_then(|c| c.reply().ok());
        assert!(before.is_some(), "window should exist before exit");

        let _ = <crate::layout::PanelSpecData as Lifecycle>::exit(panel, &ctx);
        ctx.conn.flush().ok();

        // After exit the window must no longer exist.
        let after = XprotoExt::get_geometry(&*ctx.conn, win_id)
            .ok()
            .and_then(|c| c.reply().ok());
        assert!(after.is_none(), "get_geometry should fail after exit (window destroyed)");
    }

    // ---------------------------------------------------------------------------
    // Claim C: update sets raw_layout when content changes.
    // ---------------------------------------------------------------------------
    #[test]
    fn lifecycle_update_sets_raw_layout_when_content_changes() {
        use crate::managed_set::Lifecycle;

        let ctx = match make_panel_ctx() {
            Some(c) => c,
            None => {
                println!("SKIP: no X11 display available");
                return;
            }
        };

        let spec = make_spec("test-update", 200, 30);
        let mut panel = <crate::layout::PanelSpecData as Lifecycle>::enter(spec, &ctx, &())
            .expect("enter must succeed for update test");

        let new_content = serde_json::json!({"type": "text", "text": "hello"});
        let new_spec = crate::layout::PanelSpecData {
            id: "test-update".to_string(),
            anchor: None,
            width: 200,
            height: 30,
            x: 0,
            y: 0,
            outer_gap: 0,
            output: None,
            above: false,
            content: new_content.clone(),
        };

        <crate::layout::PanelSpecData as Lifecycle>::reconcile_self(new_spec, &mut panel, &ctx, &())
            .expect("reconcile_self must succeed");

        assert_eq!(
            panel.raw_layout,
            Some(new_content),
            "raw_layout should be set to the new content after update"
        );

        // Cleanup
        let _ = <crate::layout::PanelSpecData as Lifecycle>::exit(panel, &ctx);
    }

    // ---------------------------------------------------------------------------
    // Claim A (compile-time): PanelContext has all required fields.
    // The helper function `_check_panel_context_fields` above the module will
    // cause a compile error until `PanelContext` is defined with every field.
    // This test is a placeholder that passes once the struct compiles.
    // ---------------------------------------------------------------------------
    #[test]
    fn panel_context_struct_fields_exist() {
        // Compile-time check: the free function `_check_panel_context_fields`
        // references every required field of PanelContext.  If any field is
        // missing the crate will not compile and this test will not run.
        // We just need one statement here so the test is not empty.
        let _ = std::marker::PhantomData::<super::PanelContext>::default;
    }

    // ---------------------------------------------------------------------------
    // Claim B: PanelSpec implements Lifecycle with Key = String and
    // fn key(&self) -> String returning self.id.clone().
    // ---------------------------------------------------------------------------
    #[test]
    fn panel_spec_lifecycle_key_returns_id() {
        use crate::layout::PanelSpecData;
        use crate::managed_set::Lifecycle;

        let spec = PanelSpecData {
            id: "my-panel".to_string(),
            anchor: None,
            width: 100,
            height: 30,
            x: 0,
            y: 0,
            outer_gap: 0,
            output: None,
            above: false,
            content: serde_json::Value::Null,
        };
        assert_eq!(spec.key(), "my-panel".to_string());
    }
}
