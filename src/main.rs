use std::collections::HashMap;
use std::fmt::Debug;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

use costae::{RenderCache, inject_root_bg, init_global_ctx, parse_layout, render_frame};
use costae::data::data_loop::{DataLoop, DataLoopHandle, BuiltInSource, ProcessIdentity, ProcessSource, StreamItem, StreamSource};
use costae::x11::click::do_hit_test;
use costae::x11::panel::{sample_root_bg, i3_dpi, PanelContext};
use costae::managed_set::ManagedSet;
use costae::layout::PanelSpec;

type ModuleEventTxs = Arc<std::sync::Mutex<HashMap<String, mpsc::Sender<serde_json::Value>>>>;

const FREEZE_WATCHDOG_POLL_SECS: u64 = 10;
const FREEZE_STALE_THRESHOLD_SECS: u64 = 10;
const FILE_WATCHER_POLL_MS: u64 = 500;

fn log_lifecycle_errors<K: Debug, E: Debug>(errors: costae::managed_set::ReconcileErrors<K, E>) {
    for (key, err) in errors {
        tracing::error!(key = ?key, error = ?err, "lifecycle error");
    }
}
use x11rb::{
    connection::Connection,
    protocol::{randr::ConnectionExt as RandrExt, xproto::*},
    rust_connection::RustConnection,
};

fn rebuild_output_map_from_stream(
    stream_values: &HashMap<(String, Option<String>), String>,
) -> Option<HashMap<String, (i16, i16, u32, u32)>> {
    let json_str = stream_values.get(&("costae:outputs".to_string(), None))?;
    let outputs: Vec<serde_json::Value> = serde_json::from_str(json_str).ok()?;
    Some(outputs.iter().filter_map(|o| {
        let name = o["name"].as_str()?.to_string();
        let x = o["x"].as_i64()? as i16;
        let y = o["y"].as_i64()? as i16;
        let w = o["width"].as_u64()? as u32;
        let h = o["height"].as_u64()? as u32;
        Some((name, (x, y, w, h)))
    }).collect())
}

fn resolve_layout(raw_layout: &Option<serde_json::Value>) -> Option<takumi::layout::node::Node> {
    raw_layout.as_ref().and_then(|layout| {
        parse_layout(layout)
            .map_err(|e| tracing::error!(error = %e, "layout parse error"))
            .ok()
    })
}

fn make_builtin(key: &str) -> Option<BuiltInSource> {
    use costae::x11::outputs::outputs_thread;
    match key {
        "costae:outputs" => Some(BuiltInSource { key: key.to_string(), func: outputs_thread }),
        _ => None,
    }
}

fn stream_calls_to_specs(calls: &[(String, Option<String>)]) -> Vec<StreamSource> {
    calls.iter().map(|(bin, script)| {
        if let Some(builtin) = make_builtin(bin) {
            return StreamSource::BuiltIn(builtin);
        }
        StreamSource::Process(ProcessSource {
            identity: ProcessIdentity {
                bin: bin.clone(),
                key: format!("{}:{}", bin, script.as_deref().unwrap_or("")),
            },
            script: script.clone(),
            args: vec![],
            env: std::collections::BTreeMap::new(),
            current_dir: None,
            props: None,
        })
    }).collect()
}

fn apply_eval_result(
    value: &serde_json::Value,
    stream_calls: &[(String, Option<String>)],
    module_calls: &[(String, serde_json::Value)],
    handle: &DataLoopHandle,
    panel_set: &mut ManagedSet<PanelSpec>,
    panel_ctx: &PanelContext,
    mod_init_fn: &dyn Fn(&[costae::PanelSpec]) -> serde_json::Value,
) -> bool {
    let specs = match costae::parse_root_node(value) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "root node parse error");
            return false;
        }
    };
    let mod_init = mod_init_fn(&specs);

    let module_bins: std::collections::HashSet<String> =
        module_calls.iter().map(|(b, _)| b.clone()).collect();
    let stream_specs = stream_calls_to_specs(stream_calls)
        .into_iter()
        .filter(|s| match s {
            StreamSource::Process(p) => !module_bins.contains(&p.identity.bin),
            StreamSource::BuiltIn(_) => true,
        })
        .collect::<Vec<_>>();
    let module_specs: Vec<StreamSource> = module_calls.iter().map(|(bin, _)| {
        StreamSource::Process(ProcessSource {
            identity: ProcessIdentity { bin: bin.clone(), key: bin.clone() },
            script: None,
            args: vec![],
            env: std::collections::BTreeMap::new(),
            current_dir: None,
            props: Some(mod_init.clone()),
        })
    }).collect();
    let combined: Vec<StreamSource> = stream_specs.into_iter().chain(module_specs).collect();
    handle.set_desired(combined);

    let panel_errors = panel_set.reconcile(specs, panel_ctx);
    log_lifecycle_errors(panel_errors);
    true
}

fn poll_x11_events(
    conn: &RustConnection,
    panel_set: &mut ManagedSet<PanelSpec>,
    module_event_txs: &std::sync::Mutex<HashMap<String, mpsc::Sender<serde_json::Value>>>,
    dpr: f32,
    depth: u8,
    root: u32,
    xrootpmap_atom: Option<u32>,
) -> Result<bool, Box<dyn std::error::Error>> {
    let mut wallpaper_changed = false;
    while let Some(event) = conn.poll_for_event()? {
        match event {
            x11rb::protocol::Event::Expose(e) => {
                if let Some(panel) = panel_set.iter().find(|(_, p)| p.win_id == e.window).map(|(_, p)| p) {
                    tracing::debug!(panel = %panel.id, win_id = panel.win_id, "expose repaint");
                    conn.put_image(ImageFormat::Z_PIXMAP, panel.win_id, panel.gc, panel.phys_width as u16, panel.phys_height as u16, 0, 0, 0, depth, &panel.bgrx[..])?;
                    conn.flush()?;
                }
            }
            x11rb::protocol::Event::ButtonPress(e) => {
                let panel_ids: Vec<u32> = panel_set.iter().map(|(_, p)| p.win_id).collect();
                tracing::debug!(event_win = e.event, x = e.event_x, y = e.event_y, known_wins = ?panel_ids, "ButtonPress");
                if let Some(panel) = panel_set.iter().find(|(_, p)| p.win_id == e.event).map(|(_, p)| p) {
                    let txs = module_event_txs.lock().unwrap();
                    do_hit_test(
                        &panel.raw_layout, &txs,
                        panel.phys_width, panel.phys_height, dpr,
                        e.event_x as f32, e.event_y as f32,
                    );
                }
            }
            x11rb::protocol::Event::PropertyNotify(e) => {
                if xrootpmap_atom == Some(e.atom) {
                    for (_, panel) in panel_set.iter_mut() {
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
    handle: &DataLoopHandle,
    stream_values: &mut HashMap<(String, Option<String>), String>,
    jsx_evaluator: &mut Option<costae::jsx::JsxEvaluator>,
    layout_jsx_path: &std::path::Path,
    jsx_ctx: &serde_json::Value,
    panel_set: &mut ManagedSet<PanelSpec>,
    panel_ctx: &PanelContext,
    mod_init_fn: &dyn Fn(&[costae::PanelSpec]) -> serde_json::Value,
) -> bool {
    if reload_rx.try_recv().is_err() {
        return false;
    }
    handle.set_desired(vec![]);
    stream_values.clear();
    *jsx_evaluator = None;

    if layout_jsx_path.exists() {
        match std::fs::read_to_string(layout_jsx_path) {
            Ok(source) => match costae::jsx::JsxEvaluator::new(&source, jsx_ctx.clone()) {
                Ok(evaluator) => match evaluator.eval(stream_values) {
                    Ok((value, stream_calls, module_calls)) => {
                        apply_eval_result(
                            &value, &stream_calls, &module_calls,
                            handle, panel_set, panel_ctx,
                            mod_init_fn,
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

fn init_logging() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
}

fn install_panic_hook(log_path: String) {
    std::panic::set_hook(Box::new(move |info| {
        let msg = format!("PANIC: {info}");
        tracing::error!("{msg}");
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&log_path) {
            use std::io::Write;
            let _ = writeln!(f, "{msg}");
        }
    }));
}

fn spawn_freeze_watchdog(last_tick: Arc<std::sync::atomic::AtomicU64>, log_path: String) {
    thread::spawn(move || {
        loop {
            thread::sleep(Duration::from_secs(FREEZE_WATCHDOG_POLL_SECS));
            let last = last_tick.load(Ordering::Relaxed);
            if last == 0 { continue; }
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let stale = now.saturating_sub(last);
            if stale > FREEZE_STALE_THRESHOLD_SECS {
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

fn spawn_file_watcher(
    path: std::path::PathBuf,
    baseline: Option<std::time::SystemTime>,
    on_change_tx: mpsc::Sender<()>,
    dl_wake_tx: mpsc::SyncSender<()>,
) {
    thread::spawn(move || {
        let mut last_modified = baseline;
        loop {
            thread::sleep(Duration::from_millis(FILE_WATCHER_POLL_MS));
            let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
            if modified != last_modified {
                last_modified = modified;
                let _ = on_change_tx.send(());
                let _ = dl_wake_tx.try_send(());
            }
        }
    });
}

fn spawn_layout_watcher(
    path: std::path::PathBuf,
    reload_tx: mpsc::Sender<()>,
    dl_wake_tx: mpsc::SyncSender<()>,
) {
    let baseline = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    spawn_file_watcher(path, baseline, reload_tx, dl_wake_tx);
}

fn spawn_binary_watcher(
    path: std::path::PathBuf,
    baseline: Option<std::time::SystemTime>,
    bin_reload_tx: mpsc::Sender<()>,
    dl_wake_tx: mpsc::SyncSender<()>,
) {
    spawn_file_watcher(path, baseline, bin_reload_tx, dl_wake_tx);
}

struct X11Init {
    conn: Arc<RustConnection>,
    panel_ctx: PanelContext,
    dpr: f32,
    dpi: f32,
    output_name: String,
    screen_width_logical: u32,
    screen_height_logical: u32,
    jsx_ctx: serde_json::Value,
}

fn init_x11() -> Result<X11Init, Box<dyn std::error::Error>> {
    let (conn, screen_num) = RustConnection::connect(None)?;
    let conn = Arc::new(conn);
    let screen = conn.setup().roots[screen_num].clone();

    let dpi = i3_dpi(&conn, screen.root, &screen);
    let dpr = dpi / 96.0;

    let primary_output = conn.randr_get_output_primary(screen.root)?.reply()?.output;
    let output_info = conn.randr_get_output_info(primary_output, 0)?.reply()?;
    let output_name = String::from_utf8_lossy(&output_info.name).into_owned();
    let crtc_info = conn.randr_get_crtc_info(output_info.crtc, 0)?.reply()?;
    let mon_x = crtc_info.x;
    let mon_y = crtc_info.y;
    let mon_width = crtc_info.width as u32;
    let mon_height = crtc_info.height as u32;

    let output_map = costae::x11::outputs::build_output_map(&conn, screen.root);

    init_global_ctx();

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

    let panel_ctx = PanelContext {
        conn: Arc::clone(&conn),
        root: screen.root,
        depth: screen.root_depth,
        root_visual: screen.root_visual,
        black_pixel: screen.black_pixel,
        dpr,
        mon_x,
        mon_y,
        mon_width,
        mon_height,
        xrootpmap_atom,
        strut_atom,
        strut_legacy_atom,
        output_map: Arc::new(output_map),
    };

    let screen_width_logical = (mon_width as f32 / dpr).round() as u32;
    let screen_height_logical = (mon_height as f32 / dpr).round() as u32;

    let jsx_ctx = serde_json::json!({
        "output": output_name,
        "dpi": dpi,
        "screen_width": screen_width_logical,
        "screen_height": screen_height_logical,
    });

    Ok(X11Init { conn, panel_ctx, dpr, dpi, output_name, screen_width_logical, screen_height_logical, jsx_ctx })
}

fn make_mod_init_value(
    specs: &[costae::PanelSpec],
    dpr: f32,
    output_name: &str,
    dpi: f32,
    screen_width_logical: u32,
    screen_height_logical: u32,
) -> serde_json::Value {
    let spec = specs.iter()
        .find(|p| p.anchor == Some(costae::PanelAnchor::Left))
        .or_else(|| specs.first());
    let (bar_w, og) = spec
        .map(|p| ((p.width as f32 * dpr).round() as u32, (p.outer_gap as f32 * dpr).round() as u32))
        .unwrap_or((250, 0));
    serde_json::json!({
        "type": "init",
        "config": {"width": bar_w, "outer_gap": og},
        "output": output_name,
        "dpi": dpi,
        "screen_width": screen_width_logical,
        "screen_height": screen_height_logical,
    })
}

struct TickReceivers {
    item_rx: mpsc::Receiver<((String, Option<String>), String)>,
    bin_reload_rx: mpsc::Receiver<()>,
    reload_rx: mpsc::Receiver<()>,
}

struct TickState {
    stream_values: HashMap<(String, Option<String>), String>,
    jsx_evaluator: Option<costae::jsx::JsxEvaluator>,
    panel_set: ManagedSet<PanelSpec>,
    panel_ctx: PanelContext,
    handle: DataLoopHandle,
    jsx_ctx: serde_json::Value,
    item_rx: mpsc::Receiver<((String, Option<String>), String)>,
    bin_reload_rx: mpsc::Receiver<()>,
    reload_rx: mpsc::Receiver<()>,
    layout_jsx_path: std::path::PathBuf,
    conn: Arc<RustConnection>,
    module_event_txs: ModuleEventTxs,
    dpr: f32,
    dpi: f32,
    output_name: String,
    screen_width_logical: u32,
    screen_height_logical: u32,
    stop: Arc<AtomicBool>,
    last_tick: Arc<std::sync::atomic::AtomicU64>,
}

impl TickState {
    fn new(
        x11: X11Init,
        handle: DataLoopHandle,
        rx: TickReceivers,
        layout_jsx_path: std::path::PathBuf,
        module_event_txs: ModuleEventTxs,
        stop: Arc<AtomicBool>,
        last_tick: Arc<std::sync::atomic::AtomicU64>,
    ) -> Self {
        let X11Init { conn, panel_ctx, dpr, dpi, output_name, screen_width_logical, screen_height_logical, jsx_ctx } = x11;
        let mut state = Self {
            stream_values: HashMap::new(),
            jsx_evaluator: None,
            panel_set: ManagedSet::new(),
            panel_ctx,
            handle,
            jsx_ctx,
            item_rx: rx.item_rx,
            bin_reload_rx: rx.bin_reload_rx,
            reload_rx: rx.reload_rx,
            layout_jsx_path,
            conn,
            module_event_txs,
            dpr,
            dpi,
            output_name,
            screen_width_logical,
            screen_height_logical,
            stop,
            last_tick,
        };
        state.initial_load();
        state
    }

    fn initial_load(&mut self) {
        if !self.layout_jsx_path.exists() { return; }
        let source = match std::fs::read_to_string(&self.layout_jsx_path) {
            Ok(s) => s,
            Err(e) => { tracing::error!(error = %e, "JSX file error"); return; }
        };
        let t = std::time::Instant::now();
        let evaluator = match costae::jsx::JsxEvaluator::new(&source, self.jsx_ctx.clone()) {
            Ok(e) => e,
            Err(e) => { tracing::error!(error = %e, "JSX compile error"); return; }
        };
        match evaluator.eval(&self.stream_values) {
            Ok((value, stream_calls, module_calls)) => {
                tracing::debug!(elapsed_ms = t.elapsed().as_millis(), "jsx eval");
                let (dpr, dpi, sw, sh) = (self.dpr, self.dpi, self.screen_width_logical, self.screen_height_logical);
                let output_name = self.output_name.clone();
                let make_mod_init = |specs: &[costae::PanelSpec]| make_mod_init_value(specs, dpr, &output_name, dpi, sw, sh);
                apply_eval_result(&value, &stream_calls, &module_calls, &self.handle, &mut self.panel_set, &self.panel_ctx, &make_mod_init);
                self.jsx_evaluator = Some(evaluator);
            }
            Err(e) => tracing::error!(error = %e, "JSX eval error"),
        }
    }

    fn tick(&mut self) {
        self.last_tick.store(
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs(),
            Ordering::Relaxed,
        );

        let mut needs_render = false;

        let mut changed = false;
        while let Ok((key, value)) = self.item_rx.try_recv() {
            if self.stream_values.get(&key).map(|s| s.as_str()) != Some(value.as_str()) {
                self.stream_values.insert(key, value);
                changed = true;
            }
        }

        let (dpr, dpi, sw, sh) = (self.dpr, self.dpi, self.screen_width_logical, self.screen_height_logical);
        let output_name = self.output_name.clone();
        let make_mod_init = |specs: &[costae::PanelSpec]| make_mod_init_value(specs, dpr, &output_name, dpi, sw, sh);

        if changed {
            if let Some(new_map) = rebuild_output_map_from_stream(&self.stream_values) {
                self.panel_ctx.output_map = Arc::new(new_map);
            }
            if let Some(ref evaluator) = self.jsx_evaluator {
                let t = std::time::Instant::now();
                match evaluator.eval(&self.stream_values) {
                    Ok((new_value, new_calls, new_module_calls)) => {
                        tracing::debug!(elapsed_us = t.elapsed().as_micros(), "jsx re-eval");
                        if apply_eval_result(&new_value, &new_calls, &new_module_calls, &self.handle, &mut self.panel_set, &self.panel_ctx, &make_mod_init) {
                            needs_render = true;
                        }
                    }
                    Err(e) => tracing::error!(error = %e, "JSX re-eval error"),
                }
            }
        }

        if self.bin_reload_rx.try_recv().is_ok() {
            tracing::info!("binary changed, restarting...");
            self.stop.store(true, Ordering::Relaxed);
            return;
        }

        if handle_layout_reload(
            &self.reload_rx, &self.handle, &mut self.stream_values,
            &mut self.jsx_evaluator, &self.layout_jsx_path, &self.jsx_ctx,
            &mut self.panel_set, &self.panel_ctx,
            &make_mod_init,
        ) {
            needs_render = true;
        }

        match poll_x11_events(
            &self.conn, &mut self.panel_set, &self.module_event_txs,
            self.dpr, self.panel_ctx.depth, self.panel_ctx.root, self.panel_ctx.xrootpmap_atom,
        ) {
            Ok(wallpaper_changed) => { if wallpaper_changed { needs_render = true; } }
            Err(e) => tracing::error!(error = %e, "X11 event error"),
        }

        if needs_render {
            let key = serde_json::to_value(&self.stream_values).unwrap_or_default();
            for (_, panel) in self.panel_set.iter_mut() {
                inject_root_bg(panel.root_bg_rgba.clone(), panel.phys_width, panel.phys_height);
                panel.bgrx = panel.render_cache.get_or_render(&key, || {
                    let t = std::time::Instant::now();
                    let layout = resolve_layout(&panel.raw_layout);
                    tracing::debug!(elapsed_us = t.elapsed().as_micros(), "resolve_layout");
                    render_frame(layout, panel.phys_width, panel.phys_height, self.dpr)
                });
                tracing::debug!(panel = %panel.id, win_id = panel.win_id, "put_image");
                if let Err(e) = self.conn.put_image(ImageFormat::Z_PIXMAP, panel.win_id, panel.gc, panel.phys_width as u16, panel.phys_height as u16, 0, 0, 0, self.panel_ctx.depth, &panel.bgrx[..]) {
                    tracing::error!(error = %e, "put_image failed");
                }
            }
            if let Err(e) = self.conn.flush() {
                tracing::error!(error = %e, "flush failed");
            }
            tracing::debug!("flush ok");
        }
    }
}

impl Drop for TickState {
    fn drop(&mut self) {
        let panel_errors = self.panel_set.reconcile(vec![], &self.panel_ctx);
        log_lifecycle_errors(panel_errors);
        let _ = self.conn.flush();
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_logging();

    let log_path = {
        let home = std::env::var("HOME").unwrap_or_default();
        format!("{home}/.local/share/costae-crash.log")
    };
    install_panic_hook(log_path.clone());

    let exe_path = std::env::current_exe().unwrap_or_default();

    let layout_jsx_path = {
        let home = std::env::var("HOME").unwrap_or_default();
        std::path::PathBuf::from(home).join(".config/costae/layout.jsx")
    };

    let last_tick = Arc::new(std::sync::atomic::AtomicU64::new(0));
    spawn_freeze_watchdog(Arc::clone(&last_tick), log_path);

    // Shared wake channel: file watchers and bi-streams ping this to interrupt
    // DataLoop's recv_timeout early.
    let (dl_wake_tx, dl_wake_rx) = mpsc::sync_channel::<()>(1);

    let (reload_tx, reload_rx) = mpsc::channel::<()>();
    spawn_layout_watcher(layout_jsx_path.clone(), reload_tx, dl_wake_tx.clone());

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
        spawn_binary_watcher(exe_path.clone(), exe_baseline, bin_reload_tx, dl_wake_tx.clone());
    }

    // DataLoop manages all subprocesses (string streams and modules).
    let (mut data_loop, handle) = DataLoop::new();
    data_loop = data_loop.with_extra_rx(dl_wake_rx);
    let module_event_txs = data_loop.event_txs_handle();

    let x11 = init_x11()?;

    // Cross-closure channel: on_item sends key/value here, on_tick processes them.
    let (item_tx, item_rx) = mpsc::channel::<((String, Option<String>), String)>();
    let stop = Arc::new(AtomicBool::new(false));

    let mut tick_state = TickState::new(
        x11, handle,
        TickReceivers { item_rx, bin_reload_rx, reload_rx },
        layout_jsx_path, module_event_txs,
        Arc::clone(&stop), Arc::clone(&last_tick),
    );

    data_loop.run(
        Arc::clone(&stop),
        move |item: StreamItem| { let _ = item_tx.send((item.key, item.line)); },
        move || tick_state.tick(),
    );

    // run() returned because stop was set (binary reload). TickState::drop handles cleanup.
    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new(&exe_path);
    if let Ok(mtime) = std::fs::metadata(&exe_path).and_then(|m| m.modified()) {
        if let Ok(dur) = mtime.duration_since(std::time::UNIX_EPOCH) {
            cmd.env("COSTAE_EXE_MTIME_NS", dur.as_nanos().to_string());
        }
    }
    let _ = cmd.exec();

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::stream_calls_to_specs;
    use costae::data::data_loop::StreamSource;

    #[test]
    fn stream_calls_to_specs_maps_calls_to_process_sources() {
        let calls = vec![
            ("bash".to_string(), None),
            ("python".to_string(), Some("print('hi')".to_string())),
        ];
        let specs = stream_calls_to_specs(&calls);
        assert_eq!(specs.len(), 2);
        let StreamSource::Process(ref s0) = specs[0] else { panic!("expected Process") };
        assert_eq!(s0.identity.bin, "bash");
        assert_eq!(s0.script, None);
        let StreamSource::Process(ref s1) = specs[1] else { panic!("expected Process") };
        assert_eq!(s1.identity.bin, "python");
        assert_eq!(s1.script, Some("print('hi')".to_string()));
    }

    #[test]
    fn stream_calls_to_specs_routes_costae_prefix_to_builtin() {
        let calls = vec![("costae:outputs".to_string(), None)];
        let specs = stream_calls_to_specs(&calls);
        assert_eq!(specs.len(), 1);
        assert!(matches!(specs[0], StreamSource::BuiltIn(_)), "costae: prefix must map to BuiltIn");
    }
}
