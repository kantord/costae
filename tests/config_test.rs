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
    assert_eq!(config.config, BarConfig { width: 300 });
}

#[test]
fn layout_is_parsed_as_json_value() {
    let config = load_config(&fixture("config.yaml")).expect("should load config");
    assert_eq!(config.layout["type"], "container");
}

#[test]
fn layout_preserves_at_module_reference() {
    let config = load_config(&fixture("config.yaml")).expect("should load config");
    let children = config.layout["children"].as_array().expect("children should be array");
    assert_eq!(children[0]["type"], "@~/.config/costae/modules/workspaces");
}

#[test]
fn missing_file_returns_error() {
    let result = load_config(&fixture("nonexistent.yaml"));
    assert!(result.is_err());
}
