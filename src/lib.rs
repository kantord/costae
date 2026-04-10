use std::io::{Seek, SeekFrom, Write as IoWrite};
use std::num::NonZeroUsize;
use std::os::unix::io::FromRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::thread;

use lru::LruCache;

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

use serde::Deserialize;
pub use takumi::GlobalContext;
pub use takumi::rendering::MeasuredNode;
use takumi::{
    layout::{Viewport, node::Node},
    rendering::{RenderOptions, render},
    resources::{font::FontResource, image::ImageSource},
};

#[derive(Debug, Deserialize, PartialEq)]
pub struct BarConfig {
    pub width: u32,
}

#[derive(Debug, Deserialize)]
pub struct Config {
    pub config: BarConfig,
    pub layout: serde_json::Value,
}

pub fn default_config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".config/costae/config.yaml")
}

pub fn load_config(path: &Path) -> Result<Config, Box<dyn std::error::Error>> {
    let content = std::fs::read_to_string(path)?;
    Ok(serde_yaml::from_str(&content)?)
}

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
    serde_json::from_value(value.clone())
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

pub fn render_frame(layout: Option<Node>, global: &GlobalContext, width: u32, height: u32) -> Vec<u8> {
    let node = layout.unwrap_or_else(|| Node::container(vec![]));
    let options = RenderOptions::builder()
        .global(global)
        .viewport(Viewport::new((Some(width), Some(height))))
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
