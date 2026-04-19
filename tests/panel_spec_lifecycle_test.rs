/// Tests for cycle 6: `impl Lifecycle for PanelSpec` with unified `PanelContext`.
///
/// Behavioral claims:
/// 1. `PanelSpec` implements `Lifecycle` — compile-time proof via coercion to trait object.
/// 2. `PanelSpec::X11(data).key()` returns `data.id`.
/// 3. `PanelSpec::Wayland(data).key()` returns `data.id`.
/// 4. `PanelContext` is an enum with at least `X11` and `Wayland` variants (constructible).
///
/// No real X11 or Wayland display connection is required.
/// Tests use only struct construction and trait method calls that do not touch I/O.

use costae::layout::{PanelSpec, PanelSpecData};
use costae::managed_set::Lifecycle;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_data(id: &str) -> PanelSpecData {
    PanelSpecData {
        id: id.to_string(),
        anchor: None,
        width: 200,
        height: 40,
        x: 0,
        y: 0,
        outer_gap: 0,
        output: None,
        above: false,
        content: serde_json::Value::Null,
    }
}

// ---------------------------------------------------------------------------
// Claim 4: PanelContext is an enum with X11 and Wayland variants.
// ---------------------------------------------------------------------------

/// `PanelContext::X11` and `PanelContext::Wayland` must be constructible without a
/// live display connection. The types they wrap (`X11PanelContext` / `WaylandPanelContext`)
/// may be unit structs or opaque structs; what matters here is that the enum variants exist.
#[test]
fn panel_context_enum_has_x11_and_wayland_variants() {
    use costae::panel::PanelContext;

    // Both variants must be constructible. Pattern-match to prove they are distinct.
    let x11_ctx = PanelContext::X11(costae::panel::X11PanelContext::test_stub());
    let wayland_ctx = PanelContext::Wayland(costae::panel::WaylandPanelContext::test_stub());

    assert!(
        matches!(x11_ctx, PanelContext::X11(_)),
        "PanelContext::X11 variant must match itself"
    );
    assert!(
        matches!(wayland_ctx, PanelContext::Wayland(_)),
        "PanelContext::Wayland variant must match itself"
    );
}

// ---------------------------------------------------------------------------
// Claim 2: PanelSpec::X11(data).key() == data.id
// ---------------------------------------------------------------------------

#[test]
fn panel_spec_x11_key_returns_inner_id() {
    let data = make_data("sidebar");
    let spec = PanelSpec::X11(data);

    assert_eq!(
        spec.key(),
        "sidebar".to_string(),
        "PanelSpec::X11(data).key() must equal data.id"
    );
}

// ---------------------------------------------------------------------------
// Claim 3: PanelSpec::Wayland(data).key() == data.id
// ---------------------------------------------------------------------------

#[test]
fn panel_spec_wayland_key_returns_inner_id() {
    let data = make_data("topbar");
    let spec = PanelSpec::Wayland(data);

    assert_eq!(
        spec.key(),
        "topbar".to_string(),
        "PanelSpec::Wayland(data).key() must equal data.id"
    );
}

// ---------------------------------------------------------------------------
// Claim 1: PanelSpec implements Lifecycle — compile-time proof via generic bound.
// ---------------------------------------------------------------------------

/// This function is never called at runtime. Its only purpose is to prove at compile
/// time that `PanelSpec` satisfies the `Lifecycle` bound with the expected associated
/// types. If `impl Lifecycle for PanelSpec` does not exist (or uses wrong associated
/// types), this will produce a compile error and the test suite will not build.
///
/// We use a generic function constrained to `Lifecycle<Key=String, Context=PanelContext>`
/// because `Lifecycle` is not dyn-compatible (exit takes `state: Self::State` by value).
#[allow(dead_code)]
fn _assert_panel_spec_implements_lifecycle<T>(_spec: &T)
where
    T: Lifecycle<Key = String, Context = costae::panel::PanelContext>,
{
    // Body intentionally empty — compile-time check only.
}

#[allow(dead_code)]
fn _call_with_panel_spec(spec: &PanelSpec) {
    _assert_panel_spec_implements_lifecycle(spec);
}

#[test]
fn panel_spec_lifecycle_impl_compiles() {
    // If `_call_with_panel_spec` compiled (it references `_assert_panel_spec_implements_lifecycle`
    // instantiated with `PanelSpec`), the impl exists with the correct associated types.
    // This test is a placeholder that passes once the crate compiles.
    let _ = std::marker::PhantomData::<PanelSpec>::default;
}
