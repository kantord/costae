use std::io::{BufRead, Read, Write};
use std::os::unix::net::UnixStream;

// --- i3 IPC ---

const I3_MAGIC: &[u8; 6] = b"i3-ipc";

fn i3_send(s: &mut UnixStream, msg_type: u32, payload: &[u8]) -> std::io::Result<()> {
    s.write_all(I3_MAGIC)?;
    s.write_all(&(payload.len() as u32).to_le_bytes())?;
    s.write_all(&msg_type.to_le_bytes())?;
    s.write_all(payload)
}

fn i3_recv(s: &mut UnixStream) -> std::io::Result<(u32, Vec<u8>)> {
    let mut hdr = [0u8; 14];
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

// i3 only scales gaps if dpi/96 >= 1.25 (logical_px threshold in libi3/dpi.c)
fn apply_bar_gap(socket: &str, dpi: f32, bar_width: u32) {
    if let Ok(mut s) = UnixStream::connect(socket) {
        let gap = if (dpi / 96.0) < 1.25 {
            bar_width
        } else {
            (bar_width as f32 * 96.0 / dpi).floor() as u32
        };
        let cmd = format!("gaps left current set {}", gap);
        let _ = i3_send(&mut s, 0, cmd.as_bytes());
        let _ = i3_recv(&mut s);
    }
}

// --- Workspace types ---

pub struct Workspace {
    pub name: String,
    pub focused: bool,
}

fn fetch_workspaces(socket: &str, output: &str) -> std::io::Result<Vec<Workspace>> {
    let mut s = UnixStream::connect(socket)?;
    i3_send(&mut s, 1, b"")?;
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

// --- Node builder ---

pub fn build_workspace_node(workspaces: &[Workspace]) -> serde_json::Value {
    let children: Vec<serde_json::Value> = workspaces
        .iter()
        .map(|ws| {
            let tw = if ws.focused {
                "text-[16px] text-[#cba6f7] font-bold text-center w-full"
            } else {
                "text-[16px] text-[#a6adc8] text-center w-full"
            };
            serde_json::json!({"type": "text", "tw": tw, "text": ws.name})
        })
        .collect();

    serde_json::json!({
        "type": "container",
        "tw": "flex flex-col items-center gap-[8px] pt-[16px] w-full",
        "children": children
    })
}

// --- Init event ---

pub struct InitEvent {
    pub output: String,
    pub bar_width: u32,
    pub dpi: f32,
}

pub fn parse_init_event(json: &str) -> Option<InitEvent> {
    let val: serde_json::Value = serde_json::from_str(json).ok()?;
    if val["type"].as_str() != Some("init") {
        return None;
    }
    Some(InitEvent {
        output: val["output"].as_str()?.to_string(),
        bar_width: val["config"]["width"].as_u64()? as u32,
        dpi: val["dpi"].as_f64().unwrap_or(96.0) as f32,
    })
}

// --- Main ---

fn main() {
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();

    // Block until we receive the init event
    let init = loop {
        match lines.next() {
            Some(Ok(line)) => {
                if let Some(ev) = parse_init_event(&line) {
                    break ev;
                }
            }
            _ => return,
        }
    };

    let socket = i3_socket_path();

    // Emit initial workspace state
    if let Ok(ws) = fetch_workspaces(&socket, &init.output) {
        if ws.iter().any(|w| w.focused) {
            apply_bar_gap(&socket, init.dpi, init.bar_width);
        }
        println!("{}", build_workspace_node(&ws));
    }

    // Subscribe to workspace events
    let mut sub = match UnixStream::connect(&socket) {
        Ok(s) => s,
        Err(_) => return,
    };
    let _ = i3_send(&mut sub, 2, b"[\"workspace\"]");
    let _ = i3_recv(&mut sub);

    loop {
        match i3_recv(&mut sub) {
            Ok((0x80000000, payload)) => {
                if let Ok(event) = serde_json::from_slice::<serde_json::Value>(&payload) {
                    if event["current"]["output"].as_str() == Some(init.output.as_str()) {
                        apply_bar_gap(&socket, init.dpi, init.bar_width);
                    }
                }
                if let Ok(ws) = fetch_workspaces(&socket, &init.output) {
                    println!("{}", build_workspace_node(&ws));
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_workspace_node_type_is_container() {
        let node = build_workspace_node(&[]);
        assert_eq!(node["type"], "container");
    }

    #[test]
    fn build_workspace_node_empty_has_no_children() {
        let node = build_workspace_node(&[]);
        assert_eq!(node["children"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn build_workspace_node_contains_workspace_names() {
        let ws = vec![
            Workspace { name: "web".into(), focused: false },
            Workspace { name: "term".into(), focused: false },
        ];
        let node = build_workspace_node(&ws);
        let children = node["children"].as_array().unwrap();
        assert_eq!(children[0]["text"], "web");
        assert_eq!(children[1]["text"], "term");
    }

    #[test]
    fn build_workspace_node_focused_workspace_has_highlight_color() {
        let ws = vec![
            Workspace { name: "1".into(), focused: true },
            Workspace { name: "2".into(), focused: false },
        ];
        let node = build_workspace_node(&ws);
        let children = node["children"].as_array().unwrap();
        assert!(children[0]["tw"].as_str().unwrap().contains("#cba6f7"));
        assert!(!children[1]["tw"].as_str().unwrap().contains("#cba6f7"));
    }

    #[test]
    fn build_workspace_node_unfocused_workspace_has_muted_color() {
        let ws = vec![Workspace { name: "1".into(), focused: false }];
        let node = build_workspace_node(&ws);
        let children = node["children"].as_array().unwrap();
        assert!(children[0]["tw"].as_str().unwrap().contains("#a6adc8"));
    }

    #[test]
    fn parse_init_event_extracts_output_and_config() {
        let json = r#"{"type":"init","output":"DP-1","config":{"width":200},"dpi":96.0}"#;
        let ev = parse_init_event(json).unwrap();
        assert_eq!(ev.output, "DP-1");
        assert_eq!(ev.bar_width, 200);
        assert!((ev.dpi - 96.0).abs() < 0.01);
    }

    #[test]
    fn parse_init_event_defaults_dpi_to_96() {
        let json = r#"{"type":"init","output":"DP-1","config":{"width":200}}"#;
        let ev = parse_init_event(json).unwrap();
        assert!((ev.dpi - 96.0).abs() < 0.01);
    }

    #[test]
    fn parse_init_event_returns_none_for_wrong_type() {
        let json = r#"{"type":"ping","output":"DP-1","config":{"width":200}}"#;
        assert!(parse_init_event(json).is_none());
    }

    #[test]
    fn parse_init_event_returns_none_for_invalid_json() {
        assert!(parse_init_event("not json").is_none());
    }
}
