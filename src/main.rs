use std::collections::HashMap;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use costae::{GlobalContext, RenderCache, inject_root_bg, load_fonts, parse_layout, preload_layout_images, reconcile_panels, reconcile_streams, render_frame, spawn_bi_stream, spawn_string_stream};
use costae::x11::panel::{Panel, X11Context, create_panel, destroy_panel, sample_root_bg, i3_dpi, do_hit_test};
use x11rb::{
    connection::Connection,
    protocol::{randr::ConnectionExt as RandrExt, xproto::*},
    rust_connection::RustConnection,
};

fn resolve_layout(raw_layout: &Option<serde_json::Value>) -> Option<takumi::layout::node::Node> {
    raw_layout.as_ref().and_then(|layout| {
        parse_layout(layout)
            .map_err(|e| tracing::error!(error = %e, "layout parse error"))
            .ok()
    })
}

/// Tracks all running child processes and their communication channels.
struct StreamRegistry {
    stream_children: HashMap<(String, Option<String>), std::process::Child>,
    bi_stream_children: HashMap<String, std::process::Child>,
    module_event_txs: HashMap<String, mpsc::Sender<serde_json::Value>>,
}

impl StreamRegistry {
    fn new() -> Self {
        Self {
            stream_children: HashMap::new(),
            bi_stream_children: HashMap::new(),
            module_event_txs: HashMap::new(),
        }
    }
}

/// Apply the result of a JSX evaluation: parse specs, reconcile streams, spawn/kill children,
/// and update panels.
///
/// `current_calls` should be `&[]` on a fresh load (blocks 1 & 2) so that all stream_calls are
/// treated as new spawns. On a re-eval (block 3) pass the currently running keys so that removed
/// streams are killed.
///
/// Returns `true` on success, `false` if `parse_root_node` fails.
#[allow(clippy::too_many_arguments)]
fn apply_eval_result(
    value: &serde_json::Value,
    stream_calls: &[(String, Option<String>)],
    module_calls: &[String],
    current_calls: &[(String, Option<String>)],
    registry: &mut StreamRegistry,
    panels: &mut Vec<Panel>,
    stream_tx: &mpsc::Sender<(String, Option<String>, String)>,
    wake_tx: &mpsc::SyncSender<()>,
    mod_init_fn: &dyn Fn(&[costae::PanelSpec]) -> serde_json::Value,
    apply_panel_specs_fn: &mut dyn FnMut(&mut Vec<Panel>, Vec<costae::PanelSpec>),
) -> bool {
    let specs = match costae::parse_root_node(value) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "root node parse error");
            return false;
        }
    };
    let mod_init = mod_init_fn(&specs);

    let (to_spawn, to_kill) = reconcile_streams(current_calls, stream_calls);
    for (b, s) in to_kill {
        if let Some(mut child) = registry.stream_children.remove(&(b, s)) {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
    for (bin, script) in to_spawn {
        let child = spawn_string_stream(&bin, script.as_deref(), stream_tx.clone(), wake_tx.clone());
        registry.stream_children.insert((bin, script), child);
    }
    for bin in module_calls {
        if !registry.bi_stream_children.contains_key(bin) {
            let bi = spawn_bi_stream(bin, &mod_init, stream_tx.clone(), wake_tx.clone());
            registry.module_event_txs.insert(bin.clone(), bi.event_tx);
            registry.bi_stream_children.insert(bin.clone(), bi.child);
        }
    }

    apply_panel_specs_fn(panels, specs);
    true
}

fn drain_stream_updates(
    rx: &mpsc::Receiver<(String, Option<String>, String)>,
    values: &mut HashMap<(String, Option<String>), String>,
) -> bool {
    let mut changed = false;
    while let Ok((bin, script, value)) = rx.try_recv() {
        let key = (bin, script);
        if values.get(&key).map(|s| s.as_str()) != Some(value.as_str()) {
            values.insert(key, value);
            changed = true;
        }
    }
    changed
}

#[allow(clippy::too_many_arguments)]
fn handle_x11_events(
    conn: &RustConnection,
    panels: &mut [Panel],
    module_event_txs: &HashMap<String, mpsc::Sender<serde_json::Value>>,
    global: &GlobalContext,
    dpr: f32,
    depth: u8,
    root: u32,
    xrootpmap_atom: Option<u32>,
) -> Result<bool, Box<dyn std::error::Error>> {
    let mut wallpaper_changed = false;
    while let Some(event) = conn.poll_for_event()? {
        match event {
            x11rb::protocol::Event::Expose(e) => {
                if let Some(panel) = panels.iter().find(|p| p.win_id == e.window) {
                    tracing::debug!(panel = %panel.id, win_id = panel.win_id, "expose repaint");
                    conn.put_image(ImageFormat::Z_PIXMAP, panel.win_id, panel.gc, panel.phys_width as u16, panel.phys_height as u16, 0, 0, 0, depth, &panel.bgrx[..])?;
                    conn.flush()?;
                }
            }
            x11rb::protocol::Event::ButtonPress(e) => {
                if let Some(panel) = panels.iter().find(|p| p.win_id == e.event) {
                    do_hit_test(
                        &panel.raw_layout, module_event_txs,
                        global, panel.phys_width, panel.phys_height, dpr,
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
                        if let Some(rgba) = sample_root_bg(conn, root, panel.win_x, panel.win_y, panel.phys_width, panel.phys_height, xrootpmap_atom) {
                            panel.root_bg_rgba = rgba;
                            tracing::debug!(panel = %panel.id, "root bg updated");
                        }
                        panel.render_cache = RenderCache::new(30);
                    }
                    wallpaper_changed = true;
                }
            }
            x11rb::protocol::Event::Error(e) => {
                tracing::error!(error = ?e, "X11 async error");
            }
            _ => {}
        }
    }
    Ok(wallpaper_changed)
}

#[allow(clippy::too_many_arguments)]
fn handle_layout_reload(
    reload_rx: &mpsc::Receiver<()>,
    registry: &mut StreamRegistry,
    stream_values: &mut HashMap<(String, Option<String>), String>,
    jsx_evaluator: &mut Option<costae::jsx::JsxEvaluator>,
    layout_jsx_path: &std::path::Path,
    jsx_ctx: &serde_json::Value,
    panels: &mut Vec<Panel>,
    stream_tx: &mpsc::Sender<(String, Option<String>, String)>,
    wake_tx: &mpsc::SyncSender<()>,
    mod_init_fn: &dyn Fn(&[costae::PanelSpec]) -> serde_json::Value,
    apply_panel_specs_fn: &mut dyn FnMut(&mut Vec<Panel>, Vec<costae::PanelSpec>),
) -> bool {
    if reload_rx.try_recv().is_err() {
        return false;
    }
    for (_, mut child) in registry.stream_children.drain() {
        let _ = child.kill();
        let _ = child.wait();
    }
    for (_, mut child) in registry.bi_stream_children.drain() {
        let _ = child.kill();
        let _ = child.wait();
    }
    registry.module_event_txs.clear();
    stream_values.clear();
    *jsx_evaluator = None;

    if layout_jsx_path.exists() {
        match std::fs::read_to_string(layout_jsx_path) {
            Ok(source) => match costae::jsx::JsxEvaluator::new(&source, jsx_ctx.clone()) {
                Ok(evaluator) => match evaluator.eval(stream_values) {
                    Ok((value, stream_calls, module_calls)) => {
                        apply_eval_result(
                            &value, &stream_calls, &module_calls,
                            &[], registry, panels,
                            stream_tx, wake_tx,
                            mod_init_fn, apply_panel_specs_fn,
                        );
                        *jsx_evaluator = Some(evaluator);
                    }
                    Err(e) => tracing::error!(error = %e, "JSX eval error"),
                },
                Err(e) => tracing::error!(error = %e, "JSX compile error"),
            },
            Err(e) => tracing::error!(error = %e, "JSX file error"),
        }
    }
    tracing::info!("layout reloaded");
    true
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env()
            .add_directive(tracing::Level::INFO.into()))
        .init();

    let log_path = {
        let home = std::env::var("HOME").unwrap_or_default();
        format!("{home}/.local/share/costae-crash.log")
    };
    {
        let log_path = log_path.clone();
        std::panic::set_hook(Box::new(move |info| {
            let msg = format!("PANIC: {info}");
            tracing::error!("{msg}");
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&log_path) {
                use std::io::Write;
                let _ = writeln!(f, "{msg}");
            }
        }));
    }

    let exe_path = std::env::current_exe().unwrap_or_default();

    let layout_jsx_path = {
        let home = std::env::var("HOME").unwrap_or_default();
        std::path::PathBuf::from(home).join(".config/costae/layout.jsx")
    };

    let last_tick = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    {
        let last_tick = std::sync::Arc::clone(&last_tick);
        thread::spawn(move || {
            loop {
                thread::sleep(Duration::from_secs(10));
                let last = last_tick.load(std::sync::atomic::Ordering::Relaxed);
                if last == 0 { continue; }
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let stale = now.saturating_sub(last);
                if stale > 10 {
                    let msg = format!("FREEZE: main loop stalled for {stale}s");
                    tracing::error!("{msg}");
                    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&log_path) {
                        use std::io::Write;
                        let _ = writeln!(f, "{msg}");
                    }
                }
            }
        });
    }

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

    let mut registry = StreamRegistry::new();
    let mut stream_values: HashMap<(String, Option<String>), String> = HashMap::new();
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
    let mut apply_panel_specs = |panels: &mut Vec<Panel>, specs: Vec<costae::PanelSpec>| {
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
                Err(e) => tracing::error!(panel = %spec.id, error = %e, "failed to create panel"),
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
                                tracing::debug!(elapsed_ms = t.elapsed().as_millis(), "jsx eval");
                                apply_eval_result(
                                    &value, &stream_calls, &module_calls,
                                    &[], &mut registry, &mut panels,
                                    &stream_tx, &wake_tx,
                                    &make_mod_init, &mut apply_panel_specs,
                                );
                                jsx_evaluator = Some(evaluator);
                            }
                            Err(e) => tracing::error!(error = %e, "JSX eval error"),
                        }
                    }
                    Err(e) => tracing::error!(error = %e, "JSX compile error"),
                }
            }
            Err(e) => tracing::error!(error = %e, "JSX file error"),
        }
    }

    loop {
        last_tick.store(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            std::sync::atomic::Ordering::Relaxed,
        );
        let mut changed = false;

        if bin_reload_rx.try_recv().is_ok() {
            tracing::info!("binary changed, restarting...");
            for child in registry.stream_children.values_mut() {
                let _ = child.kill();
                let _ = child.wait();
            }
            for child in registry.bi_stream_children.values_mut() {
                let _ = child.kill();
                let _ = child.wait();
            }
            for panel in std::mem::take(&mut panels) {
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

        if handle_layout_reload(
            &reload_rx, &mut registry, &mut stream_values, &mut jsx_evaluator,
            &layout_jsx_path, &jsx_ctx, &mut panels, &stream_tx, &wake_tx,
            &make_mod_init, &mut apply_panel_specs,
        ) {
            changed = true;
        }

        let streams_updated = drain_stream_updates(&stream_rx, &mut stream_values);
        if streams_updated {
            if let Some(ref evaluator) = jsx_evaluator {
                let t = std::time::Instant::now();
                match evaluator.eval(&stream_values) {
                    Ok((new_value, new_calls, new_module_calls)) => {
                        tracing::debug!(elapsed_us = t.elapsed().as_micros(), "jsx re-eval");
                        let current_calls: Vec<_> = registry.stream_children.keys().cloned().collect();
                        let did_apply = apply_eval_result(
                            &new_value, &new_calls, &new_module_calls,
                            &current_calls, &mut registry, &mut panels,
                            &stream_tx, &wake_tx,
                            &make_mod_init, &mut apply_panel_specs,
                        );
                        if did_apply {
                            changed = true;
                        }
                    }
                    Err(e) => tracing::error!(error = %e, "JSX re-eval error"),
                }
            }
        }

        // Process X11 events
        if handle_x11_events(
            &conn, &mut panels, &registry.module_event_txs,
            &global, dpr, depth, screen.root, xrootpmap_atom,
        )? {
            changed = true;
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
                    tracing::debug!(elapsed_us = t.elapsed().as_micros(), "resolve_layout");
                    render_frame(layout, &global, panel.phys_width, panel.phys_height, dpr)
                });
                tracing::debug!(panel = %panel.id, win_id = panel.win_id, "put_image");
                conn.put_image(ImageFormat::Z_PIXMAP, panel.win_id, panel.gc, panel.phys_width as u16, panel.phys_height as u16, 0, 0, 0, depth, &panel.bgrx[..])?;
            }
            conn.flush()?;
            tracing::debug!("flush ok");
        }

        let _ = wake_rx.recv_timeout(Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::drain_stream_updates;
    use std::collections::HashMap;
    use std::sync::mpsc;

    // helper: create a channel, send items, drop the sender, return receiver
    fn make_rx(items: Vec<(String, Option<String>, String)>) -> mpsc::Receiver<(String, Option<String>, String)> {
        let (tx, rx) = mpsc::channel();
        for item in items {
            tx.send(item).unwrap();
        }
        rx
    }

    #[test]
    fn empty_channel_returns_false() {
        let rx = make_rx(vec![]);
        let mut values: HashMap<(String, Option<String>), String> = HashMap::new();
        assert!(!drain_stream_updates(&rx, &mut values));
    }

    #[test]
    fn new_key_returns_true_and_inserts_value() {
        let rx = make_rx(vec![
            ("bin".into(), None, "hello".into()),
        ]);
        let mut values: HashMap<(String, Option<String>), String> = HashMap::new();
        assert!(drain_stream_updates(&rx, &mut values));
        assert_eq!(values.get(&("bin".to_string(), None)).map(|s| s.as_str()), Some("hello"));
    }

    #[test]
    fn same_value_on_second_drain_returns_false() {
        let (tx, rx) = mpsc::channel::<(String, Option<String>, String)>();
        let mut values: HashMap<(String, Option<String>), String> = HashMap::new();

        tx.send(("bin".into(), None, "v1".into())).unwrap();
        // first drain: value is new → true
        assert!(drain_stream_updates(&rx, &mut values));

        tx.send(("bin".into(), None, "v1".into())).unwrap();
        // second drain: same value → false
        assert!(!drain_stream_updates(&rx, &mut values));
    }

    #[test]
    fn changed_value_returns_true() {
        let (tx, rx) = mpsc::channel::<(String, Option<String>, String)>();
        let mut values: HashMap<(String, Option<String>), String> = HashMap::new();

        tx.send(("bin".into(), Some("script".into()), "v1".into())).unwrap();
        drain_stream_updates(&rx, &mut values);

        tx.send(("bin".into(), Some("script".into()), "v2".into())).unwrap();
        assert!(drain_stream_updates(&rx, &mut values));
    }

    #[test]
    fn multiple_keys_one_changes_returns_true() {
        let (tx, rx) = mpsc::channel::<(String, Option<String>, String)>();
        let mut values: HashMap<(String, Option<String>), String> = HashMap::new();

        // seed two keys
        tx.send(("bin_a".into(), None, "stable".into())).unwrap();
        tx.send(("bin_b".into(), None, "old".into())).unwrap();
        drain_stream_updates(&rx, &mut values);

        // only bin_b changes
        tx.send(("bin_a".into(), None, "stable".into())).unwrap();
        tx.send(("bin_b".into(), None, "new".into())).unwrap();
        assert!(drain_stream_updates(&rx, &mut values));
    }
}
