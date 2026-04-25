use std::collections::HashMap;
use std::sync::Arc;

use x11rb::{
    connection::Connection,
    protocol::xproto::*,
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};

use crate::layout::{PanelSpecData, PanelAnchor};
use crate::display_manager::DisplayManager;

const XRESOURCES_PROP_MAX_LEN: u32 = 65536;
const MM_PER_INCH: f32 = 25.4;
const FALLBACK_DPI: f32 = 96.0;
use crate::managed_set::Lifecycle;
use costae_lifecycle_derive::lifecycle_trace;
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
    pub dpi: f32,
    pub output_name: String,
    pub screen_width_logical: u32,
    pub screen_height_logical: u32,
}

/// Backward-compatible alias so callers that import `x11::panel::PanelContext` still compile.
pub type PanelContext = X11PanelContext;

#[lifecycle_trace]
impl Lifecycle for PanelSpecData {
    type Key = String;
    type State = Panel;
    type Context = X11PanelContext;
    type Output = ();
    type Error = anyhow::Error;

    fn key(&self) -> String {
        self.id.clone()
    }

    fn enter(self, ctx: &mut Self::Context, _output: &mut ()) -> Result<Self::State, Self::Error> {
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

    fn reconcile_self(self, state: &mut Self::State, ctx: &mut Self::Context, _output: &mut ()) -> Result<(), Self::Error> {
        if !self.content.is_null() {
            preload_layout_images(&self.content);
            state.raw_layout = Some(self.content.clone());
            state.render_cache = RenderCache::new(30);
        }

        let new_phys_width = (self.width as f32 * ctx.dpr).round() as u32;
        let new_phys_height = (self.height as f32 * ctx.dpr).round() as u32;

        if new_phys_width != state.phys_width || new_phys_height != state.phys_height {
            ctx.conn.configure_window(
                state.win_id,
                &ConfigureWindowAux::new()
                    .width(new_phys_width)
                    .height(new_phys_height),
            )?;
            state.phys_width = new_phys_width;
            state.phys_height = new_phys_height;

            // Recompute position using the same anchor logic as create_panel.
            let (mon_x, mon_y, mon_width, mon_height) = self.output.as_ref()
                .and_then(|name| ctx.output_map.get(name).copied())
                .unwrap_or((ctx.mon_x, ctx.mon_y, ctx.mon_width, ctx.mon_height));

            let (win_x, win_y) = match &self.anchor {
                Some(PanelAnchor::Left) | Some(PanelAnchor::Top) => (mon_x, mon_y),
                Some(PanelAnchor::Right) => (mon_x + mon_width as i16 - new_phys_width as i16, mon_y),
                Some(PanelAnchor::Bottom) => (mon_x, mon_y + mon_height as i16 - new_phys_height as i16),
                None => (
                    mon_x + (self.x as f32 * ctx.dpr).round() as i16,
                    mon_y + (self.y as f32 * ctx.dpr).round() as i16,
                ),
            };

            if win_x != state.win_x || win_y != state.win_y {
                ctx.conn.configure_window(
                    state.win_id,
                    &ConfigureWindowAux::new().x(win_x as i32).y(win_y as i32),
                )?;
                state.win_x = win_x;
                state.win_y = win_y;
            }

            // Re-set strut properties if anchor is Some.
            if let Some(anchor) = self.anchor.clone() {
                let strut_vals = strut_partial_values_for_anchor(
                    anchor, mon_x, mon_y, mon_width, mon_height, new_phys_width, new_phys_height,
                );
                ctx.conn.change_property32(PropMode::REPLACE, state.win_id, ctx.strut_atom, AtomEnum::CARDINAL, &strut_vals)?;
                ctx.conn.change_property32(PropMode::REPLACE, state.win_id, ctx.strut_legacy_atom, AtomEnum::CARDINAL, &strut_vals[..4])?;
            }

            ctx.conn.flush()?;
        }

        Ok(())
    }

    fn exit(state: Self::State, ctx: &mut Self::Context, _output: &mut Self::Output) -> Result<(), Self::Error> {
        tracing::info!(panel = %state.id, "panel destroyed");
        let _ = ctx.conn.free_gc(state.gc);
        let _ = ctx.conn.destroy_window(state.win_id);
        Ok(())
    }
}

impl DisplayManager for X11PanelContext {
    type Panel = Panel;

    fn create_window(&mut self, spec: &PanelSpecData) -> Result<Panel, anyhow::Error> {
        init_global_ctx();
        let mut panel = create_panel(spec, self)?;
        if !spec.content.is_null() {
            preload_layout_images(&spec.content);
            panel.raw_layout = Some(spec.content.clone());
        }
        Ok(panel)
    }

    fn delete_window(&mut self, panel: Panel) -> Result<(), anyhow::Error> {
        let _ = self.conn.free_gc(panel.gc);
        let _ = self.conn.destroy_window(panel.win_id);
        Ok(())
    }

    fn update_image(&mut self, panel: &mut Panel, bgrx: &[u8]) -> Result<(), anyhow::Error> {
        self.conn.put_image(
            ImageFormat::Z_PIXMAP,
            panel.win_id,
            panel.gc,
            panel.phys_width as u16,
            panel.phys_height as u16,
            0,
            0,
            0,
            self.depth,
            bgrx,
        ).map_err(|e| anyhow::anyhow!(e))?;
        self.conn.flush().map_err(|e| anyhow::anyhow!(e))?;
        Ok(())
    }

    fn update_position(&mut self, panel: &mut Panel, spec: &PanelSpecData) -> Result<(), anyhow::Error> {
        let (mon_x, mon_y, mon_width, mon_height) = spec.output.as_ref()
            .and_then(|name| self.output_map.get(name).copied())
            .unwrap_or((self.mon_x, self.mon_y, self.mon_width, self.mon_height));

        let (win_x, win_y) = match &spec.anchor {
            Some(PanelAnchor::Left) | Some(PanelAnchor::Top) => (mon_x, mon_y),
            Some(PanelAnchor::Right) => (mon_x + mon_width as i16 - panel.phys_width as i16, mon_y),
            Some(PanelAnchor::Bottom) => (mon_x, mon_y + mon_height as i16 - panel.phys_height as i16),
            None => (
                mon_x + (spec.x as f32 * self.dpr).round() as i16,
                mon_y + (spec.y as f32 * self.dpr).round() as i16,
            ),
        };

        if win_x != panel.win_x || win_y != panel.win_y {
            self.conn.configure_window(
                panel.win_id,
                &ConfigureWindowAux::new().x(win_x as i32).y(win_y as i32),
            ).map_err(|e| anyhow::anyhow!(e))?;
            panel.win_x = win_x;
            panel.win_y = win_y;
        }

        Ok(())
    }

    fn update_dimensions(&mut self, panel: &mut Panel, spec: &PanelSpecData) -> Result<(), anyhow::Error> {
        if !spec.content.is_null() {
            preload_layout_images(&spec.content);
            panel.raw_layout = Some(spec.content.clone());
            panel.render_cache = RenderCache::new(30);
        }

        let new_phys_width = (spec.width as f32 * self.dpr).round() as u32;
        let new_phys_height = (spec.height as f32 * self.dpr).round() as u32;

        if new_phys_width != panel.phys_width || new_phys_height != panel.phys_height {
            self.conn.configure_window(
                panel.win_id,
                &ConfigureWindowAux::new()
                    .width(new_phys_width)
                    .height(new_phys_height),
            ).map_err(|e| anyhow::anyhow!(e))?;
            panel.phys_width = new_phys_width;
            panel.phys_height = new_phys_height;

            if let Some(anchor) = spec.anchor.clone() {
                let (mon_x, mon_y, mon_width, mon_height) = spec.output.as_ref()
                    .and_then(|name| self.output_map.get(name).copied())
                    .unwrap_or((self.mon_x, self.mon_y, self.mon_width, self.mon_height));

                let strut_vals = strut_partial_values_for_anchor(
                    anchor, mon_x, mon_y, mon_width, mon_height, new_phys_width, new_phys_height,
                );
                self.conn.change_property32(PropMode::REPLACE, panel.win_id, self.strut_atom, AtomEnum::CARDINAL, &strut_vals)
                    .map_err(|e| anyhow::anyhow!(e))?;
                self.conn.change_property32(PropMode::REPLACE, panel.win_id, self.strut_legacy_atom, AtomEnum::CARDINAL, &strut_vals[..4])
                    .map_err(|e| anyhow::anyhow!(e))?;
            }

            self.conn.flush().map_err(|e| anyhow::anyhow!(e))?;
        }

        Ok(())
    }

    fn flush(&mut self) {
        let _ = self.conn.flush();
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
    let _ = ctx.dpi;
    let _ = ctx.output_name;
    let _ = ctx.screen_width_logical;
    let _ = ctx.screen_height_logical;
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
            dpi: 96.0,
            output_name: String::new(),
            screen_width_logical: mon_width,
            screen_height_logical: mon_height,
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

        let mut ctx = match make_panel_ctx() {
            Some(c) => c,
            None => {
                println!("SKIP: no X11 display available");
                return;
            }
        };

        let spec = make_spec("test-enter", 200, 30);
        let panel = <crate::layout::PanelSpecData as Lifecycle>::enter(spec, &mut ctx, &mut ())
            .expect("enter should succeed when X11 is available");

        assert!(panel.phys_width > 0, "phys_width must be > 0");
        assert!(panel.phys_height > 0, "phys_height must be > 0");

        // Cleanup
        let _ = <crate::layout::PanelSpecData as Lifecycle>::exit(panel, &mut ctx, &mut ());
    }

    // ---------------------------------------------------------------------------
    // Claim B: exit destroys the X11 window (get_geometry returns an error).
    // ---------------------------------------------------------------------------
    #[test]
    fn lifecycle_exit_destroys_x11_window() {
        use crate::managed_set::Lifecycle;
        use x11rb::connection::Connection as _;
        use x11rb::protocol::xproto::ConnectionExt as XprotoExt;

        let mut ctx = match make_panel_ctx() {
            Some(c) => c,
            None => {
                println!("SKIP: no X11 display available");
                return;
            }
        };

        let spec = make_spec("test-exit", 200, 30);
        let panel = <crate::layout::PanelSpecData as Lifecycle>::enter(spec, &mut ctx, &mut ())
            .expect("enter must succeed for exit test");

        let win_id = panel.win_id;

        // Sanity: window should exist before exit.
        ctx.conn.flush().ok();
        let before = XprotoExt::get_geometry(&*ctx.conn, win_id)
            .ok()
            .and_then(|c| c.reply().ok());
        assert!(before.is_some(), "window should exist before exit");

        let _ = <crate::layout::PanelSpecData as Lifecycle>::exit(panel, &mut ctx, &mut ());
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

        let mut ctx = match make_panel_ctx() {
            Some(c) => c,
            None => {
                println!("SKIP: no X11 display available");
                return;
            }
        };

        let spec = make_spec("test-update", 200, 30);
        let mut panel = <crate::layout::PanelSpecData as Lifecycle>::enter(spec, &mut ctx, &mut ())
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

        <crate::layout::PanelSpecData as Lifecycle>::reconcile_self(new_spec, &mut panel, &mut ctx, &mut ())
            .expect("reconcile_self must succeed");

        assert_eq!(
            panel.raw_layout,
            Some(new_content),
            "raw_layout should be set to the new content after update"
        );

        // Cleanup
        let _ = <crate::layout::PanelSpecData as Lifecycle>::exit(panel, &mut ctx, &mut ());
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
    // Claim D: reconcile_self resizes the X11 window when width changes.
    // ---------------------------------------------------------------------------
    #[test]
    fn reconcile_self_resizes_window_when_width_changes() {
        use crate::managed_set::Lifecycle;
        use x11rb::connection::Connection as _;
        use x11rb::protocol::xproto::ConnectionExt as XprotoExt;

        let mut ctx = match make_panel_ctx() {
            Some(c) => c,
            None => {
                println!("SKIP: no X11 display available");
                return;
            }
        };

        let spec = make_spec("test-resize-window", 200, 30);
        let mut panel = <crate::layout::PanelSpecData as Lifecycle>::enter(spec, &mut ctx, &mut ())
            .expect("enter must succeed for reconcile_self resize test");

        let new_spec = make_spec("test-resize-window", 300, 30);
        <crate::layout::PanelSpecData as Lifecycle>::reconcile_self(new_spec, &mut panel, &mut ctx, &mut ())
            .expect("reconcile_self must succeed");

        ctx.conn.flush().ok();
        let geom = XprotoExt::get_geometry(&*ctx.conn, panel.win_id)
            .ok()
            .and_then(|c| c.reply().ok())
            .expect("get_geometry should succeed after reconcile_self");

        assert_eq!(geom.width, 300u16, "X11 window width should be updated to 300");

        // Cleanup
        let _ = <crate::layout::PanelSpecData as Lifecycle>::exit(panel, &mut ctx, &mut ());
    }

    // ---------------------------------------------------------------------------
    // Claim E: reconcile_self updates phys_width in state when width changes.
    // ---------------------------------------------------------------------------
    #[test]
    fn reconcile_self_updates_phys_width_in_state() {
        use crate::managed_set::Lifecycle;

        let mut ctx = match make_panel_ctx() {
            Some(c) => c,
            None => {
                println!("SKIP: no X11 display available");
                return;
            }
        };

        let spec = make_spec("test-resize-state", 200, 30);
        let mut panel = <crate::layout::PanelSpecData as Lifecycle>::enter(spec, &mut ctx, &mut ())
            .expect("enter must succeed for reconcile_self state test");

        let new_spec = make_spec("test-resize-state", 300, 30);
        <crate::layout::PanelSpecData as Lifecycle>::reconcile_self(new_spec, &mut panel, &mut ctx, &mut ())
            .expect("reconcile_self must succeed");

        assert_eq!(panel.phys_width, 300, "phys_width in state should be updated to 300 after reconcile_self");

        // Cleanup
        let _ = <crate::layout::PanelSpecData as Lifecycle>::exit(panel, &mut ctx, &mut ());
    }

    // ---------------------------------------------------------------------------
    // Claim F: X11PanelContext implements DisplayManager — create_window returns
    // a Panel with phys_width > 0 and phys_height > 0.
    // ---------------------------------------------------------------------------
    #[test]
    fn display_manager_create_window_returns_panel_with_positive_dimensions() {
        use crate::display_manager::DisplayManager;

        let mut ctx = match make_panel_ctx() {
            Some(c) => c,
            None => {
                println!("SKIP: no X11 display available");
                return;
            }
        };

        let spec = make_spec("dm-create", 200, 30);
        let panel = <super::X11PanelContext as DisplayManager>::create_window(&mut ctx, &spec)
            .expect("create_window should succeed when X11 is available");

        assert!(panel.phys_width > 0, "phys_width must be > 0");
        assert!(panel.phys_height > 0, "phys_height must be > 0");

        // Cleanup
        let _ = <super::X11PanelContext as DisplayManager>::delete_window(&mut ctx, panel);
    }

    // ---------------------------------------------------------------------------
    // Claim G: X11PanelContext implements DisplayManager — delete_window destroys
    // the X11 window (get_geometry returns an error afterwards).
    // ---------------------------------------------------------------------------
    #[test]
    fn display_manager_delete_window_destroys_x11_window() {
        use crate::display_manager::DisplayManager;
        use x11rb::connection::Connection as _;
        use x11rb::protocol::xproto::ConnectionExt as XprotoExt;

        let mut ctx = match make_panel_ctx() {
            Some(c) => c,
            None => {
                println!("SKIP: no X11 display available");
                return;
            }
        };

        let spec = make_spec("dm-delete", 200, 30);
        let panel = <super::X11PanelContext as DisplayManager>::create_window(&mut ctx, &spec)
            .expect("create_window must succeed for delete_window test");

        let win_id = panel.win_id;

        // Sanity: window should exist before delete_window.
        ctx.conn.flush().ok();
        let before = XprotoExt::get_geometry(&*ctx.conn, win_id)
            .ok()
            .and_then(|c| c.reply().ok());
        assert!(before.is_some(), "window should exist before delete_window");

        <super::X11PanelContext as DisplayManager>::delete_window(&mut ctx, panel)
            .expect("delete_window should succeed");
        ctx.conn.flush().ok();

        // After delete_window the window must no longer exist.
        let after = XprotoExt::get_geometry(&*ctx.conn, win_id)
            .ok()
            .and_then(|c| c.reply().ok());
        assert!(after.is_none(), "get_geometry should fail after delete_window (window destroyed)");
    }

    // ---------------------------------------------------------------------------
    // Claim H: X11PanelContext implements DisplayManager — update_image does not
    // panic and flushes to the connection without error.
    // ---------------------------------------------------------------------------
    #[test]
    fn display_manager_update_image_does_not_panic_and_flushes() {
        use crate::display_manager::DisplayManager;

        let mut ctx = match make_panel_ctx() {
            Some(c) => c,
            None => {
                println!("SKIP: no X11 display available");
                return;
            }
        };

        let spec = make_spec("dm-update-image", 10, 10);
        let mut panel = <super::X11PanelContext as DisplayManager>::create_window(&mut ctx, &spec)
            .expect("create_window must succeed for update_image test");

        // Build a minimal BGRX buffer: 10 * 10 * 4 bytes, all zeros.
        let bgrx = vec![0u8; 10 * 10 * 4];
        <super::X11PanelContext as DisplayManager>::update_image(&mut ctx, &mut panel, &bgrx)
            .expect("update_image should not return an error");

        // Cleanup
        let _ = <super::X11PanelContext as DisplayManager>::delete_window(&mut ctx, panel);
    }

    // ---------------------------------------------------------------------------
    // R1: DisplayManager::update_dimensions resizes the window and updates state.
    // ---------------------------------------------------------------------------
    #[test]
    fn display_manager_update_dimensions_resizes_window_and_updates_state() {
        use crate::display_manager::DisplayManager;
        use x11rb::connection::Connection as _;
        use x11rb::protocol::xproto::ConnectionExt as XprotoExt;

        let mut ctx = match make_panel_ctx() {
            Some(c) => c,
            None => {
                println!("SKIP: no X11 display available");
                return;
            }
        };

        let mut panel = <super::X11PanelContext as DisplayManager>::create_window(
            &mut ctx,
            &make_spec("test-dm-dims", 200, 30),
        )
        .expect("create_window should succeed when X11 is available");

        <super::X11PanelContext as DisplayManager>::update_dimensions(
            &mut ctx,
            &mut panel,
            &make_spec("test-dm-dims", 300, 30),
        )
        .expect("update_dimensions should succeed");

        assert_eq!(panel.phys_width, 300, "phys_width in state should be updated to 300");

        ctx.conn.flush().ok();
        let geom = XprotoExt::get_geometry(&*ctx.conn, panel.win_id)
            .ok()
            .and_then(|c| c.reply().ok())
            .expect("get_geometry should succeed after update_dimensions");
        assert_eq!(geom.width, 300u16, "X11 window width should be 300 after update_dimensions");

        // Cleanup
        let _ = <super::X11PanelContext as DisplayManager>::delete_window(&mut ctx, panel);
    }

    // ---------------------------------------------------------------------------
    // R1b: DisplayManager::update_dimensions sets raw_layout when content is
    // not null JSON.
    // ---------------------------------------------------------------------------
    #[test]
    fn display_manager_update_dimensions_sets_raw_layout_when_content_is_not_null() {
        use crate::display_manager::DisplayManager;

        let mut ctx = match make_panel_ctx() {
            Some(c) => c,
            None => {
                println!("SKIP: no X11 display available");
                return;
            }
        };

        // Create the window with null content so raw_layout starts as None.
        let spec_no_content = make_spec("dm-upd-raw-layout", 200, 30);
        let mut panel = <super::X11PanelContext as DisplayManager>::create_window(
            &mut ctx,
            &spec_no_content,
        )
        .expect("create_window should succeed when X11 is available");

        // Build a spec with non-null content.
        let expected_content = serde_json::json!({"type": "container"});
        let spec_with_content = crate::layout::PanelSpecData {
            id: "dm-upd-raw-layout".to_string(),
            anchor: None,
            width: 200,
            height: 30,
            x: 0,
            y: 0,
            outer_gap: 0,
            output: None,
            above: false,
            content: expected_content.clone(),
        };

        <super::X11PanelContext as DisplayManager>::update_dimensions(
            &mut ctx,
            &mut panel,
            &spec_with_content,
        )
        .expect("update_dimensions should succeed");

        assert_eq!(
            panel.raw_layout,
            Some(expected_content),
            "update_dimensions should set raw_layout to Some(spec.content) when content is not null"
        );

        // Cleanup
        let _ = <super::X11PanelContext as DisplayManager>::delete_window(&mut ctx, panel);
    }

    // ---------------------------------------------------------------------------
    // R2: DisplayManager::update_position moves the window and updates state.
    // ---------------------------------------------------------------------------
    #[test]
    fn display_manager_update_position_moves_window_and_updates_state() {
        use crate::display_manager::DisplayManager;

        let mut ctx = match make_panel_ctx() {
            Some(c) => c,
            None => {
                println!("SKIP: no X11 display available");
                return;
            }
        };

        let mut panel = <super::X11PanelContext as DisplayManager>::create_window(
            &mut ctx,
            &make_spec("test-dm-pos", 100, 30),
        )
        .expect("create_window should succeed when X11 is available");

        let new_spec = crate::layout::PanelSpecData {
            id: "test-dm-pos".to_string(),
            x: 50,
            y: 20,
            width: 100,
            height: 30,
            anchor: None,
            outer_gap: 0,
            output: None,
            above: false,
            content: serde_json::Value::Null,
        };

        <super::X11PanelContext as DisplayManager>::update_position(&mut ctx, &mut panel, &new_spec)
            .expect("update_position should succeed");

        // With dpr=1.0, mon_x=0, mon_y=0: win_x = 0 + (50 * 1.0) = 50, win_y = 0 + (20 * 1.0) = 20
        assert_eq!(panel.win_x, 50, "win_x should be updated to 50 after update_position");
        assert_eq!(panel.win_y, 20, "win_y should be updated to 20 after update_position");

        // Cleanup
        let _ = <super::X11PanelContext as DisplayManager>::delete_window(&mut ctx, panel);
    }

    // ---------------------------------------------------------------------------
    // Claim H: create_window sets raw_layout to Some(spec.content) when
    // spec.content is not null JSON.
    // ---------------------------------------------------------------------------
    #[test]
    fn create_window_sets_raw_layout_when_content_is_not_null() {
        use crate::display_manager::DisplayManager;

        let mut ctx = match make_panel_ctx() {
            Some(c) => c,
            None => {
                println!("SKIP: no X11 display available");
                return;
            }
        };

        let expected_content = serde_json::json!({"type": "container"});
        let spec = crate::layout::PanelSpecData {
            id: "dm-create-raw-layout".to_string(),
            anchor: None,
            width: 200,
            height: 30,
            x: 0,
            y: 0,
            outer_gap: 0,
            output: None,
            above: false,
            content: expected_content.clone(),
        };

        let panel = <super::X11PanelContext as DisplayManager>::create_window(&mut ctx, &spec)
            .expect("create_window should succeed when X11 is available");

        assert_eq!(
            panel.raw_layout,
            Some(expected_content),
            "raw_layout should be set to Some(spec.content) after create_window"
        );

        // Cleanup
        let _ = <super::X11PanelContext as DisplayManager>::delete_window(&mut ctx, panel);
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
