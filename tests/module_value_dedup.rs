use std::collections::HashMap;
use costae::update_module_value;

#[test]
fn returns_true_when_value_is_new() {
    let mut values = HashMap::new();
    let changed = update_module_value(&mut values, "mod/cpu".to_string(), serde_json::json!("5%"));
    assert!(changed);
}

#[test]
fn returns_false_when_value_unchanged() {
    let mut values = HashMap::new();
    update_module_value(&mut values, "mod/cpu".to_string(), serde_json::json!("5%"));
    let changed = update_module_value(&mut values, "mod/cpu".to_string(), serde_json::json!("5%"));
    assert!(!changed);
}

#[test]
fn returns_true_when_value_actually_changes() {
    let mut values = HashMap::new();
    update_module_value(&mut values, "mod/cpu".to_string(), serde_json::json!("5%"));
    let changed = update_module_value(&mut values, "mod/cpu".to_string(), serde_json::json!("10%"));
    assert!(changed);
}

#[test]
fn updates_the_stored_value() {
    let mut values = HashMap::new();
    update_module_value(&mut values, "mod/cpu".to_string(), serde_json::json!("5%"));
    update_module_value(&mut values, "mod/cpu".to_string(), serde_json::json!("10%"));
    assert_eq!(values["mod/cpu"], serde_json::json!("10%"));
}

#[test]
fn different_paths_are_independent() {
    let mut values = HashMap::new();
    update_module_value(&mut values, "mod/cpu".to_string(), serde_json::json!("5%"));
    let changed = update_module_value(&mut values, "mod/mem".to_string(), serde_json::json!("5%"));
    assert!(changed);
}
