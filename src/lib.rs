use std::borrow::Cow;
use std::path::{Path, PathBuf};

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

#[derive(Clone)]
pub struct Workspace {
    pub name: String,
    pub focused: bool,
}

pub fn build_node(workspaces: &[Workspace], width: u32, height: u32) -> Node {
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
            .with(StyleDeclaration::height(Px(height as f32)))
            .with(StyleDeclaration::background_color(ColorInput::Value(Color([
                30, 30, 46, 255,
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
                0, 255, 0, 255,
            ])))),
    )
}

pub fn render_frame(workspaces: &[Workspace], global: &GlobalContext, width: u32, height: u32) -> Vec<u8> {
    let node = build_node(workspaces, width, height);
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

