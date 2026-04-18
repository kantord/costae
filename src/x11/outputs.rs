use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

use x11rb::connection::Connection;
use x11rb::protocol::randr::{self, ConnectionExt as RandrExt};
use x11rb::rust_connection::RustConnection;

use crate::data::data_loop::{StreamItem, StreamKind};
use super::panel::i3_dpi;

pub fn build_output_map(conn: &RustConnection, root: u32) -> HashMap<String, (i16, i16, u32, u32)> {
    let mut map = HashMap::new();
    if let Ok(cookie) = conn.randr_get_screen_resources_current(root) {
        if let Ok(resources) = cookie.reply() {
            for &out_id in &resources.outputs {
                if let Ok(info_cookie) = conn.randr_get_output_info(out_id, 0) {
                    if let Ok(info) = info_cookie.reply() {
                        if info.crtc == 0 { continue; }
                        if let Ok(crtc_cookie) = conn.randr_get_crtc_info(info.crtc, 0) {
                            if let Ok(crtc) = crtc_cookie.reply() {
                                let name = String::from_utf8_lossy(&info.name).into_owned();
                                map.insert(name, (crtc.x, crtc.y, crtc.width as u32, crtc.height as u32));
                            }
                        }
                    }
                }
            }
        }
    }
    map
}

fn emit_outputs(conn: &RustConnection, root: u32, dpr: f32, key: &str, tx: &mpsc::Sender<StreamItem>) {
    let map = build_output_map(conn, root);
    let outputs: Vec<serde_json::Value> = map.iter().map(|(name, &(x, y, w, h))| {
        serde_json::json!({
            "name": name,
            "x": x,
            "y": y,
            "width": w,
            "height": h,
            "screen_width":  (w as f32 / dpr).round() as u32,
            "screen_height": (h as f32 / dpr).round() as u32,
        })
    }).collect();
    let line = serde_json::to_string(&outputs).unwrap_or_default();
    let _ = tx.send(StreamItem { key: (key.to_string(), None), stream: StreamKind::Stdout, line });
}

pub fn outputs_thread(tx: mpsc::Sender<StreamItem>, key: String, stop: Arc<AtomicBool>) {
    let (conn, screen_num) = match RustConnection::connect(None) {
        Ok(c) => c,
        Err(e) => { tracing::error!(error = %e, "outputs_thread: X11 connect failed"); return; }
    };
    let screen = conn.setup().roots[screen_num].clone();
    let root = screen.root;
    let dpr = i3_dpi(&conn, root, &screen) / 96.0;

    if let Err(e) = conn.randr_select_input(root, randr::NotifyMask::SCREEN_CHANGE) {
        tracing::error!(error = %e, "outputs_thread: randr_select_input failed");
        return;
    }
    let _ = conn.flush();

    emit_outputs(&conn, root, dpr, &key, &tx);

    loop {
        if stop.load(Ordering::Relaxed) { break; }
        match conn.poll_for_event() {
            Ok(Some(event)) => {
                if matches!(event, x11rb::protocol::Event::RandrScreenChangeNotify(_)) {
                    emit_outputs(&conn, root, dpr, &key, &tx);
                }
            }
            Ok(None) => { thread::sleep(Duration::from_millis(50)); }
            Err(e) => { tracing::error!(error = %e, "outputs_thread: X11 error"); break; }
        }
    }
}
