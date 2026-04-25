use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;

use costae::{init_global_ctx, parse_layout, render_frame};
use costae::data::data_loop::{DataLoopHandle, BuiltInSource, ProcessIdentity, ProcessSource, StreamSource};
use costae::x11::click::do_hit_test;
use costae::x11::panel::PanelContext;
use costae::managed_set::{ManagedSet, Reconcile};
use costae::windowing::wayland::WaylandDisplayServer;
use costae::panel::PanelSpec;
use costae::presentation::{PanelCommand, PanelFrame, PresentationThread, PresenterEvent};

use crate::presenter::x11::run_x11_presenter_thread;
use crate::presenter::wayland::run_wayland_presenter_thread;

pub(crate) type ModuleEventTxs = Arc<std::sync::Mutex<HashMap<String, mpsc::Sender<serde_json::Value>>>>;

fn log_lifecycle_errors<K: std::fmt::Debug, E: std::fmt::Debug>(errors: costae::managed_set::ReconcileErrors<K, E>) {
    for (key, err) in errors {
        tracing::error!(key = ?key, error = ?err, "lifecycle error");
    }
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

pub(crate) fn stream_calls_to_specs(calls: &[(String, Option<String>)]) -> Vec<StreamSource> {
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

// ---------------------------------------------------------------------------
// App — non-generic, DM lives on the presenter thread
// ---------------------------------------------------------------------------

pub(crate) struct TickReceivers {
    pub(crate) item_rx: mpsc::Receiver<((String, Option<String>), String)>,
    pub(crate) bin_reload_rx: mpsc::Receiver<()>,
    pub(crate) reload_rx: mpsc::Receiver<()>,
}

pub(crate) struct X11Init {
    pub(crate) panel_ctx: PanelContext,
    pub(crate) jsx_ctx: serde_json::Value,
}

pub(crate) struct App {
    dpr: f32,
    dpi: f32,
    output_name: String,
    screen_width_logical: u32,
    screen_height_logical: u32,
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
    module_event_txs: ModuleEventTxs,
    presenter_thread: Option<thread::JoinHandle<()>>,
}

impl App {
    pub(crate) fn new_x11(
        x11: X11Init,
        handle: DataLoopHandle,
        rx: TickReceivers,
        layout_jsx_path: std::path::PathBuf,
        module_event_txs: ModuleEventTxs,
        stop: Arc<AtomicBool>,
        last_tick: Arc<std::sync::atomic::AtomicU64>,
    ) -> Self {
        let X11Init { panel_ctx, jsx_ctx } = x11;
        let dpr = panel_ctx.dpr;
        let dpi = panel_ctx.dpi;
        let output_name = panel_ctx.output_name.clone();
        let screen_width_logical = panel_ctx.screen_width_logical;
        let screen_height_logical = panel_ctx.screen_height_logical;
        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let pt = PresentationThread::new(panel_ctx);
        let presenter_thread = thread::spawn(move || {
            run_x11_presenter_thread(pt, command_rx, event_tx);
        });
        let mut state = Self {
            dpr,
            dpi,
            output_name,
            screen_width_logical,
            screen_height_logical,
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
            module_event_txs,
            presenter_thread: Some(presenter_thread),
        };
        state.initial_load();
        state
    }

    pub(crate) fn new_wayland(
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
        let presenter_thread = thread::spawn(move || {
            run_wayland_presenter_thread(pt, command_rx, event_tx);
        });
        let mut state = Self {
            dpr: 1.0,
            dpi: 96.0,
            output_name: String::new(),
            screen_width_logical: screen_width,
            screen_height_logical: screen_height,
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
            module_event_txs,
            presenter_thread: Some(presenter_thread),
        };
        init_global_ctx();
        state.initial_load();
        state
    }

    fn apply_eval_result_dispatch(&mut self, out: &costae::jsx::EvalOutput) -> bool {
        let (dpr, dpi, sw, sh) = (self.dpr, self.dpi, self.screen_width_logical, self.screen_height_logical);
        let output_name = self.output_name.clone();
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
                self.render_panels();
            }
            Err(e) => tracing::error!(error = %e, "JSX eval error"),
        }
    }

    fn handle_layout_reload(&mut self) -> bool {
        if self.reload_rx.try_recv().is_err() { return false; }

        self.handle.set_desired(vec![]);
        self.stream_values.clear();
        self.jsx_evaluator = None;

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

    pub(crate) fn tick(&mut self) {
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
                PresenterEvent::Click { panel_id, x, y, phys_width, phys_height, dpr } => {
                    if let Some(spec) = self.panels.get(&panel_id) {
                        let raw_layout = if spec.content.is_null() { None } else { Some(spec.content.clone()) };
                        let txs = self.module_event_txs.lock().unwrap();
                        do_hit_test(&raw_layout, &txs, phys_width, phys_height, dpr, x, y);
                    }
                }
            }
        }

        if needs_render {
            self.render_panels();
        }
    }

    fn render_panels(&self) {
        let dpr = self.dpr;
        for (_, spec) in self.panels.iter() {
            if spec.content.is_null() { continue; }
            let phys_width = (spec.width as f32 * dpr) as u32;
            let phys_height = (spec.height as f32 * dpr) as u32;
            let layout = resolve_layout(&Some(spec.content.clone()));
            let pixels = render_frame(layout, phys_width, phys_height, dpr);
            let frame = PanelFrame { pixels: Arc::new(pixels), width: phys_width, height: phys_height };
            let _ = self.command_tx.send(PanelCommand::UpdatePicture { id: spec.id.clone(), frame });
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

#[cfg(test)]
mod tests {
    use super::{make_mod_init_value, stream_calls_to_specs};
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

    fn wayland_mod_init(specs: &[costae::PanelSpecData]) -> serde_json::Value {
        make_mod_init_value(specs, 1.0, "", 96.0, 0, 0)
    }

    /// Claim: output field must be "" (empty string), NOT "wayland" or any compositor name.
    /// fetch_workspaces in costae-i3 filters all workspaces when output is non-empty.
    #[test]
    fn mod_init_wayland_output_is_empty_string() {
        let result = wayland_mod_init(&[left_spec(250)]);
        assert_eq!(result["output"].as_str(), Some(""),
            "output must be empty string — if it is \"wayland\", fetch_workspaces filters all workspaces");
    }

    #[test]
    fn mod_init_type_is_init() {
        let result = wayland_mod_init(&[left_spec(250)]);
        assert_eq!(result["type"].as_str(), Some("init"));
    }

    /// Claim: config.width must match the width of the left-anchored spec (no dpr scaling at 1.0).
    #[test]
    fn mod_init_config_width_matches_left_anchor_spec() {
        let result = wayland_mod_init(&[left_spec(320)]);
        assert_eq!(result["config"]["width"].as_u64(), Some(320),
            "config.width must match the left-anchored spec width");
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
