use costae::{GlobalContext, parse_layout, render_frame};

#[test]
fn render_frame_respects_width_parameter() {
    let bgrx = render_frame(None, &GlobalContext::default(), 100, 50);
    assert_eq!(bgrx.len(), (100 * 50 * 4) as usize);
}

#[test]
fn render_frame_respects_different_width() {
    let bgrx_200 = render_frame(None, &GlobalContext::default(), 200, 50);
    let bgrx_400 = render_frame(None, &GlobalContext::default(), 400, 50);
    assert_eq!(bgrx_200.len(), 200 * 50 * 4);
    assert_eq!(bgrx_400.len(), 400 * 50 * 4);
}

#[test]
fn parse_layout_succeeds_for_valid_node_json() {
    let json = serde_json::json!({"type": "container", "children": []});
    assert!(parse_layout(&json).is_ok());
}

#[test]
fn render_frame_with_layout_returns_correct_size() {
    let layout = parse_layout(&serde_json::json!({
        "type": "container",
        "children": [{"type": "text", "text": "from layout"}]
    })).unwrap();
    let bgrx = render_frame(Some(layout), &GlobalContext::default(), 100, 200);
    assert_eq!(bgrx.len(), 100 * 200 * 4);
}
