use takumi::rendering::MeasuredNode;

pub fn is_module_node(value: &serde_json::Value) -> bool {
    value.as_object().map_or(false, |o| o.contains_key("bin@"))
}

pub struct ModuleNode {
    pub path: String,
    pub bin: String,
    pub script: Option<String>,
}

pub fn find_modules(tree: &serde_json::Value) -> Vec<ModuleNode> {
    let mut out = Vec::new();
    find_modules_inner(tree, "", &mut out);
    out
}

fn find_modules_inner(value: &serde_json::Value, path: &str, out: &mut Vec<ModuleNode>) {
    if is_module_node(value) {
        let obj = value.as_object().unwrap();
        out.push(ModuleNode {
            path: path.to_string(),
            bin: obj["bin@"].as_str().unwrap_or("").to_string(),
            script: obj.get("script").and_then(|s| s.as_str()).map(str::to_string),
        });
        return; // terminal — do not recurse
    }
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                find_modules_inner(v, &format!("{}/{}", path, k), out);
            }
        }
        serde_json::Value::Array(arr) => {
            for (i, v) in arr.iter().enumerate() {
                find_modules_inner(v, &format!("{}/{}", path, i), out);
            }
        }
        _ => {}
    }
}

pub fn substitute(
    tree: &serde_json::Value,
    values: &std::collections::HashMap<String, serde_json::Value>,
) -> serde_json::Value {
    substitute_inner(tree, "", values)
}

fn substitute_inner(
    value: &serde_json::Value,
    path: &str,
    values: &std::collections::HashMap<String, serde_json::Value>,
) -> serde_json::Value {
    if is_module_node(value) {
        return values.get(path).cloned().unwrap_or(serde_json::Value::Null);
    }
    match value {
        serde_json::Value::Object(map) => {
            let new_map = map
                .iter()
                .map(|(k, v)| (k.clone(), substitute_inner(v, &format!("{}/{}", path, k), values)))
                .collect();
            serde_json::Value::Object(new_map)
        }
        serde_json::Value::Array(arr) => {
            let new_arr = arr
                .iter()
                .enumerate()
                .map(|(i, v)| substitute_inner(v, &format!("{}/{}", path, i), values))
                .collect();
            serde_json::Value::Array(new_arr)
        }
        _ => value.clone(),
    }
}

/// Walk the MeasuredNode and JSON trees in lockstep to find the deepest node
/// under (click_x, click_y) that carries an `on_click` field.
/// Returns (json_path, on_click_value) on hit, None otherwise.
/// Transforms are relative to parent, so we accumulate them as we descend.
pub fn hit_test(
    measured: &MeasuredNode,
    json: &serde_json::Value,
    click_x: f32,
    click_y: f32,
) -> Option<(String, serde_json::Value)> {
    hit_test_inner(measured, json, click_x, click_y, "")
}

fn hit_test_inner(
    measured: &MeasuredNode,
    json: &serde_json::Value,
    click_x: f32,
    click_y: f32,
    path: &str,
) -> Option<(String, serde_json::Value)> {
    // Takumi stores absolute screen coordinates in transform[4/5]
    let node_x = measured.transform[4];
    let node_y = measured.transform[5];

    if click_x < node_x || click_x > node_x + measured.width
        || click_y < node_y || click_y > node_y + measured.height
    {
        return None;
    }

    // Prefer deepest child hit first
    if let Some(children_json) = json.get("children").and_then(|c| c.as_array()) {
        for (i, (child_m, child_j)) in measured.children.iter().zip(children_json).enumerate() {
            let child_path = format!("{}/children/{}", path, i);
            if let Some(result) = hit_test_inner(child_m, child_j, click_x, click_y, &child_path) {
                return Some(result);
            }
        }
    }

    // This node is the deepest hit — return it if it has on_click
    json.get("on_click").map(|v| (path.to_string(), v.clone()))
}

/// Insert `new_value` at `path` in `values`. Returns `true` if the value actually changed
/// (i.e. was absent or different), `false` if it was already identical. Use this to avoid
/// unnecessary re-renders when a module emits the same output repeatedly.
pub fn update_module_value(
    values: &mut std::collections::HashMap<String, serde_json::Value>,
    path: String,
    new_value: serde_json::Value,
) -> bool {
    if values.get(&path) == Some(&new_value) {
        return false;
    }
    values.insert(path, new_value);
    true
}
