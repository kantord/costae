use std::collections::HashMap;

use costae::{find_modules, is_module_node, substitute};

// --- is_module_node ---

#[test]
fn object_with_bin_at_key_is_module_node() {
    let v = serde_json::json!({"bin@": "/usr/bin/bash", "script": "echo hi"});
    assert!(is_module_node(&v));
}

#[test]
fn object_without_bin_at_key_is_not_module_node() {
    let v = serde_json::json!({"type": "text", "text": "hello"});
    assert!(!is_module_node(&v));
}

#[test]
fn scalar_is_not_module_node() {
    assert!(!is_module_node(&serde_json::json!("hello")));
    assert!(!is_module_node(&serde_json::json!(42)));
    assert!(!is_module_node(&serde_json::json!(null)));
}

// --- substitute ---

#[test]
fn substitute_leaves_static_tree_unchanged() {
    let tree = serde_json::json!({"type": "text", "text": "hello"});
    let result = substitute(&tree, &HashMap::new());
    assert_eq!(result, tree);
}

#[test]
fn substitute_replaces_module_node_with_current_value() {
    let tree = serde_json::json!({
        "type": "text",
        "text": {"bin@": "/usr/bin/bash", "script": "date"}
    });
    let mut values = HashMap::new();
    values.insert("/text".to_string(), serde_json::json!("12:34:56"));
    let result = substitute(&tree, &values);
    assert_eq!(result["type"], "text");
    assert_eq!(result["text"], "12:34:56");
}

#[test]
fn substitute_uses_null_for_module_with_no_current_value() {
    let tree = serde_json::json!({
        "type": "text",
        "text": {"bin@": "/usr/bin/bash"}
    });
    let result = substitute(&tree, &HashMap::new());
    assert_eq!(result["text"], serde_json::json!(null));
}

#[test]
fn substitute_does_not_recurse_into_module_node() {
    let tree = serde_json::json!({
        "type": "container",
        "children": [
            {"bin@": "/usr/bin/bash", "script": "echo hi"}
        ]
    });
    let mut values = HashMap::new();
    values.insert("/children/0".to_string(), serde_json::json!("replaced"));
    let result = substitute(&tree, &values);
    assert_eq!(result["children"][0], "replaced");
}

#[test]
fn substitute_handles_module_in_array() {
    let tree = serde_json::json!([
        {"bin@": "/usr/bin/bash"},
        {"type": "text", "text": "static"}
    ]);
    let mut values = HashMap::new();
    values.insert("/0".to_string(), serde_json::json!("dynamic"));
    let result = substitute(&tree, &values);
    assert_eq!(result[0], "dynamic");
    assert_eq!(result[1]["text"], "static");
}

// --- find_modules ---

#[test]
fn find_modules_returns_empty_for_static_tree() {
    let tree = serde_json::json!({"type": "text", "text": "hello"});
    assert!(find_modules(&tree).is_empty());
}

#[test]
fn find_modules_finds_nested_module() {
    let tree = serde_json::json!({
        "type": "text",
        "text": {"bin@": "/usr/bin/bash", "script": "echo hi"}
    });
    let modules = find_modules(&tree);
    assert_eq!(modules.len(), 1);
    assert_eq!(modules[0].path, "/text");
    assert_eq!(modules[0].bin, "/usr/bin/bash");
    assert_eq!(modules[0].script, Some("echo hi".to_string()));
}

#[test]
fn find_modules_finds_multiple_modules() {
    let tree = serde_json::json!({
        "type": "container",
        "children": [
            {"bin@": "/usr/bin/bash", "script": "date"},
            {"type": "text", "text": "static"},
            {"bin@": "/usr/bin/python3", "script": "print('hi')"}
        ]
    });
    let modules = find_modules(&tree);
    assert_eq!(modules.len(), 2);
    assert_eq!(modules[0].path, "/children/0");
    assert_eq!(modules[1].path, "/children/2");
}

#[test]
fn find_modules_does_not_recurse_into_module_node() {
    // A module node is terminal — we should not find sub-modules inside it
    let tree = serde_json::json!({
        "bin@": "/usr/bin/bash",
        "script": {"bin@": "/should/not/be/found"}
    });
    let modules = find_modules(&tree);
    assert_eq!(modules.len(), 1);
    assert_eq!(modules[0].path, "");
}
