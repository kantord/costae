use std::collections::HashMap;
use std::sync::{Arc, mpsc};

use takumi::GlobalContext;
use takumi::layout::Viewport;
use takumi::rendering::{RenderOptions, measure_layout};
use x11rb::{
    connection::Connection,
    protocol::xproto::*,
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};

use crate::layout::{PanelSpec, PanelAnchor};
use crate::render::{RenderCache, render_frame};
use crate::modules::hit_test;
use crate::layout::parse_layout;
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

pub struct X11Context<'a> {
    pub conn: &'a RustConnection,
    pub screen: &'a Screen,
    pub depth: u8,
    pub global: &'a GlobalContext,
    pub dpr: f32,
    /// Primary monitor coordinates (fallback when panel has no output= prop).
    pub mon_x: i16,
    pub mon_y: i16,
    pub mon_width: u32,
    pub mon_height: u32,
    pub xrootpmap_atom: Option<u32>,
    pub strut_atom: u32,
    pub strut_legacy_atom: u32,
    /// All connected outputs: name → (x, y, phys_width, phys_height).
    pub output_map: &'a HashMap<String, (i16, i16, u32, u32)>,
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
                eprintln!("[costae] root bg from _XROOTPMAP_ID pixmap in {}ms", t.elapsed().as_millis());
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
            eprintln!("[costae] root bg from adjacent pixel ({:#06x}) in {}ms", pixel, t.elapsed().as_millis());
            return Some(solid_color_rgba(pixel, width, height));
        }
    }

    // Tier 3: GetImage on root window — last resort.
    match conn.get_image(ImageFormat::Z_PIXMAP, root, win_x, win_y, width as u16, height as u16, !0) {
        Err(e) => { eprintln!("[costae] root bg send error: {e:?}"); None }
        Ok(cookie) => match cookie.reply() {
            Err(e) => { eprintln!("[costae] root bg reply error: {e:?}"); None }
            Ok(img) => {
                eprintln!("[costae] root bg from root window (fallback) in {}ms", t.elapsed().as_millis());
                Some(x11_bgrx_to_rgba(&img.data))
            }
        }
    }
}

pub fn i3_dpi(conn: &RustConnection, root: Window, screen: &Screen) -> f32 {
    let from_xresources = (|| -> Option<f32> {
        let atom = conn.intern_atom(false, b"RESOURCE_MANAGER").ok()?.reply().ok()?.atom;
        let prop = conn
            .get_property(false, root, atom, AtomEnum::ANY, 0, 65536)
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
        eprintln!("[costae] DPI {dpi:.1} (from Xft.dpi)");
        return dpi;
    }
    if screen.height_in_millimeters > 0 {
        let dpi = screen.height_in_pixels as f32 * 25.4 / screen.height_in_millimeters as f32;
        eprintln!("[costae] DPI {dpi:.1} (from screen physical dimensions)");
        return dpi;
    }
    eprintln!("[costae] DPI 96.0 (fallback)");
    96.0
}

pub fn do_hit_test(
    raw_layout: &Option<serde_json::Value>,
    module_event_txs: &HashMap<String, mpsc::Sender<serde_json::Value>>,
    global: &GlobalContext,
    phys_width: u32,
    phys_height: u32,
    dpr: f32,
    click_x: f32,
    click_y: f32,
) {
    let layout_json = match raw_layout {
        Some(l) => l,
        None => return,
    };
    let node = match raw_layout.as_ref().and_then(|layout| {
        parse_layout(layout)
            .map_err(|e| eprintln!("[costae] layout parse error: {e}"))
            .ok()
    }) {
        Some(n) => n,
        None => return,
    };

    let options = RenderOptions::builder()
        .global(global)
        .viewport(Viewport::new((Some(phys_width), Some(phys_height))).with_device_pixel_ratio(dpr))
        .node(node)
        .build();
    let measured = match measure_layout(options) {
        Ok(m) => m,
        Err(_) => return,
    };

    let (hit_path, on_click) = match hit_test(&measured, layout_json, click_x, click_y) {
        Some(r) => r,
        None => return,
    };

    if let Some(channel) = on_click.get("__channel__").and_then(|v| v.as_str()) {
        if let Some(tx) = module_event_txs.get(channel) {
            let mut payload = on_click.clone();
            if let Some(obj) = payload.as_object_mut() { obj.remove("__channel__"); }
            let _ = tx.send(serde_json::json!({"event": "click", "data": payload}));
            return;
        }
    }

    let mut path = hit_path.clone();
    loop {
        if let Some(tx) = module_event_txs.get(&path) {
            let _ = tx.send(serde_json::json!({"event": "click", "data": on_click}));
            return;
        }
        match path.rfind('/') {
            Some(pos) => path.truncate(pos),
            None => return,
        }
    }
}

pub fn create_panel(
    spec: &PanelSpec,
    x11: &X11Context,
) -> Result<Panel, Box<dyn std::error::Error>> {
    let phys_width = (spec.width as f32 * x11.dpr).round() as u32;
    let phys_height = (spec.height as f32 * x11.dpr).round() as u32;

    // Resolve which monitor this panel lives on. When spec.output names a known RandR
    // output, use its coordinates; otherwise fall back to the primary monitor.
    let (mon_x, mon_y, mon_width, mon_height) = spec.output.as_ref()
        .and_then(|name| x11.output_map.get(name).copied())
        .unwrap_or((x11.mon_x, x11.mon_y, x11.mon_width, x11.mon_height));

    // outer_gap is informational (passed to modules so they can tell i3 about tiling gaps),
    // not a window position offset. Anchored windows sit flush at the monitor edge.
    let (win_x, win_y) = match &spec.anchor {
        Some(PanelAnchor::Left)  => (mon_x, mon_y),
        Some(PanelAnchor::Right) => (mon_x + mon_width as i16 - phys_width as i16, mon_y),
        Some(PanelAnchor::Top)   => (mon_x, mon_y),
        Some(PanelAnchor::Bottom)=> (mon_x, mon_y + mon_height as i16 - phys_height as i16),
        None => (
            mon_x + (spec.x as f32 * x11.dpr).round() as i16,
            mon_y + (spec.y as f32 * x11.dpr).round() as i16,
        ),
    };

    let win_id = x11.conn.generate_id()?;
    x11.conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        win_id,
        x11.screen.root,
        win_x,
        win_y,
        phys_width as u16,
        phys_height as u16,
        0,
        WindowClass::INPUT_OUTPUT,
        x11.screen.root_visual,
        &CreateWindowAux::new()
            .background_pixel(x11.screen.black_pixel)
            .override_redirect(1)
            .event_mask(EventMask::EXPOSURE | EventMask::BUTTON_PRESS),
    )?;

    let root_bg_rgba = sample_root_bg(x11.conn, x11.screen.root, win_x, win_y, phys_width, phys_height, x11.xrootpmap_atom)
        .unwrap_or_default();
    inject_root_bg(x11.global, root_bg_rgba.clone(), phys_width, phys_height);

    x11.conn.map_window(win_id)?;
    let stack_mode = if spec.above { StackMode::ABOVE } else { StackMode::BELOW };
    x11.conn.configure_window(win_id, &ConfigureWindowAux::new().stack_mode(stack_mode))?;

    if let Some(anchor) = spec.anchor.clone() {
        let strut_vals = strut_partial_values_for_anchor(
            anchor, mon_x, mon_y, mon_width, mon_height, phys_width, phys_height,
        );
        x11.conn.change_property32(PropMode::REPLACE, win_id, x11.strut_atom, AtomEnum::CARDINAL, &strut_vals)?;
        x11.conn.change_property32(PropMode::REPLACE, win_id, x11.strut_legacy_atom, AtomEnum::CARDINAL, &strut_vals[..4])?;
    }

    let gc = x11.conn.generate_id()?;
    x11.conn.create_gc(gc, win_id, &CreateGCAux::new())?;

    x11.conn.flush()?;

    let bgrx = Arc::new(render_frame(None, x11.global, phys_width, phys_height, x11.dpr));
    x11.conn.put_image(ImageFormat::Z_PIXMAP, win_id, gc, phys_width as u16, phys_height as u16, 0, 0, 0, x11.depth, &bgrx[..])?;
    x11.conn.flush()?;

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

pub fn destroy_panel(panel: Panel, conn: &RustConnection) {
    let _ = conn.free_gc(panel.gc);
    let _ = conn.destroy_window(panel.win_id);
}
