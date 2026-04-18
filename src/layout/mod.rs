use takumi::layout::node::Node;

/// Which screen edge a panel is anchored to. Drives both window placement and EWMH strut
/// reservation. Panels without an anchor are free-floating (no strut).
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum PanelAnchor {
    Left,
    Right,
    Top,
    Bottom,
}

impl PanelAnchor {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "left"   => Some(Self::Left),
            "right"  => Some(Self::Right),
            "top"    => Some(Self::Top),
            "bottom" => Some(Self::Bottom),
            _        => None,
        }
    }
}

/// Logical-pixel description of a `<panel>` node extracted from the JSX root.
/// All dimensions are in logical pixels; the display backend scales to physical pixels.
pub struct PanelSpec {
    pub id: String,
    pub anchor: Option<PanelAnchor>,
    /// Logical width in CSS px (same unit as i3 config / Tailwind values).
    pub width: u32,
    pub height: u32,
    pub x: i32,
    pub y: i32,
    /// i3-specific gap to reserve around the screen edges. Temporary until a
    /// cleaner per-WM mechanism exists.
    pub outer_gap: u32,
    /// RandR output name to place this panel on (e.g. "DP-2"). None = primary output.
    pub output: Option<String>,
    /// When true the window is stacked above other windows (for floating overlays like
    /// notifications). When false (default) the window sits below tiled content.
    pub above: bool,
    /// The layout subtree that lives inside this panel (first child of the panel node).
    pub content: serde_json::Value,
}

pub fn parse_layout(value: &serde_json::Value) -> Result<Node, serde_json::Error> {
    use serde::Deserialize;
    Node::deserialize(value)
}

/// Parse the JSX evaluator's output into a list of panel specs.
///
/// Expects the root value to be `{ type: "root", children: [...panels] }`. Each panel
/// child must have at minimum `id`, `width`, and `height`. Returns an error string if
/// the root type is wrong or a required field is missing.
pub fn parse_root_node(root: &serde_json::Value) -> Result<Vec<PanelSpec>, String> {
    if root.get("type").and_then(|t| t.as_str()) != Some("root") {
        return Err(format!("expected root node, got {:?}", root.get("type")));
    }
    let children = root.get("children")
        .and_then(|c| c.as_array())
        .ok_or_else(|| "root node has no children array".to_string())?;

    children.iter().enumerate()
        .filter(|(_, p)| p.get("type").and_then(|t| t.as_str()) == Some("panel"))
        .map(|(i, p)| parse_panel_spec(i, p))
        .collect()
}

fn parse_panel_spec(i: usize, panel: &serde_json::Value) -> Result<PanelSpec, String> {
    let id = panel.get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("panel[{i}] missing id"))?
        .to_string();
    let width = panel.get("width")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| format!("panel '{id}' missing width"))? as u32;
    let height = panel.get("height")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| format!("panel '{id}' missing height"))? as u32;
    let anchor = panel.get("anchor")
        .and_then(|v| v.as_str())
        .and_then(PanelAnchor::parse);
    let x = panel.get("x").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
    let y = panel.get("y").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
    let outer_gap = panel.get("outer_gap")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    let output = panel.get("output")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let above = panel.get("above")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    // The panel's layout content: first child (typically a root container).
    let content = panel.get("children")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first())
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    Ok(PanelSpec { id, anchor, width, height, x, y, outer_gap, output, above, content })
}

