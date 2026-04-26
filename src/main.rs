use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

use costae::data::data_loop::{DataLoop, StreamItem};
use costae::windowing::wayland::WaylandDisplayServer;
use costae::x11::panel::{i3_dpi, PanelContext};
use costae::init_global_ctx;
use x11rb::{
    connection::Connection,
    protocol::{randr::ConnectionExt as RandrExt, xproto::*},
    rust_connection::RustConnection,
};

mod presenter;
mod app;
use app::{App, TickReceivers, X11Init};

const FREEZE_WATCHDOG_POLL_SECS: u64 = 10;
const FREEZE_STALE_THRESHOLD_SECS: u64 = 10;
const FILE_WATCHER_POLL_MS: u64 = 500;

fn detect_backend() -> &'static str {
    if let Ok(b) = std::env::var("COSTAE_BACKEND") {
        if b == "wayland" { return "wayland"; }
        return "x11";
    }
    if std::env::var("WAYLAND_DISPLAY").is_ok() { "wayland" } else { "x11" }
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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_logging();

    let log_path = {
        let home = std::env::var("HOME").unwrap_or_default();
        format!("{home}/.local/share/costae-crash.log")
    };
    install_panic_hook(log_path.clone());

    let exe_path = std::env::current_exe().unwrap_or_default();

    let home = std::env::var("HOME").unwrap_or_default();
    let layout_jsx_path = std::path::PathBuf::from(&home).join(".config/costae/layout.jsx");
    let config_yaml_path = std::path::PathBuf::from(&home).join(".config/costae/config.yaml");

    let last_tick = Arc::new(std::sync::atomic::AtomicU64::new(0));
    spawn_freeze_watchdog(Arc::clone(&last_tick), log_path);

    let (dl_wake_tx, dl_wake_rx) = mpsc::sync_channel::<()>(1);

    let (reload_tx, reload_rx) = mpsc::channel::<()>();
    spawn_layout_watcher(layout_jsx_path.clone(), reload_tx.clone(), dl_wake_tx.clone());
    let config_baseline = std::fs::metadata(&config_yaml_path).and_then(|m| m.modified()).ok();
    spawn_file_watcher(config_yaml_path.clone(), config_baseline, reload_tx, dl_wake_tx.clone());

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
            server, handle, rx, layout_jsx_path, config_yaml_path,
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
            x11, handle, rx, layout_jsx_path, config_yaml_path, module_event_txs,
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
