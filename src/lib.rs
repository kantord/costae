pub mod jsx;
pub mod data_loop;

use std::io::{Seek, SeekFrom, Write as IoWrite};
use std::num::NonZeroUsize;
use std::os::unix::io::FromRawFd;
use std::sync::{Arc, mpsc};
use std::thread;

use lru::LruCache;

/// LRU cache of rendered frames keyed on the canonical JSON of `module_values`.
///
/// Canonical JSON (RFC 8785 via `json_canon`) normalises object key order so that
/// `{"a":1,"b":2}` and `{"b":2,"a":1}` resolve to the same cache entry.
///
/// Frames are stored as `Arc<Vec<u8>>` so the caller can hold onto the current
/// frame across loop iterations even after it has been evicted from the cache
/// (e.g. the bar needs to repaint on Expose while the cache has already moved on).
pub struct RenderCache {
    cache: LruCache<String, Arc<Vec<u8>>>,
}

impl RenderCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            cache: LruCache::new(NonZeroUsize::new(capacity).unwrap()),
        }
    }

    pub fn get_or_render<F>(&mut self, key: &serde_json::Value, render: F) -> Arc<Vec<u8>>
    where
        F: FnOnce() -> Vec<u8>,
    {
        let t = std::time::Instant::now();
        let canonical = json_canon::to_string(key).unwrap_or_default();
        if let Some(cached) = self.cache.get(&canonical) {
            eprintln!("[costae] render cache HIT  — {}µs ({} bytes)", t.elapsed().as_micros(), cached.len());
            return Arc::clone(cached);
        }
        let result = Arc::new(render());
        eprintln!("[costae] render cache MISS — {}ms ({} bytes)", t.elapsed().as_millis(), result.len());
        self.cache.put(canonical, Arc::clone(&result));
        result
    }
}

pub use takumi::GlobalContext;

/// Convert X11 ZPixmap BGRX bytes (4 bytes per pixel, X padding ignored) to RGBA
/// with alpha=255 (wallpaper is always fully opaque).
pub fn x11_bgrx_to_rgba(bgrx: &[u8]) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(bgrx.len());
    for px in bgrx.chunks_exact(4) {
        rgba.push(px[2]); // R
        rgba.push(px[1]); // G
        rgba.push(px[0]); // B
        rgba.push(0xFF);  // A
    }
    rgba
}

/// Store a raw RGBA wallpaper slice (already cropped to bar dimensions) in the
/// persistent image store under the key `"root-bg"` so layout nodes can
/// reference it via `backgroundImage: "url(root-bg)"` in the config.
///
/// Because takumi sees this as real pixel content behind elements in the render
/// tree, CSS `backdrop-filter: blur()` on cards will correctly blur the wallpaper
/// — the same effect that a compositor would produce, but in pure software.
pub fn inject_root_bg(global: &GlobalContext, rgba: Vec<u8>, width: u32, height: u32) {
    use tiny_skia::{IntSize, Pixmap};
    use takumi::resources::image::ImageSource;
    if let Some(size) = IntSize::from_wh(width, height) {
        if let Some(pixmap) = Pixmap::from_vec(rgba, size) {
            global.persistent_image_store.insert("root-bg".to_string(), ImageSource::from(pixmap));
        }
    }
}
pub use takumi::rendering::MeasuredNode;
use takumi::{
    layout::{Viewport, node::Node},
    rendering::{RenderOptions, render},
    resources::{font::FontResource, image::ImageSource},
};

// --- substitution ---

pub fn is_module_node(value: &serde_json::Value) -> bool {
    value.as_object().map_or(false, |o| o.contains_key("bin@"))
}

pub struct ModuleNode {
    pub path: String,
    pub bin: String,
    pub script: Option<String>,
}

pub fn find_modules(tree: &serde_json::Value) -> Vec<ModuleNode> {
    let mut out = Vec::new();
    find_modules_inner(tree, "", &mut out);
    out
}

fn find_modules_inner(value: &serde_json::Value, path: &str, out: &mut Vec<ModuleNode>) {
    if is_module_node(value) {
        let obj = value.as_object().unwrap();
        out.push(ModuleNode {
            path: path.to_string(),
            bin: obj["bin@"].as_str().unwrap_or("").to_string(),
            script: obj.get("script").and_then(|s| s.as_str()).map(str::to_string),
        });
        return; // terminal — do not recurse
    }
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                find_modules_inner(v, &format!("{}/{}", path, k), out);
            }
        }
        serde_json::Value::Array(arr) => {
            for (i, v) in arr.iter().enumerate() {
                find_modules_inner(v, &format!("{}/{}", path, i), out);
            }
        }
        _ => {}
    }
}

pub fn substitute(
    tree: &serde_json::Value,
    values: &std::collections::HashMap<String, serde_json::Value>,
) -> serde_json::Value {
    substitute_inner(tree, "", values)
}

fn substitute_inner(
    value: &serde_json::Value,
    path: &str,
    values: &std::collections::HashMap<String, serde_json::Value>,
) -> serde_json::Value {
    if is_module_node(value) {
        return values.get(path).cloned().unwrap_or(serde_json::Value::Null);
    }
    match value {
        serde_json::Value::Object(map) => {
            let new_map = map
                .iter()
                .map(|(k, v)| (k.clone(), substitute_inner(v, &format!("{}/{}", path, k), values)))
                .collect();
            serde_json::Value::Object(new_map)
        }
        serde_json::Value::Array(arr) => {
            let new_arr = arr
                .iter()
                .enumerate()
                .map(|(i, v)| substitute_inner(v, &format!("{}/{}", path, i), values))
                .collect();
            serde_json::Value::Array(new_arr)
        }
        _ => value.clone(),
    }
}

// ---

pub fn parse_layout(value: &serde_json::Value) -> Result<Node, serde_json::Error> {
    use serde::Deserialize;
    Node::deserialize(value)
}

/// Returns true if the focused workspace on the given output has any fullscreen window.
/// `tree` is the JSON from an i3 GET_TREE (type 4) response.
///
/// The real i3 tree nests workspaces inside a content container:
///   root → output → content_container → workspace → windows
/// We follow the `focus` array at each level until we reach a workspace node.
pub fn has_fullscreen_on_output(tree: &serde_json::Value, output_name: &str) -> bool {
    let outputs = match tree["nodes"].as_array() {
        Some(a) => a,
        None => return false,
    };
    for output in outputs {
        if output["name"].as_str() != Some(output_name) {
            continue;
        }
        return focused_workspace_has_fullscreen(output);
    }
    false
}

/// Follow the focus chain from `container` down to the focused workspace,
/// then check if that workspace has any fullscreen window.
fn focused_workspace_has_fullscreen(container: &serde_json::Value) -> bool {
    if container["type"].as_str() == Some("workspace") {
        return node_has_fullscreen(container);
    }
    let focused_id = container["focus"].as_array()
        .and_then(|f| f.first())
        .and_then(|id| id.as_u64());
    if let (Some(fid), Some(nodes)) = (focused_id, container["nodes"].as_array()) {
        for child in nodes {
            if child["id"].as_u64() == Some(fid) {
                return focused_workspace_has_fullscreen(child);
            }
        }
    }
    false
}

fn node_has_fullscreen(node: &serde_json::Value) -> bool {
    if node["fullscreen_mode"].as_u64().unwrap_or(0) > 0 {
        return true;
    }
    for key in &["nodes", "floating_nodes"] {
        if let Some(children) = node[key].as_array() {
            if children.iter().any(node_has_fullscreen) {
                return true;
            }
        }
    }
    false
}

/// Walk the MeasuredNode and JSON trees in lockstep to find the deepest node
/// under (click_x, click_y) that carries an `on_click` field.
/// Returns (json_path, on_click_value) on hit, None otherwise.
/// Transforms are relative to parent, so we accumulate them as we descend.
pub fn hit_test(
    measured: &MeasuredNode,
    json: &serde_json::Value,
    click_x: f32,
    click_y: f32,
) -> Option<(String, serde_json::Value)> {
    hit_test_inner(measured, json, click_x, click_y, "")
}

fn hit_test_inner(
    measured: &MeasuredNode,
    json: &serde_json::Value,
    click_x: f32,
    click_y: f32,
    path: &str,
) -> Option<(String, serde_json::Value)> {
    // Takumi stores absolute screen coordinates in transform[4/5]
    let node_x = measured.transform[4];
    let node_y = measured.transform[5];

    if click_x < node_x || click_x > node_x + measured.width
        || click_y < node_y || click_y > node_y + measured.height
    {
        return None;
    }

    // Prefer deepest child hit first
    if let Some(children_json) = json.get("children").and_then(|c| c.as_array()) {
        for (i, (child_m, child_j)) in measured.children.iter().zip(children_json).enumerate() {
            let child_path = format!("{}/children/{}", path, i);
            if let Some(result) = hit_test_inner(child_m, child_j, click_x, click_y, &child_path) {
                return Some(result);
            }
        }
    }

    // This node is the deepest hit — return it if it has on_click
    json.get("on_click").map(|v| (path.to_string(), v.clone()))
}

/// Insert `new_value` at `path` in `values`. Returns `true` if the value actually changed
/// (i.e. was absent or different), `false` if it was already identical. Use this to avoid
/// unnecessary re-renders when a module emits the same output repeatedly.
pub fn update_module_value(
    values: &mut std::collections::HashMap<String, serde_json::Value>,
    path: String,
    new_value: serde_json::Value,
) -> bool {
    if values.get(&path) == Some(&new_value) {
        return false;
    }
    values.insert(path, new_value);
    true
}

/// Render `layout` into a BGRX framebuffer.
///
/// `width` and `height` are **physical** pixels — the X11 window dimensions.
/// `dpr = dpi / 96.0` scales CSS `px` units so that `1px` in the config equals
/// one logical pixel regardless of display density, matching i3's own scaling.
/// The returned buffer is always `width × height × 4` bytes (BGRX).
pub fn render_frame(layout: Option<Node>, global: &GlobalContext, width: u32, height: u32, dpr: f32) -> Vec<u8> {
    let node = layout.unwrap_or_else(|| Node::container(vec![]));
    let options = RenderOptions::builder()
        .global(global)
        .viewport(Viewport::new((Some(width), Some(height))).with_device_pixel_ratio(dpr))
        .node(node)
        .build();
    let rgba = render(options).expect("render").into_raw();
    let mut bgrx = Vec::with_capacity(rgba.len());
    for px in rgba.chunks_exact(4) {
        bgrx.extend_from_slice(&[px[2], px[1], px[0], 0x00]);
    }
    bgrx
}

pub struct SpawnedModule {
    pub rx: mpsc::Receiver<String>,
    pub child: std::process::Child,
    pub event_tx: mpsc::Sender<serde_json::Value>,
}

impl SpawnedModule {
    pub fn send_event(&self, event: &serde_json::Value) {
        let _ = self.event_tx.send(event.clone());
    }
}

pub fn spawn_module(bin: &str, script: Option<&str>) -> SpawnedModule {
    let (tx, rx) = mpsc::channel();
    let mut cmd = std::process::Command::new(bin);

    // If a script is provided, write it to a memfd and pass the path as argument
    let _memfd_file = if let Some(content) = script {
        let fd = unsafe {
            libc::memfd_create(b"costae-script\0".as_ptr() as *const libc::c_char, 0)
        };
        let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
        let _ = file.write_all(content.as_bytes());
        let _ = file.seek(SeekFrom::Start(0));
        cmd.arg(format!("/proc/self/fd/{}", fd));
        Some(file) // keep alive until after spawn so fd is inherited
    } else {
        None
    };

    let mut child = cmd
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("failed to spawn module");
    // _memfd_file can now be dropped — child has inherited the fd

    let stdout = child.stdout.take().expect("no stdout");
    thread::spawn(move || {
        let reader = std::io::BufReader::new(stdout);
        use std::io::BufRead;
        for line in reader.lines() {
            match line {
                Ok(l) => {
                    if tx.send(l).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut stdin = child.stdin.take().expect("no stdin");
    let (event_tx, event_rx) = mpsc::channel::<serde_json::Value>();
    thread::spawn(move || {
        while let Ok(event) = event_rx.recv() {
            if writeln!(stdin, "{}", event).is_err() {
                break;
            }
        }
    });

    SpawnedModule { rx, child, event_tx }
}

/// Spawn a string-streaming subprocess (e.g. a bash script that prints one line per tick).
/// Each line emitted by the process is sent to `tx` as `(bin, script, line)`.
/// The returned `Child` must be kept alive; drop it to kill the process.
pub struct SpawnedBiStream {
    pub child: std::process::Child,
    pub event_tx: mpsc::Sender<serde_json::Value>,
}

/// Spawn a bidirectional module subprocess (stdin for events, stdout for data).
/// Sends the init event immediately, then forwards stdout lines to `tx` as `(bin, None, line)`.
pub fn spawn_bi_stream(
    bin: &str,
    init_event: &serde_json::Value,
    tx: mpsc::Sender<(String, Option<String>, String)>,
    wake_tx: mpsc::SyncSender<()>,
) -> SpawnedBiStream {
    let spawned = spawn_module(bin, None);
    spawned.send_event(init_event);
    let bin_owned = bin.to_string();
    thread::spawn(move || {
        while let Ok(line) = spawned.rx.recv() {
            if tx.send((bin_owned.clone(), None, line)).is_err() {
                break;
            }
            let _ = wake_tx.try_send(());
        }
    });
    SpawnedBiStream { child: spawned.child, event_tx: spawned.event_tx }
}

pub fn spawn_string_stream(
    bin: &str,
    script: Option<&str>,
    tx: mpsc::Sender<(String, Option<String>, String)>,
    wake_tx: mpsc::SyncSender<()>,
) -> std::process::Child {
    let spawned = spawn_module(bin, script);
    let bin_owned = bin.to_string();
    let script_owned = script.map(str::to_string);
    thread::spawn(move || {
        while let Ok(line) = spawned.rx.recv() {
            if tx.send((bin_owned.clone(), script_owned.clone(), line)).is_err() {
                break;
            }
            let _ = wake_tx.try_send(());
        }
        eprintln!("[costae] stream subprocess exited: bin={:?} script={:?}", bin_owned, script_owned);
    });
    spawned.child
}

pub fn preload_layout_images(layout: &serde_json::Value, global: &GlobalContext) {
    fn walk(value: &serde_json::Value, srcs: &mut Vec<String>) {
        match value {
            serde_json::Value::Object(map) => {
                if map.get("type").and_then(|t| t.as_str()) == Some("image") {
                    if let Some(src) = map.get("src").and_then(|s| s.as_str()) {
                        srcs.push(src.to_string());
                    }
                    return; // image nodes are terminal
                }
                for v in map.values() {
                    walk(v, srcs);
                }
            }
            serde_json::Value::Array(arr) => {
                for v in arr {
                    walk(v, srcs);
                }
            }
            _ => {}
        }
    }

    let mut srcs = Vec::new();
    walk(layout, &mut srcs);

    for src in srcs {
        if src.starts_with("http://") || src.starts_with("https://") || src.starts_with("data:") {
            continue;
        }
        if let Ok(bytes) = std::fs::read(&src) {
            if let Ok(image) = ImageSource::from_bytes(&bytes) {
                global.persistent_image_store.insert(src, image);
            }
        }
    }
}

/// Compute `_NET_WM_STRUT_PARTIAL` values for a panel anchored to a screen edge.
///
/// The 12-element array follows the EWMH spec:
///   [0] left, [1] right, [2] top, [3] bottom,
///   [4] left_start_y,  [5] left_end_y,
///   [6] right_start_y, [7] right_end_y,
///   [8] top_start_x,   [9] top_end_x,
///   [10] bottom_start_x, [11] bottom_end_x
///
/// All values are in physical pixels, absolute from the screen origin.
pub fn strut_partial_values_for_anchor(
    anchor: PanelAnchor,
    mon_x: i16,
    mon_y: i16,
    _mon_width: u32,
    mon_height: u32,
    phys_panel_width: u32,
    phys_panel_height: u32,
) -> [u32; 12] {
    let mut v = [0u32; 12];
    match anchor {
        PanelAnchor::Left => {
            v[0] = mon_x as u32 + phys_panel_width;
            v[4] = mon_y as u32;
            v[5] = mon_y as u32 + mon_height.saturating_sub(1);
        }
        PanelAnchor::Right => {
            v[1] = phys_panel_width; // measured from right screen edge
            v[6] = mon_y as u32;
            v[7] = mon_y as u32 + mon_height.saturating_sub(1);
        }
        PanelAnchor::Top => {
            v[2] = mon_y as u32 + phys_panel_height;
            v[8] = mon_x as u32;
            v[9] = mon_x as u32 + _mon_width.saturating_sub(1);
        }
        PanelAnchor::Bottom => {
            v[3] = phys_panel_height; // measured from bottom screen edge
            v[10] = mon_x as u32;
            v[11] = mon_x as u32 + _mon_width.saturating_sub(1);
        }
    }
    v
}

/// Convert an X11 TrueColor pixel value (0x00RRGGBB for standard 24bpp visuals)
/// to a flat RGBA buffer of `width × height` pixels, all the same solid color.
/// Used as a fallback when no wallpaper pixmap is set (e.g. i3 solid background).
pub fn solid_color_rgba(pixel: u32, width: u32, height: u32) -> Vec<u8> {
    let r = ((pixel >> 16) & 0xFF) as u8;
    let g = ((pixel >> 8) & 0xFF) as u8;
    let b = (pixel & 0xFF) as u8;
    let count = (width * height) as usize;
    let mut rgba = Vec::with_capacity(count * 4);
    for _ in 0..count {
        rgba.extend_from_slice(&[r, g, b, 0xFF]);
    }
    rgba
}

/// Which screen edge a panel is anchored to. Drives both window placement and EWMH strut
/// reservation. Panels without an anchor are free-floating (no strut).
#[derive(Debug, PartialEq, Clone)]
pub enum PanelAnchor {
    Left,
    Right,
    Top,
    Bottom,
}

impl PanelAnchor {
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "left"   => Some(PanelAnchor::Left),
            "right"  => Some(PanelAnchor::Right),
            "top"    => Some(PanelAnchor::Top),
            "bottom" => Some(PanelAnchor::Bottom),
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

    children.iter().enumerate().filter_map(|(i, panel)| {
        if panel.get("type").and_then(|t| t.as_str()) != Some("panel") {
            return None; // skip non-panel children silently
        }
        Some((|| -> Result<PanelSpec, String> {
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
                .and_then(PanelAnchor::from_str);
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
        })())
    }).collect()
}

/// Partition new panel specs against currently-live panel ids.
///
/// Returns `(to_create, to_update, to_destroy)` where:
/// - `to_create`: specs whose id is not in `existing_ids` — a new X11 window must be created
/// - `to_update`: specs whose id IS in `existing_ids` — re-render in the existing window
/// - `to_destroy`: existing ids not present in `new_specs` — the X11 window must be destroyed
pub fn reconcile_panels<'a>(
    existing_ids: &[&str],
    new_specs: &'a [PanelSpec],
) -> (Vec<&'a PanelSpec>, Vec<&'a PanelSpec>, Vec<String>) {
    let new_ids: std::collections::HashSet<&str> = new_specs.iter().map(|p| p.id.as_str()).collect();
    let existing_set: std::collections::HashSet<&str> = existing_ids.iter().copied().collect();
    let to_create = new_specs.iter().filter(|p| !existing_set.contains(p.id.as_str())).collect();
    let to_update = new_specs.iter().filter(|p| existing_set.contains(p.id.as_str())).collect();
    let to_destroy = existing_ids.iter().filter(|id| !new_ids.contains(*id)).map(|s| s.to_string()).collect();
    (to_create, to_update, to_destroy)
}

pub fn reconcile_streams(
    old: &[(String, Option<String>)],
    new: &[(String, Option<String>)],
) -> (Vec<(String, Option<String>)>, Vec<(String, Option<String>)>) {
    let old_set: std::collections::HashSet<_> = old.iter().collect();
    let new_set: std::collections::HashSet<_> = new.iter().collect();
    let to_spawn = new.iter().filter(|x| !old_set.contains(x)).cloned().collect();
    let to_kill = old.iter().filter(|x| !new_set.contains(x)).cloned().collect();
    (to_spawn, to_kill)
}

pub fn load_fonts(global: &mut GlobalContext) {
    for path in [
        "/usr/share/fonts/TTF/JetBrainsMono-Regular.ttf",
        "/usr/share/fonts/TTF/JetBrainsMono-Bold.ttf",
    ] {
        if let Ok(bytes) = std::fs::read(path) {
            let _ = global.font_context.load_and_store(FontResource::new(bytes));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_root_node_extracts_panel_specs() {
        let root = serde_json::json!({
            "type": "root",
            "children": [{
                "type": "panel",
                "id": "sidebar",
                "anchor": "left",
                "width": 250,
                "height": 2160,
                "outer_gap": 8,
                "children": [{ "type": "container" }]
            }]
        });
        let panels = parse_root_node(&root).unwrap();
        assert_eq!(panels.len(), 1);
        assert_eq!(panels[0].id, "sidebar");
        assert_eq!(panels[0].anchor, Some(PanelAnchor::Left));
        assert_eq!(panels[0].width, 250);
        assert_eq!(panels[0].height, 2160);
        assert_eq!(panels[0].outer_gap, 8);
    }

    #[test]
    fn parse_root_node_rejects_non_root_type() {
        let node = serde_json::json!({ "type": "container" });
        assert!(parse_root_node(&node).is_err());
    }

    #[test]
    fn parse_root_node_defaults_x_y_outer_gap_to_zero() {
        let root = serde_json::json!({
            "type": "root",
            "children": [{
                "type": "panel",
                "id": "sidebar",
                "width": 250,
                "height": 2160,
                "children": []
            }]
        });
        let panels = parse_root_node(&root).unwrap();
        assert_eq!(panels[0].x, 0);
        assert_eq!(panels[0].y, 0);
        assert_eq!(panels[0].outer_gap, 0);
        assert_eq!(panels[0].anchor, None);
    }

    #[test]
    fn strut_for_anchor_left_sets_left_strut() {
        let v = strut_partial_values_for_anchor(PanelAnchor::Left, 0, 0, 1920, 2160, 365, 2160);
        assert_eq!(v[0], 365); // left strut
        assert_eq!(v[1], 0);   // right strut
        assert_eq!(v[2], 0);   // top strut
        assert_eq!(v[3], 0);   // bottom strut
        assert_eq!(v[4], 0);   // left_start_y
        assert_eq!(v[5], 2159); // left_end_y
    }

    #[test]
    fn strut_for_anchor_top_sets_top_strut() {
        let v = strut_partial_values_for_anchor(PanelAnchor::Top, 0, 0, 1920, 2160, 1920, 32);
        assert_eq!(v[0], 0);
        assert_eq!(v[2], 32);  // top strut
        assert_eq!(v[8], 0);   // top_start_x
        assert_eq!(v[9], 1919); // top_end_x
    }

    #[test]
    fn reconcile_panels_partitions_specs_into_create_update_destroy() {
        fn spec(id: &str) -> PanelSpec {
            PanelSpec { id: id.to_string(), anchor: None, width: 100, height: 100, x: 0, y: 0, outer_gap: 0, output: None, above: false, content: serde_json::Value::Null }
        }
        let new_specs = vec![spec("sidebar"), spec("topbar")];
        let (to_create, to_update, to_destroy) = reconcile_panels(&["sidebar", "bottombar"], &new_specs);
        assert_eq!(to_create.len(), 1);
        assert_eq!(to_create[0].id, "topbar");
        assert_eq!(to_update.len(), 1);
        assert_eq!(to_update[0].id, "sidebar");
        assert_eq!(to_destroy, vec!["bottombar".to_string()]);
    }

    #[test]
    fn reconcile_streams_returns_additions_and_removals() {
        let old = vec![
            ("bash".to_string(), Some("script_a".to_string())),
            ("bash".to_string(), Some("script_b".to_string())),
        ];
        let new_calls = vec![
            ("bash".to_string(), Some("script_b".to_string())),
            ("bash".to_string(), Some("script_c".to_string())),
        ];
        let (to_spawn, to_kill) = reconcile_streams(&old, &new_calls);
        assert_eq!(to_spawn, vec![("bash".to_string(), Some("script_c".to_string()))]);
        assert_eq!(to_kill, vec![("bash".to_string(), Some("script_a".to_string()))]);
    }
}
