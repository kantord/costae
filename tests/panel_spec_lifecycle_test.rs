/// Tests for `PanelSpec<DM>` — a generic wrapper over `PanelSpecData` that
/// implements `Lifecycle<Context = DM>` for any `DM: DisplayManager`.
///
/// Behavioral claims:
/// 1. `PanelSpec<DM>` implements `Lifecycle` — compile-time proof via generic bound.
/// 2. `PanelSpec<DM>.key()` returns the inner `PanelSpecData.id`.
/// 3. `PanelContext` is an enum with at least `X11` and `Wayland` variants (constructible).
///
/// No real X11 or Wayland display connection is required for claims 2 and 3.

use costae::layout::PanelSpecData;
use costae::managed_set::Lifecycle;
use costae::PanelSpec;

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
// Claim 3: PanelContext is an enum with X11 and Wayland variants.
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
// Claim 2: PanelSpec<DM>.key() returns the inner PanelSpecData.id
// ---------------------------------------------------------------------------

/// Use a minimal no-op DM just to instantiate the struct for the key test.
struct NullDM;
impl costae::display_manager::DisplayManager for NullDM {
    type Panel = ();
    type Error = String;
    fn create_window(&mut self, _spec: &PanelSpecData) -> Result<(), String> { Ok(()) }
    fn update_position(&mut self, _panel: &mut (), _spec: &PanelSpecData) -> Result<(), String> { Ok(()) }
    fn update_dimensions(&mut self, _panel: &mut (), _spec: &PanelSpecData) -> Result<(), String> { Ok(()) }
    fn update_image(&mut self, _panel: &mut (), _bgrx: &[u8]) -> Result<(), String> { Ok(()) }
    fn delete_window(&mut self, _panel: ()) -> Result<(), String> { Ok(()) }
}

#[test]
fn panel_spec_key_returns_inner_id() {
    let spec: PanelSpec<NullDM> = PanelSpec(make_data("sidebar"), std::marker::PhantomData);
    assert_eq!(
        <PanelSpec<NullDM> as Lifecycle>::key(&spec),
        "sidebar".to_string(),
        "PanelSpec.key() must equal the inner PanelSpecData.id"
    );
}

// ---------------------------------------------------------------------------
// Claim 1: PanelSpec<DM> implements Lifecycle — compile-time proof.
// ---------------------------------------------------------------------------

/// This function is never called at runtime. Its only purpose is to prove at compile
/// time that `PanelSpec<NullDM>` satisfies the `Lifecycle` bound with the expected
/// associated types. If the impl is absent or has wrong associated types, this will
/// produce a compile error and the test suite will not build.
#[allow(dead_code)]
fn _assert_panel_spec_implements_lifecycle<T>(_spec: &T)
where
    T: Lifecycle<Key = String, Context = NullDM>,
{
    // Body intentionally empty — compile-time check only.
}

#[allow(dead_code)]
fn _call_with_panel_spec(spec: &PanelSpec<NullDM>) {
    _assert_panel_spec_implements_lifecycle(spec);
}

#[test]
fn panel_spec_lifecycle_impl_compiles() {
    // If `_call_with_panel_spec` compiled, the impl exists with the correct associated types.
    // This test passes as long as the crate compiles.
    let _spec: PanelSpec<NullDM> = PanelSpec(make_data("test"), std::marker::PhantomData);
}
