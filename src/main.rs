use std::borrow::Cow;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use takumi::{
    GlobalContext,
    layout::{
        Viewport,
        node::Node,
        style::{
            AlignItems, BorderStyle, Color, ColorInput, Display, FlexDirection, FontWeight,
            JustifyContent, Length::Px, Style, StyleDeclaration,
        },
    },
    rendering::{RenderOptionsBuilder, render},
};
use x11rb::{
    connection::Connection,
    protocol::{randr::ConnectionExt as RandrExt, xproto::*},
    rust_connection::RustConnection,
};

const BAR_WIDTH: u32 = 300;

// --- i3 IPC ---

const I3_MAGIC: &[u8; 6] = b"i3-ipc";

fn i3_send(s: &mut UnixStream, msg_type: u32, payload: &[u8]) -> std::io::Result<()> {
    s.write_all(I3_MAGIC)?;
    s.write_all(&(payload.len() as u32).to_le_bytes())?;
    s.write_all(&msg_type.to_le_bytes())?;
    s.write_all(payload)
}

fn i3_recv(s: &mut UnixStream) -> std::io::Result<(u32, Vec<u8>)> {
    let mut hdr = [0u8; 14]; // 6 magic + 4 len + 4 type
    s.read_exact(&mut hdr)?;
    let len = u32::from_le_bytes(hdr[6..10].try_into().unwrap()) as usize;
    let typ = u32::from_le_bytes(hdr[10..14].try_into().unwrap());
    let mut buf = vec![0u8; len];
    s.read_exact(&mut buf)?;
    Ok((typ, buf))
}

fn i3_socket_path() -> String {
    std::env::var("I3SOCK").unwrap_or_else(|_| {
        std::process::Command::new("i3")
            .arg("--get-socketpath")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default()
    })
}

// Mirror i3's DPI detection: Xft.dpi from RESOURCE_MANAGER, then physical screen dimensions.
// See libi3/dpi.c in i3 source.
fn i3_dpi(conn: &RustConnection, root: Window, screen: &Screen) -> f32 {
    // Method 1: Xft.dpi from X11 RESOURCE_MANAGER (same priority as i3)
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

    // Method 2: Calculate from screen physical dimensions (same as i3's init_dpi_fallback)
    if screen.height_in_millimeters > 0 {
        let dpi = screen.height_in_pixels as f32 * 25.4
            / screen.height_in_millimeters as f32;
        eprintln!("[costae] DPI {dpi:.1} (from screen physical dimensions)");
        return dpi;
    }

    eprintln!("[costae] DPI 96.0 (fallback)");
    96.0
}

// Apply left gap = BAR_WIDTH physical pixels to the currently focused workspace.
// i3 internally calls logical_px(N) = ceil((dpi/96) * N) on every gap value,
// so we send the inverse: floor(BAR_WIDTH * 96 / dpi).
// See libi3/dpi.c and src/commands.c in i3 source.
fn apply_bar_gap(socket: &str, dpi: f32) {
    match UnixStream::connect(socket) {
        Err(e) => eprintln!("[costae] gap: failed to connect to i3 socket: {e}"),
        Ok(mut s) => {
            // i3 only scales if dpi/96 >= 1.25 (logical_px threshold)
            let gap = if (dpi / 96.0) < 1.25 {
                BAR_WIDTH
            } else {
                (BAR_WIDTH as f32 * 96.0 / dpi).floor() as u32
            };
            let cmd = format!("gaps left current set {}", gap);
            eprintln!("[costae] gap: dpi={dpi:.1} sending '{cmd}'");
            let _ = i3_send(&mut s, 0, cmd.as_bytes()); // RUN_COMMAND = 0
            match i3_recv(&mut s) {
                Ok((_, payload)) => eprintln!("[costae] gap: reply {}", String::from_utf8_lossy(&payload)),
                Err(e) => eprintln!("[costae] gap: recv error: {e}"),
            }
        }
    }
}

#[derive(Clone)]
struct Workspace {
    name: String,
    focused: bool,
}

fn fetch_workspaces(socket: &str, output: &str) -> std::io::Result<Vec<Workspace>> {
    let mut s = UnixStream::connect(socket)?;
    i3_send(&mut s, 1, b"")?; // GET_WORKSPACES = 1
    let (_, payload) = i3_recv(&mut s)?;
    let arr: Vec<serde_json::Value> = serde_json::from_slice(&payload).unwrap_or_default();
    Ok(arr
        .iter()
        .filter(|w| w["output"].as_str().unwrap_or("") == output)
        .map(|w| Workspace {
            name: w["name"].as_str().unwrap_or("?").to_string(),
            focused: w["focused"].as_bool().unwrap_or(false),
        })
        .collect())
}

// Subscribe to workspace events; apply left gap and send updated lists on each change.
fn spawn_i3_watcher(socket: String, output: String, dpi: f32, tx: mpsc::Sender<Vec<Workspace>>) {
    thread::spawn(move || {
        let mut sub = match UnixStream::connect(&socket) {
            Ok(s) => s,
            Err(_) => return,
        };
        let _ = i3_send(&mut sub, 2, b"[\"workspace\"]"); // SUBSCRIBE = 2
        let _ = i3_recv(&mut sub); // consume success reply

        // Initial state: apply gap if focus is already on our output, then send list.
        if let Ok(ws) = fetch_workspaces(&socket, &output) {
            if ws.iter().any(|w| w.focused) {
                apply_bar_gap(&socket, dpi);
            }
            let _ = tx.send(ws);
        }

        loop {
            match i3_recv(&mut sub) {
                Ok((0x80000000, payload)) => {
                    // On focus events, apply gap if the new workspace landed on our output.
                    if let Ok(event) = serde_json::from_slice::<serde_json::Value>(&payload) {
                        if event["current"]["output"].as_str() == Some(output.as_str()) {
                            apply_bar_gap(&socket, dpi);
                        }
                    }
                    if let Ok(ws) = fetch_workspaces(&socket, &output) {
                        if tx.send(ws).is_err() {
                            break;
                        }
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    });
}

// --- Rendering ---

fn build_node(workspaces: &[Workspace], height: u32) -> Node {
    let items: Vec<Node> = workspaces
        .iter()
        .map(|ws| {
            Node::text(ws.name.clone()).with_style(
                Style::default()
                    .with(StyleDeclaration::font_size(Px(16.0).into()))
                    .with(StyleDeclaration::font_weight(FontWeight::from(
                        if ws.focused { 700.0 } else { 400.0 },
                    )))
                    .with(StyleDeclaration::color(ColorInput::Value(if ws.focused {
                        Color([203, 166, 247, 255]) // #cba6f7 — active
                    } else {
                        Color([166, 173, 200, 255]) // #a6adc8 — inactive
                    }))),
            )
        })
        .collect();

    Node::container(items).with_style(
        Style::default()
            .with(StyleDeclaration::width(Px(BAR_WIDTH as f32)))
            .with(StyleDeclaration::height(Px(height as f32)))
            .with(StyleDeclaration::background_color(ColorInput::Value(Color([
                30, 30, 46, 255, // #1e1e2e
            ]))))
            .with(StyleDeclaration::display(Display::Flex))
            .with(StyleDeclaration::flex_direction(FlexDirection::Column))
            .with(StyleDeclaration::align_items(AlignItems::Center))
            .with(StyleDeclaration::justify_content(JustifyContent::FlexStart))
            .with(StyleDeclaration::row_gap(Px(8.0)))
            .with(StyleDeclaration::padding_top(Px(16.0)))
            .with(StyleDeclaration::border_top_width(Px(1.0)))
            .with(StyleDeclaration::border_right_width(Px(1.0)))
            .with(StyleDeclaration::border_bottom_width(Px(1.0)))
            .with(StyleDeclaration::border_left_width(Px(1.0)))
            .with(StyleDeclaration::border_style(BorderStyle::Solid))
            .with(StyleDeclaration::border_color(ColorInput::Value(Color([
                0, 255, 0, 255, // #00ff00
            ])))),
    )
}

fn render_frame(workspaces: &[Workspace], global: &GlobalContext, height: u32) -> Vec<u8> {
    let node = build_node(workspaces, height);
    let options = RenderOptionsBuilder::default()
        .global(global)
        .viewport(Viewport::new(Some(BAR_WIDTH), Some(height)))
        .node(node)
        .build()
        .expect("build options");
    let rgba = render(options).expect("render").into_raw();
    let mut bgrx = Vec::with_capacity(rgba.len());
    for px in rgba.chunks_exact(4) {
        bgrx.extend_from_slice(&[px[2], px[1], px[0], 0x00]);
    }
    bgrx
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Connect to X11, get primary monitor geometry via RandR
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

    // Create borderless window on the left of the primary monitor
    let win_id = conn.generate_id()?;
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        win_id,
        screen.root,
        mon_x,
        mon_y,
        BAR_WIDTH as u16,
        mon_height as u16,
        0,
        WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        &CreateWindowAux::new()
            .background_pixel(screen.black_pixel)
            .override_redirect(1)
            .event_mask(EventMask::EXPOSURE),
    )?;
    conn.map_window(win_id)?;
    conn.flush()?;

    // Load Regular + Bold fonts
    let mut global = GlobalContext::default();
    for path in [
        "/usr/share/fonts/TTF/JetBrainsMono-Regular.ttf",
        "/usr/share/fonts/TTF/JetBrainsMono-Bold.ttf",
    ] {
        if let Ok(bytes) = std::fs::read(path) {
            global.font_context.load_and_store(Cow::from(bytes), None, None)?;
        }
    }

    let gc = conn.generate_id()?;
    conn.create_gc(gc, win_id, &CreateGCAux::new())?;

    // Start i3 workspace watcher (also manages the left gap)
    let (tx, rx) = mpsc::channel::<Vec<Workspace>>();
    spawn_i3_watcher(i3_socket_path(), output_name, dpi, tx);

    let mut workspaces: Vec<Workspace> = Vec::new();
    let mut bgrx = render_frame(&workspaces, &global, mon_height);

    loop {
        // Drain workspace updates; re-render if anything changed
        let mut changed = false;
        while let Ok(ws) = rx.try_recv() {
            workspaces = ws;
            changed = true;
        }
        if changed {
            bgrx = render_frame(&workspaces, &global, mon_height);
            conn.put_image(ImageFormat::Z_PIXMAP, win_id, gc, BAR_WIDTH as u16, mon_height as u16, 0, 0, 0, depth, &bgrx)?;
            conn.flush()?;
        }

        // Handle X11 events (non-blocking)
        while let Some(event) = conn.poll_for_event()? {
            if matches!(event, x11rb::protocol::Event::Expose(_)) {
                conn.put_image(ImageFormat::Z_PIXMAP, win_id, gc, BAR_WIDTH as u16, mon_height as u16, 0, 0, 0, depth, &bgrx)?;
                conn.flush()?;
            }
        }

        thread::sleep(Duration::from_millis(50));
    }
}
