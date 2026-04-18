use std::os::unix::net::UnixStream;

use crate::ipc::{i3_recv, i3_send};

pub struct Workspace {
    pub name: String,
    pub focused: bool,
    pub urgent: bool,
}

pub fn fetch_workspaces(socket: &str, output: &str) -> std::io::Result<Vec<Workspace>> {
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
            urgent: w["urgent"].as_bool().unwrap_or(false),
        })
        .collect())
}

pub fn build_workspace_data(workspaces: &[Workspace]) -> serde_json::Value {
    serde_json::json!({
        "workspaces": workspaces.iter().map(|ws| serde_json::json!({
            "name": ws.name,
            "focused": ws.focused,
            "urgent": ws.urgent,
        })).collect::<Vec<_>>()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_workspace_data_has_workspaces_array() {
        let data = build_workspace_data(&[]);
        assert!(data["workspaces"].is_array());
    }

    #[test]
    fn build_workspace_data_includes_name_and_focused() {
        let ws = vec![
            Workspace { name: "1".into(), focused: true, urgent: false },
            Workspace { name: "2".into(), focused: false, urgent: false },
        ];
        let data = build_workspace_data(&ws);
        let workspaces = data["workspaces"].as_array().unwrap();
        assert_eq!(workspaces[0]["name"], "1");
        assert_eq!(workspaces[0]["focused"], true);
        assert_eq!(workspaces[1]["name"], "2");
        assert_eq!(workspaces[1]["focused"], false);
    }

    #[test]
    fn build_workspace_data_includes_urgent() {
        let ws = vec![
            Workspace { name: "1".into(), focused: true, urgent: true },
            Workspace { name: "2".into(), focused: false, urgent: false },
        ];
        let data = build_workspace_data(&ws);
        let workspaces = data["workspaces"].as_array().unwrap();
        assert_eq!(workspaces[0]["urgent"], true);
        assert_eq!(workspaces[1]["urgent"], false);
    }
}
