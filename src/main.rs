use std::collections::HashMap;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use std::sync::Arc;

use costae::{GlobalContext, RenderCache, find_modules, hit_test, load_fonts, parse_layout, preload_layout_images, render_frame, spawn_module, substitute, update_module_value};
use takumi::{layout::Viewport, rendering::{RenderOptions, measure_layout}};
use x11rb::{
    connection::Connection,
    protocol::{randr::ConnectionExt as RandrExt, xproto::*},
    rust_connection::RustConnection,
};

const DEFAULT_BAR_WIDTH: u32 = 300;

fn resolve_layout(
    raw_layout: &Option<serde_json::Value>,
    module_values: &HashMap<String, serde_json::Value>,
    module_paths: &[String],
) -> Option<takumi::layout::node::Node> {
    if !module_paths.iter().all(|p| module_values.contains_key(p)) {
        return None;
    }
    raw_layout.as_ref().and_then(|layout| {
        let substituted = substitute(layout, module_values);
        parse_layout(&substituted)
            .map_err(|e| eprintln!("[costae] layout parse error: {e}"))
            .ok()
    })
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
    module_values: &HashMap<String, serde_json::Value>,
    module_paths: &[String],
    module_event_txs: &HashMap<String, mpsc::Sender<serde_json::Value>>,
    global: &GlobalContext,
    bar_width: u32,
    mon_height: u32,
    click_x: f32,
    click_y: f32,
) {
    let layout_json = match raw_layout {
        Some(l) => l,
        None => return,
    };
    let node = match resolve_layout(raw_layout, module_values, module_paths) {
        Some(n) => n,
        None => return,
    };
    let substituted = substitute(layout_json, module_values);

    let options = RenderOptions::builder()
        .global(global)
        .viewport(Viewport::new((Some(bar_width), Some(mon_height))))
        .node(node)
        .build();
    let measured = match measure_layout(options) {
        Ok(m) => m,
        Err(_) => return,
    };

    let (hit_path, on_click) = match hit_test(&measured, &substituted, click_x, click_y) {
        Some(r) => r,
        None => return,
    };

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
    let (bar_width, mut raw_layout) = if config_path.exists() {
        match costae::load_config(&config_path) {
            Ok(cfg) => {
                eprintln!("[costae] config loaded: width={}", cfg.config.width);
                (cfg.config.width, Some(cfg.layout))
            }
            Err(e) => {
                eprintln!("[costae] config error: {e}, using defaults");
                (DEFAULT_BAR_WIDTH, None)
            }
        }
    } else {
        (DEFAULT_BAR_WIDTH, None)
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

    let mut module_values: HashMap<String, serde_json::Value> = HashMap::new();
    let (module_tx, module_rx) = mpsc::channel::<(String, String)>();
    let mut module_children: Vec<std::process::Child> = Vec::new();
    let mut module_event_txs: HashMap<String, mpsc::Sender<serde_json::Value>> = HashMap::new();
    let mut module_paths: Vec<String> = Vec::new();

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

    let win_id = conn.generate_id()?;
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        win_id,
        screen.root,
        mon_x,
        mon_y,
        bar_width as u16,
        mon_height as u16,
        0,
        WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        &CreateWindowAux::new()
            .background_pixel(screen.black_pixel)
            .override_redirect(1)
            .event_mask(EventMask::EXPOSURE | EventMask::BUTTON_PRESS),
    )?;
    conn.map_window(win_id)?;
    conn.configure_window(win_id, &ConfigureWindowAux::new().stack_mode(StackMode::BELOW))?;
    conn.flush()?;

    let mut global = GlobalContext::default();
    load_fonts(&mut global);
    if let Some(ref layout) = raw_layout {
        preload_layout_images(layout, &global);
    }

    let gc = conn.generate_id()?;
    conn.create_gc(gc, win_id, &CreateGCAux::new())?;

    let init_event = serde_json::json!({
        "type": "init",
        "config": {"width": bar_width},
        "output": output_name,
        "dpi": dpi
    });

    let spawn_all_modules = |layout: &serde_json::Value,
                              tx: &mpsc::Sender<(String, String)>,
                              wake_tx: &mpsc::SyncSender<()>,
                              init_event: &serde_json::Value,
                              children: &mut Vec<std::process::Child>,
                              event_txs: &mut HashMap<String, mpsc::Sender<serde_json::Value>>,
                              paths: &mut Vec<String>| {
        paths.clear();
        for m in find_modules(layout) {
            let tx = tx.clone();
            let wake_tx = wake_tx.clone();
            let path = m.path.clone();
            paths.push(path.clone());
            let module = spawn_module(&m.bin, m.script.as_deref());
            module.send_event(init_event);
            event_txs.insert(path.clone(), module.event_tx.clone());
            children.push(module.child);
            let rx = module.rx;
            thread::spawn(move || {
                while let Ok(line) = rx.recv() {
                    if tx.send((path.clone(), line)).is_err() {
                        break;
                    }
                    let _ = wake_tx.try_send(());
                }
            });
        }
    };

    if let Some(ref layout) = raw_layout {
        spawn_all_modules(layout, &module_tx, &wake_tx, &init_event, &mut module_children, &mut module_event_txs, &mut module_paths);
    }

    let mut render_cache = RenderCache::new(30);
    let mut bgrx: Arc<Vec<u8>> = render_cache.get_or_render(
        &serde_json::to_value(&module_values).unwrap_or_default(),
        || render_frame(resolve_layout(&raw_layout, &module_values, &module_paths), &global, bar_width, mon_height),
    );

    loop {
        let mut changed = false;

        if bin_reload_rx.try_recv().is_ok() {
            eprintln!("[costae] binary changed, restarting...");
            for child in &mut module_children {
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
            for child in &mut module_children {
                let _ = child.kill();
                let _ = child.wait();
            }
            module_children.clear();
            module_event_txs.clear();
            module_values.clear();
            render_cache = RenderCache::new(30);
            if let Ok(cfg) = costae::load_config(&config_path) {
                raw_layout = Some(cfg.layout);
            }
            if let Some(ref layout) = raw_layout {
                preload_layout_images(layout, &global);
                spawn_all_modules(layout, &module_tx, &wake_tx, &init_event, &mut module_children, &mut module_event_txs, &mut module_paths);
            }
            eprintln!("[costae] config reloaded");
            changed = true;
        }

        while let Ok((path, line)) = module_rx.try_recv() {
            let value = serde_json::from_str(&line)
                .unwrap_or(serde_json::Value::String(line));
            if update_module_value(&mut module_values, path, value) {
                changed = true;
            }
        }

        if changed {
            let key = serde_json::to_value(&module_values).unwrap_or_default();
            bgrx = render_cache.get_or_render(&key, || {
                render_frame(resolve_layout(&raw_layout, &module_values, &module_paths), &global, bar_width, mon_height)
            });
            conn.put_image(ImageFormat::Z_PIXMAP, win_id, gc, bar_width as u16, mon_height as u16, 0, 0, 0, depth, &bgrx[..])?;
            conn.flush()?;
        }

        while let Some(event) = conn.poll_for_event()? {
            match event {
                x11rb::protocol::Event::Expose(_) => {
                    conn.put_image(ImageFormat::Z_PIXMAP, win_id, gc, bar_width as u16, mon_height as u16, 0, 0, 0, depth, &bgrx[..])?;
                    conn.flush()?;
                }
                x11rb::protocol::Event::ButtonPress(e) => {
                    do_hit_test(
                        &raw_layout, &module_values, &module_paths, &module_event_txs,
                        &global, bar_width, mon_height,
                        e.event_x as f32, e.event_y as f32,
                    );
                }
                _ => {}
            }
        }

        let _ = wake_rx.recv_timeout(Duration::from_millis(50));
    }
}
