use std::collections::HashMap;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use std::sync::Arc;

use costae::{GlobalContext, RenderCache, hit_test, inject_root_bg, load_fonts, parse_layout, preload_layout_images, reconcile_panels, reconcile_streams, render_frame, solid_color_rgba, spawn_bi_stream, spawn_string_stream, x11_bgrx_to_rgba};
use takumi::{layout::Viewport, rendering::{RenderOptions, measure_layout}};
use x11rb::{
    connection::Connection,
    protocol::{randr::ConnectionExt as RandrExt, xproto::*},
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};

fn resolve_layout(raw_layout: &Option<serde_json::Value>) -> Option<takumi::layout::node::Node> {
    raw_layout.as_ref().and_then(|layout| {
        parse_layout(layout)
            .map_err(|e| eprintln!("[costae] layout parse error: {e}"))
            .ok()
    })
}

/// Sample the wallpaper pixels behind a panel window and return them as RGBA.
///
/// Tries three tiers in order: _XROOTPMAP_ID pixmap (best, immune to stacking),
/// solid-color fallback from the adjacent pixel, then a raw GetImage on the root
/// window.  Returns the RGBA bytes on success, or `None` if every tier fails.
fn sample_root_bg(
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

fn i3_dpi(conn: &RustConnection, root: Window, screen: &Screen) -> f32 {
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

fn do_hit_test(
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
    let node = match resolve_layout(raw_layout) {
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

/// A live X11 panel window, created from a `PanelSpec` at runtime.
struct Panel {
    id: String,
    win_id: u32,
    gc: u32,
    win_x: i16,
    win_y: i16,
    phys_width: u32,
    phys_height: u32,
    /// Per-panel wallpaper snapshot (RGBA). Re-injected as "root-bg" before each render
    /// so every panel sees the correct region of the wallpaper behind it.
    root_bg_rgba: Vec<u8>,
    raw_layout: Option<serde_json::Value>,
    render_cache: RenderCache,
    bgrx: Arc<Vec<u8>>,
}

struct X11Context<'a> {
    conn: &'a RustConnection,
    screen: &'a Screen,
    depth: u8,
    global: &'a GlobalContext,
    dpr: f32,
    /// Primary monitor coordinates (fallback when panel has no output= prop).
    mon_x: i16,
    mon_y: i16,
    mon_width: u32,
    mon_height: u32,
    xrootpmap_atom: Option<u32>,
    strut_atom: u32,
    strut_legacy_atom: u32,
    /// All connected outputs: name → (x, y, phys_width, phys_height).
    output_map: &'a HashMap<String, (i16, i16, u32, u32)>,
}

fn create_panel(
    spec: &costae::PanelSpec,
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
        Some(costae::PanelAnchor::Left)  => (mon_x, mon_y),
        Some(costae::PanelAnchor::Right) => (mon_x + mon_width as i16 - phys_width as i16, mon_y),
        Some(costae::PanelAnchor::Top)   => (mon_x, mon_y),
        Some(costae::PanelAnchor::Bottom)=> (mon_x, mon_y + mon_height as i16 - phys_height as i16),
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
        let strut_vals = costae::strut_partial_values_for_anchor(
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

fn destroy_panel(panel: Panel, conn: &RustConnection) {
    let _ = conn.free_gc(panel.gc);
    let _ = conn.destroy_window(panel.win_id);
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let exe_path = std::env::current_exe().unwrap_or_default();

    let layout_jsx_path = {
        let home = std::env::var("HOME").unwrap_or_default();
        std::path::PathBuf::from(home).join(".config/costae/layout.jsx")
    };

    let (wake_tx, wake_rx) = mpsc::sync_channel::<()>(1);

    let (reload_tx, reload_rx) = mpsc::channel::<()>();
    {
        let path = layout_jsx_path.clone();
        let wake_tx = wake_tx.clone();
        thread::spawn(move || {
            let mut last_modified = std::fs::metadata(&path)
                .and_then(|m| m.modified())
                .ok();
            loop {
                thread::sleep(Duration::from_millis(500));
                let modified = std::fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .ok();
                if modified != last_modified {
                    last_modified = modified;
                    let _ = reload_tx.send(());
                    let _ = wake_tx.try_send(());
                }
            }
        });
    }

    // If we were exec'd by a previous instance, it passes the mtime it saw so we
    // don't immediately re-trigger on the same installation.
    let exe_baseline: Option<std::time::SystemTime> =
        std::env::var("COSTAE_EXE_MTIME_NS")
            .ok()
            .and_then(|s| s.parse::<u128>().ok())
            .and_then(|ns| {
                std::time::UNIX_EPOCH.checked_add(std::time::Duration::from_nanos(ns as u64))
            })
            .or_else(|| std::fs::metadata(&exe_path).and_then(|m| m.modified()).ok());

    let (bin_reload_tx, bin_reload_rx) = mpsc::channel::<()>();
    if exe_path.exists() {
        let path = exe_path.clone();
        let wake_tx = wake_tx.clone();
        thread::spawn(move || {
            let mut last_modified = exe_baseline;
            loop {
                thread::sleep(Duration::from_millis(500));
                let modified = std::fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .ok();
                if modified != last_modified {
                    last_modified = modified;
                    let _ = bin_reload_tx.send(());
                    let _ = wake_tx.try_send(());
                }
            }
        });
    }

    let mut module_event_txs: HashMap<String, mpsc::Sender<serde_json::Value>> = HashMap::new();
    let mut bi_stream_children: HashMap<String, std::process::Child> = HashMap::new();

    let mut stream_values: HashMap<String, String> = HashMap::new();
    let mut stream_children: HashMap<(String, Option<String>), std::process::Child> = HashMap::new();
    let (stream_tx, stream_rx) = mpsc::channel::<(String, Option<String>, String)>();
    let mut jsx_evaluator: Option<costae::jsx::JsxEvaluator> = None;

    let (conn, screen_num) = RustConnection::connect(None)?;
    let screen = &conn.setup().roots[screen_num];
    let depth = screen.root_depth;

    let dpi = i3_dpi(&conn, screen.root, screen);

    let primary_output = conn.randr_get_output_primary(screen.root)?.reply()?.output;
    let output_info = conn.randr_get_output_info(primary_output, 0)?.reply()?;
    let output_name = String::from_utf8_lossy(&output_info.name).into_owned();
    let crtc_info = conn.randr_get_crtc_info(output_info.crtc, 0)?.reply()?;
    let mon_x = crtc_info.x;
    let mon_y = crtc_info.y;
    let mon_height = crtc_info.height as u32;
    let mon_width = crtc_info.width as u32;
    let dpr = dpi / 96.0;

    // Enumerate all connected outputs: name → (x, y, phys_width, phys_height).
    let mut output_map: HashMap<String, (i16, i16, u32, u32)> = HashMap::new();
    if let Ok(cookie) = conn.randr_get_screen_resources_current(screen.root) {
        if let Ok(resources) = cookie.reply() {
            for &out_id in &resources.outputs {
                if let Ok(info_cookie) = conn.randr_get_output_info(out_id, 0) {
                    if let Ok(info) = info_cookie.reply() {
                        if info.crtc == 0 { continue; } // not active
                        if let Ok(crtc_cookie) = conn.randr_get_crtc_info(info.crtc, 0) {
                            if let Ok(crtc) = crtc_cookie.reply() {
                                let name = String::from_utf8_lossy(&info.name).into_owned();
                                output_map.insert(name, (crtc.x, crtc.y, crtc.width as u32, crtc.height as u32));
                            }
                        }
                    }
                }
            }
        }
    }

    let mut global = GlobalContext::default();
    load_fonts(&mut global);

    // Watch for wallpaper changes: wallpaper setters (feh, nitrogen, etc.) signal a new
    // wallpaper by updating the _XROOTPMAP_ID property on the root window.
    conn.change_window_attributes(
        screen.root,
        &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
    )?;
    let xrootpmap_atom: Option<u32> = conn
        .intern_atom(false, b"_XROOTPMAP_ID").ok()
        .and_then(|c| c.reply().ok())
        .map(|r| r.atom);

    let strut_atom = conn.intern_atom(false, b"_NET_WM_STRUT_PARTIAL")?.reply()?.atom;
    let strut_legacy_atom = conn.intern_atom(false, b"_NET_WM_STRUT")?.reply()?.atom;

    let x11 = X11Context {
        conn: &conn,
        screen,
        depth,
        global: &global,
        dpr,
        mon_x,
        mon_y,
        mon_width,
        mon_height,
        xrootpmap_atom,
        strut_atom,
        strut_legacy_atom,
        output_map: &output_map,
    };

    // Express screen dimensions in logical CSS px (physical ÷ DPR) so that layout code
    // can use ctx.screen_height directly in panel height props without double-scaling.
    // create_panel multiplies by DPR to recover physical pixels.
    let screen_width_logical = (mon_width as f32 / dpr).round() as u32;
    let screen_height_logical = (mon_height as f32 / dpr).round() as u32;

    // Build outputs array for jsx: all connected monitors with logical dimensions.
    let outputs_json: Vec<serde_json::Value> = output_map.iter().map(|(name, &(_, _, pw, ph))| {
        serde_json::json!({
            "name": name,
            "screen_width":  (pw as f32 / dpr).round() as u32,
            "screen_height": (ph as f32 / dpr).round() as u32,
        })
    }).collect();

    let jsx_ctx = serde_json::json!({
        "output": output_name,
        "dpi": dpi,
        "screen_width": screen_width_logical,
        "screen_height": screen_height_logical,
        "outputs": outputs_json,
    });

    let mut panels: Vec<Panel> = Vec::new();

    // Build the init_event that is sent to bi-stream modules (e.g. costae-i3).
    // Uses the left-anchored panel spec so costae-i3 gets the correct bar_width for
    // its i3 gap command. Falls back to first spec if no left-anchored panel exists.
    let make_mod_init = |specs: &[costae::PanelSpec]| -> serde_json::Value {
        let spec = specs.iter()
            .find(|p| p.anchor == Some(costae::PanelAnchor::Left))
            .or_else(|| specs.first());
        let (bar_w, og) = spec
            .map(|p| (
                (p.width as f32 * dpr).round() as u32,
                (p.outer_gap as f32 * dpr).round() as u32,
            ))
            .unwrap_or((250, 0));
        serde_json::json!({
            "type": "init",
            "config": {"width": bar_w, "outer_gap": og},
            "output": output_name,
            "dpi": dpi,
            "screen_width": screen_width_logical,
            "screen_height": screen_height_logical,
        })
    };

    // Helper: apply a new set of PanelSpecs — create/destroy panels and update raw_layout.
    // Module spawning is done by the caller before invoking this.
    let apply_panel_specs = |panels: &mut Vec<Panel>, specs: Vec<costae::PanelSpec>| {
        let existing_ids: Vec<&str> = panels.iter().map(|p| p.id.as_str()).collect();
        let (to_create, to_update, to_destroy) = reconcile_panels(&existing_ids, &specs);

        for id in &to_destroy {
            if let Some(pos) = panels.iter().position(|p| &p.id == id) {
                let panel = panels.remove(pos);
                let _ = conn.free_gc(panel.gc);
                let _ = conn.destroy_window(panel.win_id);
            }
        }

        for spec in &to_create {
            match create_panel(spec, &x11) {
                Ok(mut panel) => {
                    let content = specs.iter().find(|s| s.id == spec.id).map(|s| s.content.clone()).unwrap_or_default();
                    if !content.is_null() {
                        preload_layout_images(&content, &global);
                        panel.raw_layout = Some(content);
                    }
                    panels.push(panel);
                }
                Err(e) => eprintln!("[costae] failed to create panel '{}': {e}", spec.id),
            }
        }

        for spec in &to_update {
            if let Some(panel) = panels.iter_mut().find(|p| p.id == spec.id) {
                let content = specs.iter().find(|s| s.id == spec.id).map(|s| s.content.clone()).unwrap_or_default();
                if !content.is_null() {
                    preload_layout_images(&content, &global);
                    panel.raw_layout = Some(content);
                    panel.render_cache = RenderCache::new(30);
                }
            }
        }
    };

    // Initial JSX load
    if layout_jsx_path.exists() {
        match std::fs::read_to_string(&layout_jsx_path) {
            Ok(source) => {
                let t = std::time::Instant::now();
                match costae::jsx::JsxEvaluator::new(&source, jsx_ctx.clone()) {
                    Ok(evaluator) => {
                        match evaluator.eval(&stream_values) {
                            Ok((value, stream_calls, module_calls)) => {
                                eprintln!("[costae] jsx eval — {}ms", t.elapsed().as_millis());
                                let specs = match costae::parse_root_node(&value) {
                                    Ok(s) => s,
                                    Err(e) => { eprintln!("[costae] root node parse error: {e}"); vec![] }
                                };
                                let mod_init = make_mod_init(&specs);
                                let (to_spawn, _) = reconcile_streams(&[], &stream_calls);
                                for (bin, script) in to_spawn {
                                    let child = spawn_string_stream(&bin, script.as_deref(), stream_tx.clone(), wake_tx.clone());
                                    stream_children.insert((bin, script), child);
                                }
                                for bin in &module_calls {
                                    if !bi_stream_children.contains_key(bin) {
                                        let bi = spawn_bi_stream(bin, &mod_init, stream_tx.clone(), wake_tx.clone());
                                        module_event_txs.insert(bin.clone(), bi.event_tx);
                                        bi_stream_children.insert(bin.clone(), bi.child);
                                    }
                                }
                                apply_panel_specs(&mut panels, specs);
                                jsx_evaluator = Some(evaluator);
                            }
                            Err(e) => eprintln!("[costae] JSX eval error: {e}"),
                        }
                    }
                    Err(e) => eprintln!("[costae] JSX compile error: {e}"),
                }
            }
            Err(e) => eprintln!("[costae] JSX file error: {e}"),
        }
    }

    loop {
        let mut changed = false;

        if bin_reload_rx.try_recv().is_ok() {
            eprintln!("[costae] binary changed, restarting...");
            for child in stream_children.values_mut() {
                let _ = child.kill();
                let _ = child.wait();
            }
            for child in bi_stream_children.values_mut() {
                let _ = child.kill();
                let _ = child.wait();
            }
            for panel in panels.drain(..) {
                destroy_panel(panel, &conn);
            }
            let _ = conn.flush();
            use std::os::unix::process::CommandExt;
            let mut cmd = std::process::Command::new(&exe_path);
            if let Ok(mtime) = std::fs::metadata(&exe_path).and_then(|m| m.modified()) {
                if let Ok(dur) = mtime.duration_since(std::time::UNIX_EPOCH) {
                    cmd.env("COSTAE_EXE_MTIME_NS", dur.as_nanos().to_string());
                }
            }
            let _ = cmd.exec();
        }

        if reload_rx.try_recv().is_ok() {
            for (_, mut child) in stream_children.drain() {
                let _ = child.kill();
                let _ = child.wait();
            }
            for (_, mut child) in bi_stream_children.drain() {
                let _ = child.kill();
                let _ = child.wait();
            }
            module_event_txs.clear();
            stream_values.clear();
            jsx_evaluator = None;

            if layout_jsx_path.exists() {
                match std::fs::read_to_string(&layout_jsx_path) {
                    Ok(source) => match costae::jsx::JsxEvaluator::new(&source, jsx_ctx.clone()) {
                        Ok(evaluator) => match evaluator.eval(&stream_values) {
                            Ok((value, stream_calls, module_calls)) => {
                                let specs = match costae::parse_root_node(&value) {
                                    Ok(s) => s,
                                    Err(e) => { eprintln!("[costae] root node parse error: {e}"); vec![] }
                                };
                                let mod_init = make_mod_init(&specs);
                                let (to_spawn, _) = reconcile_streams(&[], &stream_calls);
                                for (bin, script) in to_spawn {
                                    let child = spawn_string_stream(&bin, script.as_deref(), stream_tx.clone(), wake_tx.clone());
                                    stream_children.insert((bin, script), child);
                                }
                                for bin in &module_calls {
                                    if !bi_stream_children.contains_key(bin) {
                                        let bi = spawn_bi_stream(bin, &mod_init, stream_tx.clone(), wake_tx.clone());
                                        module_event_txs.insert(bin.clone(), bi.event_tx);
                                        bi_stream_children.insert(bin.clone(), bi.child);
                                    }
                                }
                                apply_panel_specs(&mut panels, specs);
                                jsx_evaluator = Some(evaluator);
                            }
                            Err(e) => eprintln!("[costae] JSX eval error: {e}"),
                        },
                        Err(e) => eprintln!("[costae] JSX compile error: {e}"),
                    },
                    Err(e) => eprintln!("[costae] JSX file error: {e}"),
                }
            }
            eprintln!("[costae] layout reloaded");
            changed = true;
        }

        let mut streams_updated = false;
        while let Ok((bin, script, value)) = stream_rx.try_recv() {
            let key = format!("{}\0{}", bin, script.as_deref().unwrap_or_default());
            if stream_values.get(&key).map(|s| s.as_str()) != Some(value.as_str()) {
                stream_values.insert(key, value);
                streams_updated = true;
            }
        }
        if streams_updated {
            if let Some(ref evaluator) = jsx_evaluator {
                let t = std::time::Instant::now();
                match evaluator.eval(&stream_values) {
                    Ok((new_value, new_calls, new_module_calls)) => {
                        eprintln!("[costae] jsx re-eval — {}µs", t.elapsed().as_micros());
                        let specs = match costae::parse_root_node(&new_value) {
                            Ok(s) => s,
                            Err(e) => { eprintln!("[costae] root node parse error: {e}"); vec![] }
                        };
                        let mod_init = make_mod_init(&specs);
                        let current_calls: Vec<_> = stream_children.keys().cloned().collect();
                        let (to_spawn, to_kill) = reconcile_streams(&current_calls, &new_calls);
                        for (b, s) in to_kill {
                            if let Some(mut child) = stream_children.remove(&(b, s)) {
                                let _ = child.kill();
                                let _ = child.wait();
                            }
                        }
                        for (b, s) in to_spawn {
                            let child = spawn_string_stream(&b, s.as_deref(), stream_tx.clone(), wake_tx.clone());
                            stream_children.insert((b, s), child);
                        }
                        for b in &new_module_calls {
                            if !bi_stream_children.contains_key(b) {
                                let bi = spawn_bi_stream(b, &mod_init, stream_tx.clone(), wake_tx.clone());
                                module_event_txs.insert(b.clone(), bi.event_tx);
                                bi_stream_children.insert(b.clone(), bi.child);
                            }
                        }
                        apply_panel_specs(&mut panels, specs);
                        changed = true;
                    }
                    Err(e) => eprintln!("[costae] JSX re-eval error: {e}"),
                }
            }
        }

        // Process X11 events
        while let Some(event) = conn.poll_for_event()? {
            match event {
                x11rb::protocol::Event::Expose(e) => {
                    if let Some(panel) = panels.iter().find(|p| p.win_id == e.window) {
                        conn.put_image(ImageFormat::Z_PIXMAP, panel.win_id, panel.gc, panel.phys_width as u16, panel.phys_height as u16, 0, 0, 0, depth, &panel.bgrx[..])?;
                        conn.flush()?;
                    }
                }
                x11rb::protocol::Event::ButtonPress(e) => {
                    if let Some(panel) = panels.iter().find(|p| p.win_id == e.event) {
                        do_hit_test(
                            &panel.raw_layout, &module_event_txs,
                            &global, panel.phys_width, panel.phys_height, dpr,
                            e.event_x as f32, e.event_y as f32,
                        );
                    }
                }
                x11rb::protocol::Event::PropertyNotify(e) => {
                    if xrootpmap_atom == Some(e.atom) {
                        // Wallpaper changed: re-sample each panel's own region using the
                        // same 3-tier fallback as initial sampling so every monitor gets
                        // the correct pixels regardless of pixmap layout.
                        for panel in panels.iter_mut() {
                            if let Some(rgba) = sample_root_bg(&conn, screen.root, panel.win_x, panel.win_y, panel.phys_width, panel.phys_height, xrootpmap_atom) {
                                panel.root_bg_rgba = rgba;
                                eprintln!("[costae] root bg updated for panel '{}'", panel.id);
                            }
                            panel.render_cache = RenderCache::new(30);
                        }
                        changed = true;
                    }
                }
                _ => {}
            }
        }

        if changed {
            let key = serde_json::to_value(&stream_values).unwrap_or_default();
            for panel in panels.iter_mut() {
                // Re-inject this panel's own wallpaper snapshot as "root-bg" before
                // rendering so backdrop-blur and background-image read the correct pixels.
                inject_root_bg(&global, panel.root_bg_rgba.clone(), panel.phys_width, panel.phys_height);
                panel.bgrx = panel.render_cache.get_or_render(&key, || {
                    let t = std::time::Instant::now();
                    let layout = resolve_layout(&panel.raw_layout);
                    eprintln!("[costae] resolve_layout — {}µs", t.elapsed().as_micros());
                    render_frame(layout, &global, panel.phys_width, panel.phys_height, dpr)
                });
                conn.put_image(ImageFormat::Z_PIXMAP, panel.win_id, panel.gc, panel.phys_width as u16, panel.phys_height as u16, 0, 0, 0, depth, &panel.bgrx[..])?;
            }
            conn.flush()?;
        }

        let _ = wake_rx.recv_timeout(Duration::from_millis(50));
    }
}
