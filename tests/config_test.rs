use std::path::PathBuf;

use costae::{BarConfig, load_config};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

#[test]
fn loads_bar_width_from_config_section() {
    let config = load_config(&fixture("config.yaml")).expect("should load config");
    assert_eq!(config.config, BarConfig { width: 300, outer_gap: 0 });
}

#[test]
fn layout_is_parsed_as_json_value() {
    let config = load_config(&fixture("config.yaml")).expect("should load config");
    assert_eq!(config.layout.as_ref().unwrap()["type"], "container");
}

#[test]
fn layout_preserves_at_module_reference() {
    let config = load_config(&fixture("config.yaml")).expect("should load config");
    let children = config.layout.as_ref().unwrap()["children"].as_array().expect("children should be array");
    assert_eq!(children[0]["type"], "@~/.config/costae/modules/workspaces");
}

#[test]
fn missing_file_returns_error() {
    let result = load_config(&fixture("nonexistent.yaml"));
    assert!(result.is_err());
}

#[test]
fn outer_gap_defaults_to_zero_when_absent() {
    let config = load_config(&fixture("config.yaml")).expect("should load config");
    assert_eq!(config.config.outer_gap, 0);
}

#[test]
fn outer_gap_is_parsed_from_config_section() {
    let config = load_config(&fixture("config_with_outer_gap.yaml")).expect("should load config");
    assert_eq!(config.config.outer_gap, 8);
}
