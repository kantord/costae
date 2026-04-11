use std::collections::HashMap;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use std::sync::Arc;

use costae::{GlobalContext, RenderCache, hit_test, inject_root_bg, load_fonts, parse_layout, preload_layout_images, reconcile_streams, render_frame, solid_color_rgba, spawn_bi_stream, spawn_string_stream, x11_bgrx_to_rgba};
use takumi::{layout::Viewport, rendering::{RenderOptions, measure_layout}};
use x11rb::{
    connection::Connection,
    protocol::{randr::ConnectionExt as RandrExt, xproto::*},
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};

const DEFAULT_BAR_WIDTH: u32 = 300;

fn resolve_layout(raw_layout: &Option<serde_json::Value>) -> Option<takumi::layout::node::Node> {
    raw_layout.as_ref().and_then(|layout| {
        parse_layout(layout)
            .map_err(|e| eprintln!("[costae] layout parse error: {e}"))
            .ok()
    })
}

fn sample_and_inject_root_bg(
    conn: &RustConnection,
    root: Window,
    global: &GlobalContext,
    mon_x: i16,
    mon_y: i16,
    width: u32,
    height: u32,
    xrootpmap_atom: Option<u32>,
) {
    let t = std::time::Instant::now();

    // Tier 1: _XROOTPMAP_ID pixmap (set by feh/nitrogen for wallpapers).
    // GetImage on a *pixmap* is immune to window stacking — it always returns
    // the actual stored pixels regardless of what windows are on screen.
    if let Some(atom) = xrootpmap_atom {
        let pixmap = conn
            .get_property(false, root, atom, AtomEnum::ANY, 0, 1).ok()
            .and_then(|c| c.reply().ok())
            .filter(|p| p.value.len() >= 4)
            .and_then(|p| p.value[..4].try_into().ok().map(u32::from_ne_bytes));
        if let Some(pixmap_id) = pixmap {
            if let Some(img) = conn.get_image(ImageFormat::Z_PIXMAP, pixmap_id, mon_x, mon_y, width as u16, height as u16, !0)
                .ok().and_then(|c| c.reply().ok())
            {
                let rgba = x11_bgrx_to_rgba(&img.data);
                inject_root_bg(global, rgba, width, height);
                eprintln!("[costae] root bg from _XROOTPMAP_ID pixmap in {}ms", t.elapsed().as_millis());
                return;
            }
        }
    }

    // Tier 2: sample 1 pixel from just outside the bar area (one pixel to the right).
    // This position is never covered by windows placed in the bar's own column, so for
    // solid-color backgrounds (the common case when _XROOTPMAP_ID is absent) we get the
    // correct color. We then fill the entire bar area with that single color via
    // solid_color_rgba, which is far cheaper than a full GetImage on the bar region.
    if let Some(img) = conn.get_image(
        ImageFormat::Z_PIXMAP, root,
        mon_x + width as i16, mon_y,
        1, 1, !0,
    ).ok().and_then(|c| c.reply().ok()) {
        if img.data.len() >= 4 {
            // BGRX: data[0]=B, data[1]=G, data[2]=R, data[3]=X
            let pixel = ((img.data[2] as u32) << 16)
                | ((img.data[1] as u32) << 8)
                | (img.data[0] as u32);
            let rgba = solid_color_rgba(pixel, width, height);
            inject_root_bg(global, rgba, width, height);
            eprintln!("[costae] root bg from adjacent pixel ({:#06x}) in {}ms", pixel, t.elapsed().as_millis());
            return;
        }
    }

    // Tier 3: GetImage on root window — last resort. Returns visible screen content,
    // so it may capture overlying windows rather than the true background.
    match conn.get_image(ImageFormat::Z_PIXMAP, root, mon_x, mon_y, width as u16, height as u16, !0) {
        Err(e) => eprintln!("[costae] root bg send error: {e:?}"),
        Ok(cookie) => match cookie.reply() {
            Err(e) => eprintln!("[costae] root bg reply error: {e:?}"),
            Ok(img) => {
                let rgba = x11_bgrx_to_rgba(&img.data);
                inject_root_bg(global, rgba, width, height);
                eprintln!("[costae] root bg from root window (fallback) in {}ms", t.elapsed().as_millis());
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
    phys_bar_width: u32,
    mon_height: u32,
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
        .viewport(Viewport::new((Some(phys_bar_width), Some(mon_height))).with_device_pixel_ratio(dpr))
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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let exe_path = std::env::current_exe().unwrap_or_default();

    let config_path = costae::default_config_path();
    let (bar_width, outer_gap, mut raw_layout, mut layout_file) = if config_path.exists() {
        match costae::load_config(&config_path) {
            Ok(cfg) => {
                eprintln!("[costae] config loaded: width={}, outer_gap={}", cfg.config.width, cfg.config.outer_gap);
                (cfg.config.width, cfg.config.outer_gap, cfg.layout, cfg.layout_file)
            }
            Err(e) => {
                eprintln!("[costae] config error: {e}, using defaults");
                (DEFAULT_BAR_WIDTH, 0, None, None)
            }
        }
    } else {
        (DEFAULT_BAR_WIDTH, 0, None, None)
    };

    let (wake_tx, wake_rx) = mpsc::sync_channel::<()>(1);

    let (reload_tx, reload_rx) = mpsc::channel::<()>();
    {
        let path = config_path.clone();
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
    let mut layout_source: Option<String> = None;

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
    // mon_height is physical pixels (from CRTC); bar_width is logical pixels (from config).
    // We match i3's scaling: dpr = dpi/96, so config px values have the same meaning as in i3.
    let mon_height = crtc_info.height as u32;
    // Scale bar_width (logical CSS px from config) to physical pixels, matching i3's DPI scaling.
    let dpr = dpi / 96.0;
    let phys_bar_width = (bar_width as f32 * dpr).round() as u32;
    let phys_outer_gap = (outer_gap as f32 * dpr).round() as u32;

    let win_id = conn.generate_id()?;
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        win_id,
        screen.root,
        mon_x,
        mon_y,
        phys_bar_width as u16,
        mon_height as u16,
        0,
        WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        &CreateWindowAux::new()
            .background_pixel(screen.black_pixel)
            .override_redirect(1)
            .event_mask(EventMask::EXPOSURE | EventMask::BUTTON_PRESS),
    )?;
    let mut global = GlobalContext::default();
    load_fonts(&mut global);

    // Watch for wallpaper changes: wallpaper setters (feh, nitrogen, etc.) signal a new
    // wallpaper by updating the _XROOTPMAP_ID property on the root window.
    conn.change_window_attributes(
        screen.root,
        &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
    )?;
    // Cache the atom ID so PropertyNotify events can be matched cheaply in the event loop.
    let xrootpmap_atom: Option<u32> = conn
        .intern_atom(false, b"_XROOTPMAP_ID").ok()
        .and_then(|c| c.reply().ok())
        .map(|r| r.atom);

    // Sample before mapping — X11 does not maintain a backing store for root window pixels
    // once a window covers them (no compositor). Reading after map_window returns black.
    eprintln!("[costae] sampling root bg at ({mon_x},{mon_y}) size {phys_bar_width}×{mon_height}");
    sample_and_inject_root_bg(&conn, screen.root, &global, mon_x, mon_y, phys_bar_width, mon_height, xrootpmap_atom);

    conn.map_window(win_id)?;
    conn.configure_window(win_id, &ConfigureWindowAux::new().stack_mode(StackMode::BELOW))?;

    // Tell i3 (and any EWMH-compliant WM) that we occupy the left edge of the monitor.
    // This prevents tiling windows from appearing under the bar and may suppress the
    // focused-workspace indicator border that otherwise shows at x=bar_width.
    {
        let strut_atom = conn.intern_atom(false, b"_NET_WM_STRUT_PARTIAL")?.reply()?.atom;
        let strut_vals = costae::strut_partial_values(mon_x, mon_y, bar_width, mon_height);
        conn.change_property32(PropMode::REPLACE, win_id, strut_atom, AtomEnum::CARDINAL, &strut_vals)?;
        // Legacy _NET_WM_STRUT (first 4 values) for older WMs
        let strut_legacy_atom = conn.intern_atom(false, b"_NET_WM_STRUT")?.reply()?.atom;
        conn.change_property32(PropMode::REPLACE, win_id, strut_legacy_atom, AtomEnum::CARDINAL, &strut_vals[..4])?;
    }

    conn.flush()?;

    let gc = conn.generate_id()?;
    conn.create_gc(gc, win_id, &CreateGCAux::new())?;

    let init_event = serde_json::json!({
        "type": "init",
        "config": {"width": phys_bar_width, "outer_gap": phys_outer_gap},
        "output": output_name,
        "dpi": dpi
    });

    let jsx_ctx = serde_json::json!({
        "output": output_name,
        "dpi": dpi,
        "width": bar_width,
        "outer_gap": outer_gap,
    });

    if let Some(ref path) = layout_file {
        match std::fs::read_to_string(path) {
            Ok(source) => {
                let t = std::time::Instant::now();
                match costae::jsx::eval_jsx(&source, jsx_ctx.clone(), &stream_values) {
                    Ok((value, stream_calls, module_calls)) => {
                        eprintln!("[costae] jsx eval — {}ms", t.elapsed().as_millis());
                        let (to_spawn, _) = reconcile_streams(&[], &stream_calls);
                        for (bin, script) in to_spawn {
                            let child = spawn_string_stream(&bin, script.as_deref(), stream_tx.clone(), wake_tx.clone());
                            stream_children.insert((bin, script), child);
                        }
                        for bin in &module_calls {
                            if !bi_stream_children.contains_key(bin) {
                                let bi = spawn_bi_stream(bin, &init_event, stream_tx.clone(), wake_tx.clone());
                                module_event_txs.insert(bin.clone(), bi.event_tx);
                                bi_stream_children.insert(bin.clone(), bi.child);
                            }
                        }
                        raw_layout = Some(value);
                        layout_source = Some(source);
                    }
                    Err(e) => { eprintln!("[costae] JSX eval error: {e}"); }
                }
            }
            Err(e) => { eprintln!("[costae] JSX file error: {e}"); }
        }
    }

    if let Some(ref layout) = raw_layout {
        preload_layout_images(layout, &global);
    }

    let mut render_cache = RenderCache::new(30);
    let mut bgrx: Arc<Vec<u8>> = render_cache.get_or_render(
        &serde_json::to_value(&stream_values).unwrap_or_default(),
        || {
            let t = std::time::Instant::now();
            let layout = resolve_layout(&raw_layout);
            eprintln!("[costae] resolve_layout — {}µs", t.elapsed().as_micros());
            render_frame(layout, &global, phys_bar_width, mon_height, dpr)
        },
    );

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
            let _ = conn.destroy_window(win_id);
            let _ = conn.flush();
            use std::os::unix::process::CommandExt;
            let mut cmd = std::process::Command::new(&exe_path);
            // Tell the new process what mtime we saw so it doesn't re-trigger immediately
            if let Ok(mtime) = std::fs::metadata(&exe_path).and_then(|m| m.modified()) {
                if let Ok(dur) = mtime.duration_since(std::time::UNIX_EPOCH) {
                    cmd.env("COSTAE_EXE_MTIME_NS", dur.as_nanos().to_string());
                }
            }
            let _ = cmd.exec();
            // exec failed — continue running
        }

        if reload_rx.try_recv().is_ok() {
            if let Ok(cfg) = costae::load_config(&config_path) {
                if cfg.config.width != bar_width || cfg.config.outer_gap != outer_gap {
                    eprintln!("[costae] bar width changed, restarting...");
                    for child in bi_stream_children.values_mut() {
                        let _ = child.kill();
                        let _ = child.wait();
                    }
                    use std::os::unix::process::CommandExt;
                    let _ = std::process::Command::new(&exe_path).exec();
                }
                raw_layout = cfg.layout;
                layout_file = cfg.layout_file.clone();
            }
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
            layout_source = None;
            if let Some(ref path) = layout_file {
                match std::fs::read_to_string(path) {
                    Ok(source) => match costae::jsx::eval_jsx(&source, jsx_ctx.clone(), &stream_values) {
                        Ok((value, stream_calls, module_calls)) => {
                            let (to_spawn, _) = reconcile_streams(&[], &stream_calls);
                            for (bin, script) in to_spawn {
                                let child = spawn_string_stream(&bin, script.as_deref(), stream_tx.clone(), wake_tx.clone());
                                stream_children.insert((bin, script), child);
                            }
                            for bin in &module_calls {
                                if !bi_stream_children.contains_key(bin) {
                                    let bi = spawn_bi_stream(bin, &init_event, stream_tx.clone(), wake_tx.clone());
                                    module_event_txs.insert(bin.clone(), bi.event_tx);
                                    bi_stream_children.insert(bin.clone(), bi.child);
                                }
                            }
                            raw_layout = Some(value);
                            layout_source = Some(source);
                        }
                        Err(e) => { eprintln!("[costae] JSX eval error: {e}"); }
                    },
                    Err(e) => { eprintln!("[costae] JSX file error: {e}"); }
                }
            }
            render_cache = RenderCache::new(30);
            if let Some(ref layout) = raw_layout {
                preload_layout_images(layout, &global);
            }
            eprintln!("[costae] config reloaded");
            changed = true;
        }

        while let Ok((bin, script, value)) = stream_rx.try_recv() {
            let key = format!("{}\0{}", bin, script.as_deref().unwrap_or_default());
            if stream_values.get(&key).map(|s| s.as_str()) != Some(value.as_str()) {
                stream_values.insert(key, value);
                if let Some(ref source) = layout_source {
                    let t = std::time::Instant::now();
                    match costae::jsx::eval_jsx(source, jsx_ctx.clone(), &stream_values) {
                        Ok((new_layout, new_calls, new_module_calls)) => {
                            eprintln!("[costae] jsx re-eval — {}µs", t.elapsed().as_micros());
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
                                    let bi = spawn_bi_stream(b, &init_event, stream_tx.clone(), wake_tx.clone());
                                    module_event_txs.insert(b.clone(), bi.event_tx);
                                    bi_stream_children.insert(b.clone(), bi.child);
                                }
                            }
                            raw_layout = Some(new_layout);
                            changed = true;
                        }
                        Err(e) => { eprintln!("[costae] JSX re-eval error: {e}"); }
                    }
                }
            }
        }

        // Process X11 events before rendering so PropertyNotify-triggered resamples
        // are included in the same render pass.
        while let Some(event) = conn.poll_for_event()? {
            match event {
                x11rb::protocol::Event::Expose(_) => {
                    conn.put_image(ImageFormat::Z_PIXMAP, win_id, gc, phys_bar_width as u16, mon_height as u16, 0, 0, 0, depth, &bgrx[..])?;
                    conn.flush()?;
                }
                x11rb::protocol::Event::ButtonPress(e) => {
                    do_hit_test(
                        &raw_layout, &module_event_txs,
                        &global, phys_bar_width, mon_height, dpr,
                        e.event_x as f32, e.event_y as f32,
                    );
                }
                x11rb::protocol::Event::PropertyNotify(e) => {
                    // Wallpaper changed — read from _XROOTPMAP_ID pixmap directly,
                    // which feh/nitrogen set and which is independent of our window being on top.
                    if xrootpmap_atom == Some(e.atom) {
                        if let Some(atom) = xrootpmap_atom {
                            let pixmap = conn
                                .get_property(false, screen.root, atom, AtomEnum::ANY, 0, 1).ok()
                                .and_then(|c| c.reply().ok())
                                .filter(|p| p.value.len() >= 4)
                                .and_then(|p| p.value[..4].try_into().ok().map(u32::from_ne_bytes));
                            if let Some(pixmap_id) = pixmap {
                                if let Some(img) = conn.get_image(ImageFormat::Z_PIXMAP, pixmap_id, mon_x, mon_y, bar_width as u16, mon_height as u16, !0).ok().and_then(|c| c.reply().ok()) {
                                    let rgba = x11_bgrx_to_rgba(&img.data);
                                    inject_root_bg(&global, rgba, bar_width, mon_height);
                                    render_cache = RenderCache::new(30);
                                    changed = true;
                                    eprintln!("[costae] root bg updated from wallpaper change");
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        if changed {
            let key = serde_json::to_value(&stream_values).unwrap_or_default();
            bgrx = render_cache.get_or_render(&key, || {
                let t = std::time::Instant::now();
                let layout = resolve_layout(&raw_layout);
                eprintln!("[costae] resolve_layout — {}µs", t.elapsed().as_micros());
                render_frame(layout, &global, phys_bar_width, mon_height, dpr)
            });
            conn.put_image(ImageFormat::Z_PIXMAP, win_id, gc, phys_bar_width as u16, mon_height as u16, 0, 0, 0, depth, &bgrx[..])?;
            conn.flush()?;
        }

        let _ = wake_rx.recv_timeout(Duration::from_millis(50));
    }
}
