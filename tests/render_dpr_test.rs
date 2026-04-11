use costae::{GlobalContext, render_frame};

#[test]
fn render_frame_output_matches_physical_dimensions() {
    let global = GlobalContext::default();
    // Viewport dimensions are physical pixels; output is always width*height*4 bytes.
    // DPR only controls how CSS px units are scaled inside, not the buffer size.
    let out1x = render_frame(None, &global, 10, 10, 1.0);
    assert_eq!(out1x.len(), 10 * 10 * 4);

    // At 2x DPR, pass double the physical dimensions (same logical content, sharper output).
    let out2x = render_frame(None, &global, 20, 20, 2.0);
    assert_eq!(out2x.len(), 20 * 20 * 4);
}
