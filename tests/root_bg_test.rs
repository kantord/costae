use costae::x11_bgrx_to_rgba;

#[test]
fn converts_bgrx_pixel_to_rgba() {
    let bgrx = vec![0x11, 0x22, 0x33, 0x00];
    let rgba = x11_bgrx_to_rgba(&bgrx);
    assert_eq!(rgba, vec![0x33, 0x22, 0x11, 0xFF]);
}

#[test]
fn converts_multiple_pixels() {
    let bgrx = vec![
        0x11, 0x22, 0x33, 0x00,
        0xAA, 0xBB, 0xCC, 0x00,
    ];
    let rgba = x11_bgrx_to_rgba(&bgrx);
    assert_eq!(rgba, vec![
        0x33, 0x22, 0x11, 0xFF,
        0xCC, 0xBB, 0xAA, 0xFF,
    ]);
}

#[test]
fn always_sets_alpha_to_255() {
    let bgrx = vec![0x00, 0x00, 0x00, 0xFF]; // X byte is ignored
    let rgba = x11_bgrx_to_rgba(&bgrx);
    assert_eq!(rgba[3], 0xFF);
}

#[test]
fn empty_input_returns_empty() {
    assert_eq!(x11_bgrx_to_rgba(&[]), Vec::<u8>::new());
}
