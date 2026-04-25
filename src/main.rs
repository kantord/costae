use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

use costae::{RenderCache, inject_root_bg, init_global_ctx, parse_layout, render_frame};
use costae::data::data_loop::{DataLoop, DataLoopHandle, BuiltInSource, ProcessIdentity, ProcessSource, StreamItem, StreamSource};
use costae::display_manager::DisplayManager;
use costae::x11::click::do_hit_test;
use costae::x11::panel::{sample_root_bg, i3_dpi, X11PanelContext, PanelContext};
use costae::managed_set::{ManagedSet, Reconcile};
use costae::windowing::wayland::WaylandDisplayServer;
use costae::panel::PanelSpec;
use costae::windowing::{DisplayServer, WindowEvent};
use costae::presentation::{PanelCommand, PresentationThread, Presenter, PresenterEvent};


type ModuleEventTxs = Arc<std::sync::Mutex<HashMap<String, mpsc::Sender<serde_json::Value>>>>;

const FREEZE_WATCHDOG_POLL_SECS: u64 = 10;
const FREEZE_STALE_THRESHOLD_SECS: u64 = 10;
const FILE_WATCHER_POLL_MS: u64 = 500;

fn log_lifecycle_errors<K: std::fmt::Debug, E: std::fmt::Debug>(errors: costae::managed_set::ReconcileErrors<K, E>) {
    for (key, err) in errors {
        tracing::error!(key = ?key, error = ?err, "lifecycle error");
    }
}

fn detect_backend() -> &'static str {
    if let Ok(b) = std::env::var("COSTAE_BACKEND") {
        if b == "wayland" { return "wayland"; }
        return "x11";
    }
    if std::env::var("WAYLAND_DISPLAY").is_ok() { "wayland" } else { "x11" }
}

pub(crate) fn make_wayland_mod_init(specs: &[costae::PanelSpecData]) -> serde_json::Value {
    let spec = specs.iter()
        .find(|p| p.anchor == Some(costae::PanelAnchor::Left))
        .or_else(|| specs.first());
    let (bar_w, og) = spec
        .map(|p| (p.width, p.outer_gap))
        .unwrap_or((250, 0));
    serde_json::json!({
        "type": "init",
        "config": {"width": bar_w, "outer_gap": og},
        "output": "",
        "dpi": 96.0,
    })
}

fn apply_wayland_eval_result(
    out: &costae::jsx::EvalOutput,
    handle: &DataLoopHandle,
    panels: &mut ManagedSet<PanelSpec>,
    command_tx: &mut mpsc::Sender<PanelCommand>,
) -> bool {
    let specs = match costae::parse_root_node(&out.layout) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "root node parse error");
            return false;
        }
    };
    let mod_init = make_wayland_mod_init(&specs);
    let stream_specs = stream_calls_to_specs(&out.stream_calls)
        .into_iter()
        .map(|source| match source {
            StreamSource::Process(mut p) => {
                p.props = Some(mod_init.clone());
                StreamSource::Process(p)
            }
            other => other,
        })
        .collect::<Vec<_>>();
    handle.set_desired(stream_specs);
    log_lifecycle_errors(panels.reconcile(specs.into_iter().map(PanelSpec), &mut (), command_tx));
    true
}

// ---------------------------------------------------------------------------
// X11 presenter thread
// ---------------------------------------------------------------------------

fn x11_render_all(
    presenter: &mut Presenter<X11PanelContext>,
    dm: &mut X11PanelContext,
    cache_key: &serde_json::Value,
) {
    let dpr = dm.dpr;
    let ids: Vec<String> = presenter.panels.keys().cloned().collect();
    for id in &ids {
        let Some(panel) = presenter.panels.get_mut(id) else { continue; };
        inject_root_bg(panel.root_bg_rgba.clone(), panel.phys_width, panel.phys_height);
        panel.bgrx = panel.render_cache.get_or_render(cache_key, || {
            let layout = resolve_layout(&panel.raw_layout);
            render_frame(layout, panel.phys_width, panel.phys_height, dpr)
        });
        let bgrx = Arc::clone(&panel.bgrx);
        if let Err(e) = dm.update_image(panel, &bgrx[..]) {
            tracing::error!(panel = %id, error = %e, "x11 render_all update_image failed");
        }
    }
}

fn apply_x11_cmd(
    pt: &mut PresentationThread<X11PanelContext>,
    cmd: PanelCommand,
) {
    match cmd {
        PanelCommand::RenderAll { ref cache_key } => {
            let PresentationThread { ref mut dm, ref mut presenter } = pt;
            x11_render_all(presenter, dm, cache_key);
        }
        PanelCommand::UpdateOutputMap { map } => {
            pt.dm.output_map = map;
        }
        PanelCommand::Shutdown => {} // handled by caller
        cmd => {
            let PresentationThread { ref mut dm, ref mut presenter } = pt;
            if let Err(e) = presenter.apply(cmd, dm) {
                tracing::error!(error = %e, "x11 presenter apply failed");
            }
        }
    }
}

fn run_x11_presenter_thread(
    mut pt: PresentationThread<X11PanelContext>,
    command_rx: mpsc::Receiver<PanelCommand>,
    event_tx: mpsc::Sender<PresenterEvent>,
    module_event_txs: ModuleEventTxs,
) {
    use std::sync::mpsc::RecvTimeoutError;

    'outer: loop {
        match command_rx.recv_timeout(Duration::from_millis(8)) {
            Ok(PanelCommand::Shutdown) => break 'outer,
            Ok(cmd) => apply_x11_cmd(&mut pt, cmd),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break 'outer,
        }
        loop {
            match command_rx.try_recv() {
                Ok(PanelCommand::Shutdown) => break 'outer,
                Ok(cmd) => apply_x11_cmd(&mut pt, cmd),
                Err(_) => break,
            }
        }

        let wallpaper_changed = poll_x11_events(
            &pt.dm.conn,
            &mut pt.presenter.panels,
            &module_event_txs,
            pt.dm.dpr,
            pt.dm.depth,
            pt.dm.root,
            pt.dm.xrootpmap_atom,
        ).unwrap_or(false);
        if wallpaper_changed {
            let _ = event_tx.send(PresenterEvent::NeedsRender);
        }

        let PresentationThread { ref mut dm, ref mut presenter } = pt;
        presenter.flush_pixels(dm);
    }
}

// ---------------------------------------------------------------------------
// Wayland presenter thread
// ---------------------------------------------------------------------------

fn wayland_render_all(
    presenter: &mut Presenter<WaylandDisplayServer>,
    dm: &mut WaylandDisplayServer,
    _cache_key: &serde_json::Value,
) {
    let ids: Vec<String> = presenter.panels.iter()
        .filter(|(_, p)| p.configured)
        .map(|(id, _)| id.clone())
        .collect();
    for id in &ids {
        let Some(panel) = presenter.panels.get_mut(id) else { continue; };
        let layout = panel.raw_layout.as_ref().and_then(|l| {
            parse_layout(l).map_err(|e| tracing::error!(error = %e, "layout parse error")).ok()
        });
        let bgrx = render_frame(layout, panel.width, panel.height, 1.0);
        if let Err(e) = dm.update_image(panel, &bgrx[..]) {
            tracing::error!(panel = %id, error = %e, "wayland render_all update_image failed");
        }
    }
    dm.flush();
}

fn apply_wayland_cmd(
    pt: &mut PresentationThread<WaylandDisplayServer>,
    cmd: PanelCommand,
) {
    match cmd {
        PanelCommand::RenderAll { ref cache_key } => {
            let PresentationThread { ref mut dm, ref mut presenter } = pt;
            wayland_render_all(presenter, dm, cache_key);
        }
        PanelCommand::UpdateOutputMap { .. } => {} // X11-only
        PanelCommand::Shutdown => {}               // handled by caller
        cmd => {
            let PresentationThread { ref mut dm, ref mut presenter } = pt;
            if let Err(e) = presenter.apply(cmd, dm) {
                tracing::error!(error = %e, "wayland presenter apply failed");
            }
        }
    }
}

fn run_wayland_presenter_thread(
    mut pt: PresentationThread<WaylandDisplayServer>,
    command_rx: mpsc::Receiver<PanelCommand>,
    event_tx: mpsc::Sender<PresenterEvent>,
    module_event_txs: ModuleEventTxs,
) {
    use std::sync::mpsc::RecvTimeoutError;

    'outer: loop {
        match command_rx.recv_timeout(Duration::from_millis(8)) {
            Ok(PanelCommand::Shutdown) => break 'outer,
            Ok(cmd) => apply_wayland_cmd(&mut pt, cmd),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break 'outer,
        }
        loop {
            match command_rx.try_recv() {
                Ok(PanelCommand::Shutdown) => break 'outer,
                Ok(cmd) => apply_wayland_cmd(&mut pt, cmd),
                Err(_) => break,
            }
        }

        // Apply compositor configure events (size negotiation)
        for (surface_id, new_size) in pt.dm.take_pending_configures() {
            for panel in pt.presenter.panels.values_mut() {
                if panel.surface_id != surface_id { continue; }
                if new_size.0 > 0 { panel.width = new_size.0; }
                if new_size.1 > 0 { panel.height = new_size.1; }
                panel.configured = true;
                let _ = event_tx.send(PresenterEvent::NeedsRender);
            }
        }

        match pt.dm.dispatch() {
            Ok(events) => {
                for event in events {
                    match event {
                        WindowEvent::OutputsChanged => {
                            if let Some((w, h)) = pt.dm.primary_output_size() {
                                let _ = event_tx.send(PresenterEvent::OutputsChanged {
                                    screen_width: w,
                                    screen_height: h,
                                });
                            }
                        }
                        WindowEvent::Click { panel_id, x_logical, y_logical, .. } => {
                            if let Some(panel) = pt.presenter.panels.values()
                                .find(|p| p.surface_id.to_string() == panel_id)
                            {
                                let txs = module_event_txs.lock().unwrap();
                                do_hit_test(
                                    &panel.raw_layout, &txs,
                                    panel.width, panel.height, 1.0,
                                    x_logical, y_logical,
                                );
                            }
                        }
                    }
                }
            }
            Err(_) => {
                tracing::info!("Wayland compositor disconnected, exiting");
                std::process::exit(0);
            }
        }

        let PresentationThread { ref mut dm, ref mut presenter } = pt;
        presenter.flush_pixels(dm);
        dm.flush();
    }
}

// ---------------------------------------------------------------------------
// App — non-generic, DM lives on the presenter thread
// ---------------------------------------------------------------------------

enum AppBackend {
    X11 {
        dpr: f32,
        dpi: f32,
        output_name: String,
        screen_width_logical: u32,
        screen_height_logical: u32,
    },
    Wayland,
}

struct App {
    backend: AppBackend,
    panels: ManagedSet<PanelSpec>,
    stream_values: HashMap<(String, Option<String>), String>,
    jsx_evaluator: Option<costae::jsx::JsxEvaluator>,
    handle: DataLoopHandle,
    jsx_ctx: serde_json::Value,
    item_rx: mpsc::Receiver<((String, Option<String>), String)>,
    bin_reload_rx: mpsc::Receiver<()>,
    reload_rx: mpsc::Receiver<()>,
    layout_jsx_path: std::path::PathBuf,
    stop: Arc<AtomicBool>,
    last_tick: Arc<std::sync::atomic::AtomicU64>,
    command_tx: mpsc::Sender<PanelCommand>,
    event_rx: mpsc::Receiver<PresenterEvent>,
    presenter_thread: Option<thread::JoinHandle<()>>,
}

impl App {
    fn new_x11(
        x11: X11Init,
        handle: DataLoopHandle,
        rx: TickReceivers,
        layout_jsx_path: std::path::PathBuf,
        module_event_txs: ModuleEventTxs,
        stop: Arc<AtomicBool>,
        last_tick: Arc<std::sync::atomic::AtomicU64>,
    ) -> Self {
        let X11Init { panel_ctx, jsx_ctx } = x11;
        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let backend = AppBackend::X11 {
            dpr: panel_ctx.dpr,
            dpi: panel_ctx.dpi,
            output_name: panel_ctx.output_name.clone(),
            screen_width_logical: panel_ctx.screen_width_logical,
            screen_height_logical: panel_ctx.screen_height_logical,
        };
        let pt = PresentationThread::new(panel_ctx);
        let module_event_txs_clone = Arc::clone(&module_event_txs);
        let presenter_thread = thread::spawn(move || {
            run_x11_presenter_thread(pt, command_rx, event_tx, module_event_txs_clone);
        });
        let mut state = Self {
            backend,
            panels: ManagedSet::new(),
            stream_values: HashMap::new(),
            jsx_evaluator: None,
            handle,
            jsx_ctx,
            item_rx: rx.item_rx,
            bin_reload_rx: rx.bin_reload_rx,
            reload_rx: rx.reload_rx,
            layout_jsx_path,
            stop,
            last_tick,
            command_tx,
            event_rx,
            presenter_thread: Some(presenter_thread),
        };
        state.initial_load();
        state
    }

    fn new_wayland(
        server: WaylandDisplayServer,
        handle: DataLoopHandle,
        rx: TickReceivers,
        layout_jsx_path: std::path::PathBuf,
        module_event_txs: ModuleEventTxs,
        stop: Arc<AtomicBool>,
        last_tick: Arc<std::sync::atomic::AtomicU64>,
    ) -> Self {
        let (screen_width, screen_height) = server.primary_output_size().unwrap_or((1920, 1080));
        let jsx_ctx = serde_json::json!({
            "output": "wayland",
            "dpi": 96.0,
            "screen_width": screen_width,
            "screen_height": screen_height,
        });
        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let pt = PresentationThread::new(server);
        let module_event_txs_clone = Arc::clone(&module_event_txs);
        let presenter_thread = thread::spawn(move || {
            run_wayland_presenter_thread(pt, command_rx, event_tx, module_event_txs_clone);
        });
        let mut state = Self {
            backend: AppBackend::Wayland,
            panels: ManagedSet::new(),
            stream_values: HashMap::new(),
            jsx_evaluator: None,
            handle,
            jsx_ctx,
            item_rx: rx.item_rx,
            bin_reload_rx: rx.bin_reload_rx,
            reload_rx: rx.reload_rx,
            layout_jsx_path,
            stop,
            last_tick,
            command_tx,
            event_rx,
            presenter_thread: Some(presenter_thread),
        };
        init_global_ctx();
        state.initial_load();
        state
    }

    fn apply_eval_result_dispatch(&mut self, out: &costae::jsx::EvalOutput) -> bool {
        if matches!(self.backend, AppBackend::Wayland) {
            return apply_wayland_eval_result(out, &self.handle, &mut self.panels, &mut self.command_tx);
        }
        // Extract X11 values as owned copies so the borrow of self.backend ends.
        let (dpr, dpi, output_name, sw, sh) = match &self.backend {
            AppBackend::X11 { dpr, dpi, output_name, screen_width_logical, screen_height_logical } => {
                (*dpr, *dpi, output_name.clone(), *screen_width_logical, *screen_height_logical)
            }
            AppBackend::Wayland => unreachable!(),
        };
        apply_eval_result(out, &self.handle, &mut self.panels, &mut self.command_tx,
            &move |specs| make_mod_init_value(specs, dpr, &output_name, dpi, sw, sh))
    }

    fn initial_load(&mut self) {
        if !self.layout_jsx_path.exists() { return; }
        let source = match std::fs::read_to_string(&self.layout_jsx_path) {
            Ok(s) => s,
            Err(e) => { tracing::error!(error = %e, "JSX file error"); return; }
        };
        let t = std::time::Instant::now();
        let base_dir = self.layout_jsx_path.parent().unwrap_or(&self.layout_jsx_path);
        let evaluator = match costae::jsx::JsxEvaluator::new(&source, self.jsx_ctx.clone(), Some(base_dir)) {
            Ok(e) => e,
            Err(e) => { tracing::error!(error = %e, "JSX compile error"); return; }
        };
        let eval_out = evaluator.eval(&self.stream_values);
        match eval_out {
            Ok(out) => {
                tracing::debug!(elapsed_ms = t.elapsed().as_millis(), "jsx eval");
                self.apply_eval_result_dispatch(&out);
                self.jsx_evaluator = Some(evaluator);
            }
            Err(e) => tracing::error!(error = %e, "JSX eval error"),
        }
    }

    fn handle_layout_reload(&mut self) -> bool {
        if self.reload_rx.try_recv().is_err() { return false; }

        self.handle.set_desired(vec![]);
        self.stream_values.clear();
        self.jsx_evaluator = None;
        log_lifecycle_errors(self.panels.reconcile(vec![], &mut (), &mut self.command_tx));

        if self.layout_jsx_path.exists() {
            match std::fs::read_to_string(&self.layout_jsx_path) {
                Ok(source) => {
                    let base_dir = self.layout_jsx_path.parent().unwrap_or(&self.layout_jsx_path);
                    match costae::jsx::JsxEvaluator::new(&source, self.jsx_ctx.clone(), Some(base_dir)) {
                        Ok(evaluator) => match evaluator.eval(&self.stream_values) {
                            Ok(out) => {
                                self.apply_eval_result_dispatch(&out);
                                self.jsx_evaluator = Some(evaluator);
                            }
                            Err(e) => tracing::error!(error = %e, "JSX eval error"),
                        },
                        Err(e) => tracing::error!(error = %e, "JSX compile error"),
                    }
                }
                Err(e) => tracing::error!(error = %e, "JSX file error"),
            }
        }
        tracing::info!("layout reloaded");
        true
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

        if changed {
            if matches!(self.backend, AppBackend::X11 { .. }) {
                if let Some(new_map) = rebuild_output_map_from_stream(&self.stream_values) {
                    let _ = self.command_tx.send(PanelCommand::UpdateOutputMap { map: Arc::new(new_map) });
                }
            }
            let eval_out = self.jsx_evaluator.as_ref().map(|e| {
                let t = std::time::Instant::now();
                let r = e.eval(&self.stream_values);
                tracing::debug!(elapsed_us = t.elapsed().as_micros(), "jsx re-eval");
                r
            });
            if let Some(eval_result) = eval_out {
                match eval_result {
                    Ok(out) => { if self.apply_eval_result_dispatch(&out) { needs_render = true; } }
                    Err(e) => tracing::error!(error = %e, "JSX re-eval error"),
                }
            }
        }

        if self.bin_reload_rx.try_recv().is_ok() {
            tracing::info!("binary changed, restarting...");
            self.stop.store(true, Ordering::Relaxed);
            return;
        }

        if self.handle_layout_reload() { needs_render = true; }

        while let Ok(event) = self.event_rx.try_recv() {
            match event {
                PresenterEvent::NeedsRender => needs_render = true,
                PresenterEvent::OutputsChanged { screen_width, screen_height } => {
                    self.jsx_ctx["screen_width"] = serde_json::json!(screen_width);
                    self.jsx_ctx["screen_height"] = serde_json::json!(screen_height);
                    tracing::info!(screen_width, screen_height, "Wayland output changed");
                    let eval_out = self.jsx_evaluator.as_ref().map(|e| e.eval(&self.stream_values));
                    if let Some(eval_result) = eval_out {
                        match eval_result {
                            Ok(out) => { self.apply_eval_result_dispatch(&out); needs_render = true; }
                            Err(e) => tracing::error!(error = %e, "JSX re-eval error on output change"),
                        }
                    }
                }
            }
        }

        if needs_render {
            let cache_key = serde_json::to_value(&self.stream_values).unwrap_or_default();
            let _ = self.command_tx.send(PanelCommand::RenderAll { cache_key });
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        log_lifecycle_errors(self.panels.reconcile(vec![], &mut (), &mut self.command_tx));
        let _ = self.command_tx.send(PanelCommand::Shutdown);
        if let Some(h) = self.presenter_thread.take() {
            let _ = h.join();
        }
    }
}

// ---------------------------------------------------------------------------
// X11 helpers
// ---------------------------------------------------------------------------

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
    out: &costae::jsx::EvalOutput,
    handle: &DataLoopHandle,
    panel_set: &mut ManagedSet<PanelSpec>,
    command_tx: &mut mpsc::Sender<PanelCommand>,
    mod_init_fn: &dyn Fn(&[costae::PanelSpecData]) -> serde_json::Value,
) -> bool {
    let specs = match costae::parse_root_node(&out.layout) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "root node parse error");
            return false;
        }
    };
    let mod_init = mod_init_fn(&specs);

    let module_bins: std::collections::HashSet<String> =
        out.module_calls.iter().map(|(b, _)| b.clone()).collect();
    let stream_specs = stream_calls_to_specs(&out.stream_calls)
        .into_iter()
        .filter(|s| match s {
            StreamSource::Process(p) => !module_bins.contains(&p.identity.bin),
            StreamSource::BuiltIn(_) => true,
        })
        .collect::<Vec<_>>();
    let module_specs: Vec<StreamSource> = out.module_calls.iter().map(|(bin, _)| {
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

    let panel_errors = panel_set.reconcile(
        specs.into_iter().map(PanelSpec),
        &mut (), command_tx,
    );
    log_lifecycle_errors(panel_errors);
    true
}

fn poll_x11_events(
    conn: &RustConnection,
    presenter_panels: &mut HashMap<String, costae::x11::panel::Panel>,
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
                if let Some(panel) = presenter_panels.values().find(|p| p.win_id == e.window) {
                    tracing::debug!(panel = %panel.id, win_id = panel.win_id, "expose repaint");
                    conn.put_image(ImageFormat::Z_PIXMAP, panel.win_id, panel.gc, panel.phys_width as u16, panel.phys_height as u16, 0, 0, 0, depth, &panel.bgrx[..])?;
                    conn.flush()?;
                }
            }
            x11rb::protocol::Event::ButtonPress(e) => {
                let panel_ids: Vec<u32> = presenter_panels.values().map(|p| p.win_id).collect();
                tracing::debug!(event_win = e.event, x = e.event_x, y = e.event_y, known_wins = ?panel_ids, "ButtonPress");
                if let Some(panel) = presenter_panels.values().find(|p| p.win_id == e.event) {
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
                    for panel in presenter_panels.values_mut() {
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
    panel_ctx: PanelContext,
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

    let screen_width_logical = (mon_width as f32 / dpr).round() as u32;
    let screen_height_logical = (mon_height as f32 / dpr).round() as u32;

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
        dpi,
        output_name,
        screen_width_logical,
        screen_height_logical,
    };

    let jsx_ctx = serde_json::json!({
        "output": panel_ctx.output_name,
        "dpi": dpi,
        "screen_width": screen_width_logical,
        "screen_height": screen_height_logical,
    });

    Ok(X11Init { panel_ctx, jsx_ctx })
}

fn make_mod_init_value(
    specs: &[costae::PanelSpecData],
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

    let (mut data_loop, handle) = DataLoop::new();
    data_loop = data_loop.with_extra_rx(dl_wake_rx);
    let module_event_txs = data_loop.event_txs_handle();

    let (item_tx, item_rx) = mpsc::channel::<((String, Option<String>), String)>();
    let stop = Arc::new(AtomicBool::new(false));
    let rx = TickReceivers { item_rx, bin_reload_rx, reload_rx };
    let backend = detect_backend();

    if backend == "wayland" {
        tracing::info!("display backend: Wayland");
        let server = WaylandDisplayServer::connect()?;
        let mut app = App::new_wayland(
            server, handle, rx, layout_jsx_path,
            Arc::clone(&module_event_txs),
            Arc::clone(&stop), Arc::clone(&last_tick),
        );
        data_loop.run(
            Arc::clone(&stop),
            move |item: StreamItem| { let _ = item_tx.send((item.key, item.line)); },
            move || app.tick(),
        );
    } else {
        tracing::info!("display backend: X11");
        let x11 = init_x11()?;
        let mut app = App::new_x11(
            x11, handle, rx, layout_jsx_path, module_event_txs,
            Arc::clone(&stop), Arc::clone(&last_tick),
        );
        data_loop.run(
            Arc::clone(&stop),
            move |item: StreamItem| { let _ = item_tx.send((item.key, item.line)); },
            move || app.tick(),
        );
    }

    // run() returned because stop was set (binary reload). App::drop handles cleanup.
    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new(&exe_path);
    cmd.env("COSTAE_BACKEND", backend);
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
    use super::make_wayland_mod_init;
    use costae::data::data_loop::StreamSource;

    fn left_spec(width: u32) -> costae::PanelSpecData {
        costae::PanelSpecData {
            id: "p".into(),
            width,
            height: 30,
            x: 0,
            y: 0,
            outer_gap: 0,
            above: false,
            output: None,
            anchor: Some(costae::PanelAnchor::Left),
            content: serde_json::Value::Null,
        }
    }

    /// Claim: output field must be "" (empty string), NOT "wayland" or any compositor name.
    /// This test would fail with the buggy implementation that returned "wayland".
    #[test]
    fn make_wayland_mod_init_output_is_empty_string() {
        let specs = vec![left_spec(250)];
        let result = make_wayland_mod_init(&specs);
        assert_eq!(
            result["output"].as_str(),
            Some(""),
            "output must be empty string — if it is \"wayland\", fetch_workspaces filters all workspaces",
        );
    }

    /// Claim: type field must be "init".
    #[test]
    fn make_wayland_mod_init_type_is_init() {
        let specs = vec![left_spec(250)];
        let result = make_wayland_mod_init(&specs);
        assert_eq!(
            result["type"].as_str(),
            Some("init"),
            "type must be \"init\"",
        );
    }

    /// Claim: config.width must match the width of the left-anchored spec.
    #[test]
    fn make_wayland_mod_init_config_width_matches_left_anchor_spec() {
        let specs = vec![left_spec(320)];
        let result = make_wayland_mod_init(&specs);
        assert_eq!(
            result["config"]["width"].as_u64(),
            Some(320),
            "config.width must match the left-anchored spec width",
        );
    }

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
