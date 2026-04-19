/// Tests for the PanelSpec platform-tagging redesign.
///
/// Behavioral claims:
/// - `PanelSpec::for_context(data, &ctx)` with a `DisplayContext::X11` produces `PanelSpec::X11(_)`
/// - `PanelSpec::for_context(data, &ctx)` with a `DisplayContext::Wayland` produces `PanelSpec::Wayland(_)`
/// - Wrapping `PanelSpecData` in a `PanelSpec` variant preserves all field values intact
///
/// No real X11 or Wayland display connection is required. Tests use
/// `DisplayContext::test_x11()` / `DisplayContext::test_wayland()` stub constructors
/// that create variants without a real connection. If those constructors don't exist yet,
/// the implementer must add `#[cfg(test)]` stubs to `DisplayContext`.
///
/// FINDING FOR IMPLEMENTER: `DisplayContext::X11` holds a live connection (`PanelContext`)
/// which requires a real X11 display. The implementer must add `#[cfg(test)]` stub
/// constructors (`DisplayContext::test_x11()` and `DisplayContext::test_wayland()`) that
/// build the variants without a real display. Alternatively, if `DisplayContext` variants
/// carry no connection state (only a tag), the variants are directly constructible as
/// `DisplayContext::X11` / `DisplayContext::Wayland`.

use costae::layout::{PanelSpec, PanelSpecData};
use costae::windowing::DisplayContext;

/// Build a minimal `PanelSpecData` with known field values for assertions.
fn make_data() -> PanelSpecData {
    PanelSpecData {
        id: "test-panel".to_string(),
        anchor: None,
        width: 300,
        height: 1080,
        x: 10,
        y: 20,
        outer_gap: 4,
        output: Some("DP-1".to_string()),
        above: false,
        content: serde_json::Value::Null,
    }
}

#[test]
fn for_context_with_x11_context_produces_x11_variant() {
    let data = make_data();
    let ctx = DisplayContext::test_x11();
    let spec = PanelSpec::for_context(data, &ctx);
    assert!(
        matches!(spec, PanelSpec::X11(_)),
        "expected PanelSpec::X11(_), got a different variant"
    );
}

#[test]
fn for_context_with_wayland_context_produces_wayland_variant() {
    let data = make_data();
    let ctx = DisplayContext::test_wayland();
    let spec = PanelSpec::for_context(data, &ctx);
    assert!(
        matches!(spec, PanelSpec::Wayland(_)),
        "expected PanelSpec::Wayland(_), got a different variant"
    );
}

#[test]
fn panel_spec_data_fields_are_accessible_through_variant() {
    let data = make_data();
    let ctx = DisplayContext::test_x11();
    let spec = PanelSpec::for_context(data, &ctx);

    let PanelSpec::X11(inner) = spec else {
        panic!("expected PanelSpec::X11(_)");
    };

    assert_eq!(inner.id, "test-panel");
    assert_eq!(inner.width, 300);
    assert_eq!(inner.height, 1080);
    assert_eq!(inner.x, 10);
    assert_eq!(inner.y, 20);
    assert_eq!(inner.outer_gap, 4);
    assert_eq!(inner.output, Some("DP-1".to_string()));
    assert_eq!(inner.above, false);
    assert_eq!(inner.anchor, None);
}
