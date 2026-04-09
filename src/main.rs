use std::borrow::Cow;

use takumi::{
    GlobalContext,
    layout::{
        Viewport,
        node::Node,
        style::{
            AlignItems, BorderStyle, Color, ColorInput, Display, JustifyContent, Length::Px,
            Style, StyleDeclaration,
        },
    },
    rendering::{RenderOptionsBuilder, render},
};
use x11rb::{
    connection::Connection,
    protocol::{randr::ConnectionExt as RandrExt, xproto::*},
    rust_connection::RustConnection,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Connect to X11, get screen dimensions
    let (conn, screen_num) = RustConnection::connect(None)?;
    let screen = &conn.setup().roots[screen_num];
    let depth = screen.root_depth;

    // Find the primary monitor's x position and height via RandR
    let primary_output = conn
        .randr_get_output_primary(screen.root)?
        .reply()?
        .output;
    let output_info = conn
        .randr_get_output_info(primary_output, 0)?
        .reply()?;
    let crtc_info = conn
        .randr_get_crtc_info(output_info.crtc, 0)?
        .reply()?;
    let mon_x = crtc_info.x;
    let mon_y = crtc_info.y;
    let mon_height = crtc_info.height as u32;

    const WIDTH: u32 = 300;

    // Create and map borderless window on the left of the primary monitor (override_redirect bypasses WM)
    let win_id = conn.generate_id()?;
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        win_id,
        screen.root,
        mon_x,
        mon_y,
        WIDTH as u16,
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

    // Build Takumi node tree: centered text on dark background
    let node = Node::container([Node::text("costae").with_style(
        Style::default()
            .with(StyleDeclaration::font_size(Px(32.0).into()))
            .with(StyleDeclaration::color(ColorInput::Value(Color([
                203, 166, 247, 255, // #cba6f7
            ])))),
    )])
    .with_style(
        Style::default()
            .with(StyleDeclaration::width(Px(WIDTH as f32)))
            .with(StyleDeclaration::height(Px(mon_height as f32)))
            .with(StyleDeclaration::background_color(ColorInput::Value(Color([
                30, 30, 46, 255, // #1e1e2e
            ]))))
            .with(StyleDeclaration::display(Display::Flex))
            .with(StyleDeclaration::align_items(AlignItems::Center))
            .with(StyleDeclaration::justify_content(JustifyContent::Center))
            .with(StyleDeclaration::border_top_width(Px(1.0)))
            .with(StyleDeclaration::border_right_width(Px(1.0)))
            .with(StyleDeclaration::border_bottom_width(Px(1.0)))
            .with(StyleDeclaration::border_left_width(Px(1.0)))
            .with(StyleDeclaration::border_style(BorderStyle::Solid))
            .with(StyleDeclaration::border_color(ColorInput::Value(Color([
                0, 255, 0, 255, // #00ff00
            ])))),
    );

    // Load a system font so text renders
    let mut global = GlobalContext::default();
    let font_bytes = std::fs::read("/usr/share/fonts/TTF/JetBrainsMono-Regular.ttf")
        .or_else(|_| std::fs::read("/usr/share/fonts/liberation/LiberationSans-Regular.ttf"))?;
    global
        .font_context
        .load_and_store(Cow::from(font_bytes), None, None)?;

    // Render + convert pixels: Takumi outputs RGBA, X11 ZPixmap expects BGRX
    let viewport = Viewport::new(Some(WIDTH), Some(mon_height));
    let options = RenderOptionsBuilder::default()
        .global(&global)
        .viewport(viewport)
        .node(node)
        .build()?;
    let rgba = render(options)?.into_raw();

    let mut bgrx: Vec<u8> = Vec::with_capacity(rgba.len());
    for px in rgba.chunks_exact(4) {
        bgrx.extend_from_slice(&[px[2], px[1], px[0], 0x00]); // [R,G,B,A] → [B,G,R,0]
    }

    // Create GC for blitting
    let gc = conn.generate_id()?;
    conn.create_gc(gc, win_id, &CreateGCAux::new())?;

    // Blit to window on Expose; block forever afterwards
    loop {
        let event = conn.wait_for_event()?;
        if let x11rb::protocol::Event::Expose(_) = event {
            conn.put_image(
                ImageFormat::Z_PIXMAP,
                win_id,
                gc,
                WIDTH as u16,
                mon_height as u16,
                0,
                0,
                0,
                depth,
                &bgrx,
            )?;
            conn.flush()?;
        }
    }
}
