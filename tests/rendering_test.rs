use costae::{GlobalContext, render_frame};

#[test]
fn render_frame_respects_width_parameter() {
    let width = 100u32;
    let height = 50u32;
    let bgrx = render_frame(&[], &GlobalContext::default(), width, height);
    assert_eq!(bgrx.len(), (width * height * 4) as usize);
}

#[test]
fn render_frame_respects_different_width() {
    let bgrx_200 = render_frame(&[], &GlobalContext::default(), 200, 50);
    let bgrx_400 = render_frame(&[], &GlobalContext::default(), 400, 50);
    assert_eq!(bgrx_200.len(), 200 * 50 * 4);
    assert_eq!(bgrx_400.len(), 400 * 50 * 4);
}
