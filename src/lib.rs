use std::borrow::Cow;
use std::io::{Seek, SeekFrom, Write as IoWrite};
use std::os::unix::io::FromRawFd;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;

use serde::Deserialize;
pub use takumi::GlobalContext;
use takumi::{
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

#[derive(Clone)]
pub struct Workspace {
    pub name: String,
    pub focused: bool,
}

fn build_workspace_section(workspaces: &[Workspace], width: u32) -> Node {
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
                        Color([203, 166, 247, 255])
                    } else {
                        Color([166, 173, 200, 255])
                    }))),
            )
        })
        .collect();

    Node::container(items).with_style(
        Style::default()
            .with(StyleDeclaration::width(Px(width as f32)))
            .with(StyleDeclaration::display(Display::Flex))
            .with(StyleDeclaration::flex_direction(FlexDirection::Column))
            .with(StyleDeclaration::align_items(AlignItems::Center))
            .with(StyleDeclaration::justify_content(JustifyContent::FlexStart))
            .with(StyleDeclaration::row_gap(Px(8.0)))
            .with(StyleDeclaration::padding_top(Px(16.0))),
    )
}

pub fn build_node(workspaces: &[Workspace], layout: Option<Node>, width: u32, height: u32) -> Node {
    let mut children = vec![build_workspace_section(workspaces, width)];
    if let Some(node) = layout {
        children.push(node);
    }

    Node::container(children).with_style(
        Style::default()
            .with(StyleDeclaration::width(Px(width as f32)))
            .with(StyleDeclaration::height(Px(height as f32)))
            .with(StyleDeclaration::background_color(ColorInput::Value(Color([
                30, 30, 46, 255,
            ]))))
            .with(StyleDeclaration::display(Display::Flex))
            .with(StyleDeclaration::flex_direction(FlexDirection::Column))
            .with(StyleDeclaration::border_top_width(Px(1.0)))
            .with(StyleDeclaration::border_right_width(Px(1.0)))
            .with(StyleDeclaration::border_bottom_width(Px(1.0)))
            .with(StyleDeclaration::border_left_width(Px(1.0)))
            .with(StyleDeclaration::border_style(BorderStyle::Solid))
            .with(StyleDeclaration::border_color(ColorInput::Value(Color([
                0, 255, 0, 255,
            ])))),
    )
}

pub fn render_frame(workspaces: &[Workspace], layout: Option<Node>, global: &GlobalContext, width: u32, height: u32) -> Vec<u8> {
    let node = build_node(workspaces, layout, width, height);
    let options = RenderOptionsBuilder::default()
        .global(global)
        .viewport(Viewport::new(Some(width), Some(height)))
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

pub fn spawn_module(bin: &str, script: Option<&str>) -> (mpsc::Receiver<String>, std::process::Child) {
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

    (rx, child)
}

pub fn load_fonts(global: &mut GlobalContext) {
    for path in [
        "/usr/share/fonts/TTF/JetBrainsMono-Regular.ttf",
        "/usr/share/fonts/TTF/JetBrainsMono-Bold.ttf",
    ] {
        if let Ok(bytes) = std::fs::read(path) {
            let _ = global.font_context.load_and_store(Cow::from(bytes), None, None);
        }
    }
}
