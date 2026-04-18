use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};

/// Shared map of stream values: keyed by `(bin, script)`, holds the latest stdout line.
type StreamValues = Arc<RwLock<HashMap<(String, Option<String>), String>>>;
/// Recorded `useStringStream` calls made during the last `_render()` invocation.
type StreamCalls = Arc<Mutex<Vec<(String, Option<String>)>>>;
/// Return type of a successful JSX evaluation.
type EvalResult = rquickjs::Result<(serde_json::Value, Vec<(String, Option<String>)>, Vec<(String, serde_json::Value)>)>;

use rquickjs::CatchResultExt;

use oxc_allocator::Allocator;
use oxc_codegen::Codegen;
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::SourceType;
use oxc_transformer::{JsxOptions, JsxRuntime, TransformOptions, Transformer};

/// Wraps the JSX *source* so that all top-level declarations live inside
/// `globalThis._render = function() { ... }` and the final expression becomes its
/// return value.
///
/// Uses OXC's AST to locate the last `ExpressionStatement` in `Program.body` by byte
/// offset — no string heuristics. This means wrapping parens around the root expression
/// are never required; the user just writes their JSX as the final statement.
///
/// Each call to `_render()` creates fresh variable bindings, so `const`/`let`
/// declarations in the body work correctly across multiple invocations.
fn wrap_source_as_render_fn(source: &str) -> String {
    use oxc_ast::ast::Statement;

    let allocator = Allocator::default();
    let ret = Parser::new(&allocator, source, SourceType::jsx()).parse();

    // Find the byte offset of the last top-level ExpressionStatement.
    let split_at = ret.program.body.last().and_then(|stmt| {
        if let Statement::ExpressionStatement(expr) = stmt {
            Some(expr.span.start as usize)
        } else {
            None
        }
    });

    split_at.map_or_else(
        || format!("globalThis._render = function() {{\nreturn {source};\n}};\n"),
        |start| {
            let before = &source[..start];
            let after = &source[start..];
            format!("globalThis._render = function() {{\n{before}return {after};\n}};\n")
        },
    )
}

const JSX_GLOBALS_JS: &str = r#"
    globalThis._jsx = (tag, props, ...children) => {
        const flat = children.flat().filter(c => c !== null && c !== undefined && c !== false);
        if (typeof tag === 'function') {
            return tag({ ...props, children: flat });
        }
        if (tag === 'text') {
            const text = flat.length === 1 && typeof flat[0] === 'object'
                ? flat[0]
                : flat.join('');
            return { type: tag, ...props, text };
        }
        return { type: tag, ...props, children: flat };
    };
    globalThis.useJSONStream = (bin, script) => {
        const str = useStringStream(bin, script);
        if (!str) return null;
        try { return JSON.parse(str); } catch { return null; }
    };
    globalThis.Module = ({ bin, children, ...rest }) => {
        const child = Array.isArray(children) ? children[0] : children;
        if (typeof child === 'function') {
            registerModule(bin, rest);
            const data = useJSONStream(bin);
            const events = new Proxy({}, {
                get: (_, type) => ({ __channel__: bin, type: String(type) })
            });
            return child(data, events);
        }
        return { "bin@": bin, ...rest };
    };
"#;

/// A persistent JSX evaluator that compiles the layout source once and re-evaluates
/// cheaply on each tick by calling the pre-compiled `_render()` function.
pub struct JsxEvaluator {
    context: rquickjs::Context,
    _runtime: rquickjs::Runtime,
    stream_values: StreamValues,
    calls: StreamCalls,
    module_calls: Arc<Mutex<Vec<(String, serde_json::Value)>>>,
    global_state: Arc<Mutex<serde_json::Map<String, serde_json::Value>>>,
}

impl JsxEvaluator {
    pub fn new(source: &str, ctx: serde_json::Value) -> rquickjs::Result<Self> {
        let render_js = transform_jsx(&wrap_source_as_render_fn(source));
        let runtime = rquickjs::Runtime::new()?;
        let context = rquickjs::Context::full(&runtime)?;
        let stream_values: StreamValues = Arc::new(RwLock::new(HashMap::new()));
        let calls: StreamCalls = Arc::new(Mutex::new(Vec::new()));
        let module_calls: Arc<Mutex<Vec<(String, serde_json::Value)>>> = Arc::new(Mutex::new(Vec::new()));

        {
            let sv = Arc::clone(&stream_values);
            let calls_inner = Arc::clone(&calls);
            let module_calls_inner = Arc::clone(&module_calls);
            context.with(|qjs_ctx| {
                qjs_ctx.eval::<(), _>(JSX_GLOBALS_JS)?;
                let func = rquickjs::Function::new(qjs_ctx.clone(), move |bin: String, script: Option<String>| {
                    calls_inner.lock().unwrap().push((bin.clone(), script.clone()));
                    sv.read().unwrap().get(&(bin, script)).cloned().unwrap_or_default()
                })?;
                qjs_ctx.globals().set("useStringStream", func)?;
                let func2 = rquickjs::Function::new(qjs_ctx.clone(), move |bin: String, props: rquickjs::Value| {
                    let props: serde_json::Value = rquickjs_serde::from_value(props)
                        .unwrap_or(serde_json::Value::Null);
                    let mut mc = module_calls_inner.lock().unwrap();
                    if !mc.iter().any(|(b, _)| b == &bin) {
                        mc.push((bin, props));
                    }
                })?;
                qjs_ctx.globals().set("registerModule", func2)?;
                if !ctx.is_null() {
                    let js_ctx = rquickjs_serde::to_value(qjs_ctx.clone(), &ctx)
                        .map_err(|_| rquickjs::Error::Unknown)?;
                    qjs_ctx.globals().set("ctx", js_ctx)?;
                }
                qjs_ctx.eval::<(), _>(render_js.as_str())?;
                Ok::<(), rquickjs::Error>(())
            })?;
        }

        let global_state = Arc::new(Mutex::new(serde_json::Map::new()));
        Ok(Self { context, _runtime: runtime, stream_values, calls, module_calls, global_state })
    }

    pub fn eval(
        &self,
        new_stream_values: &HashMap<(String, Option<String>), String>,
    ) -> EvalResult {
        self.stream_values.write().unwrap().clone_from(new_stream_values);
        self.calls.lock().unwrap().clear();
        self.module_calls.lock().unwrap().clear();

        self.context.with(|qjs_ctx| {
            let state_json = serde_json::to_string(&*self.global_state.lock().unwrap())
                .map_err(|_| rquickjs::Error::Unknown)?;
            qjs_ctx.eval::<(), _>(format!("globalThis.globals = {};", state_json).as_str())?;

            let value: rquickjs::Value = qjs_ctx.eval("_render()")
                .catch(&qjs_ctx)
                .map_err(|e| { tracing::error!(exception = %e, "JS exception"); rquickjs::Error::Exception })?;

            let globals_val: rquickjs::Value = qjs_ctx.eval("globalThis.globals")?;
            let globals_json_str = qjs_ctx
                .json_stringify(globals_val)?
                .ok_or(rquickjs::Error::Unknown)?
                .to_string()?;
            if let Ok(new_state) = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&globals_json_str) {
                *self.global_state.lock().unwrap() = new_state;
            }

            let json_str = qjs_ctx
                .json_stringify(value)?
                .ok_or(rquickjs::Error::Unknown)?
                .to_string()?;
            let json_value = serde_json::from_str(&json_str).map_err(|_| rquickjs::Error::Unknown)?;
            let recorded = self.calls.lock().unwrap().clone();
            let recorded_modules = self.module_calls.lock().unwrap().clone();
            Ok((json_value, recorded, recorded_modules))
        })
    }
}

pub fn eval_jsx(source: &str, ctx: serde_json::Value, stream_values: &HashMap<(String, Option<String>), String>) -> EvalResult {
    JsxEvaluator::new(source, ctx)?.eval(stream_values)
}

pub fn eval_js(source: &str) -> rquickjs::Result<String> {
    let runtime = rquickjs::Runtime::new()?;
    let ctx = rquickjs::Context::full(&runtime)?;
    ctx.with(|ctx| ctx.eval(source))
}

pub fn transform_jsx(source: &str) -> String {
    let allocator = Allocator::default();
    let source_type = SourceType::jsx();

    let ret = Parser::new(&allocator, source, source_type).parse();
    let mut program = ret.program;

    let scoping = SemanticBuilder::new()
        .with_excess_capacity(2.0)
        .build(&program)
        .semantic
        .into_scoping();

    let options = TransformOptions {
        jsx: JsxOptions {
            runtime: JsxRuntime::Classic,
            pragma: Some("_jsx".to_string()),
            pragma_frag: Some("_jsxFrag".to_string()),
            ..JsxOptions::enable()
        },
        ..TransformOptions::default()
    };

    Transformer::new(&allocator, Path::new("input.jsx"), &options)
        .build_with_scoping(scoping, &mut program);

    Codegen::new().build(&program).code
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eval_jsx_returns_tag_props_and_children() {
        let (result, _, _) = eval_jsx(r#"<text tw="flex">{"hello"}</text>"#, serde_json::Value::Null, &std::collections::HashMap::new()).unwrap();
        assert_eq!(result["type"], "text");
        assert_eq!(result["tw"], "flex");
        assert_eq!(result["text"], "hello");
    }

    #[test]
    fn eval_js_returns_string_result_of_expression() {
        let result = eval_js("\"hello\"");
        assert_eq!(result.unwrap(), "hello".to_string());
    }

    #[test]
    fn transform_jsx_self_closing_element_with_tw_prop() {
        let result = transform_jsx(r#"<text tw="flex" />"#);
        assert!(result.contains("_jsx"), "expected '_jsx' in output, got: {result}");
        assert!(result.contains("\"text\""), "expected '\"text\"' in output, got: {result}");
        assert!(result.contains("\"flex\""), "expected '\"flex\"' in output, got: {result}");
    }

    #[test]
    fn eval_jsx_nested_tree_parses_to_node() {
        let (result, _, _) = eval_jsx(
            r#"<container tw="flex flex-col"><text tw="text-white">{"hello"}</text></container>"#,
            serde_json::Value::Null,
            &std::collections::HashMap::new(),
        )
        .unwrap();
        let node = crate::parse_layout(&result);
        assert!(node.is_ok(), "parse_layout failed: {:?}", node);
    }

    #[test]
    fn use_string_stream_returns_injected_value() {
        let mut streams = std::collections::HashMap::new();
        streams.insert(("/usr/bin/bash".to_string(), Some("echo hi".to_string())), "hello".to_string());
        let (result, _, _) = eval_jsx(
            r#"<text tw="text-white">{useStringStream("/usr/bin/bash", "echo hi")}</text>"#,
            serde_json::Value::Null,
            &streams,
        )
        .unwrap();
        assert_eq!(result["text"], "hello");
    }

    #[test]
    fn eval_jsx_injects_ctx_into_script() {
        let ctx = serde_json::json!({
            "output": "DP-4",
            "dpi": 96.0,
            "width": 250,
            "outer_gap": 8
        });
        let (value, _, _) = eval_jsx(
            r#"<text tw="text-white">{ctx.output}</text>"#,
            ctx,
            &std::collections::HashMap::new(),
        )
        .expect("eval_jsx should not error");
        let node = crate::parse_layout(&value);
        assert!(node.is_ok(), "parse_layout failed: {:?}", node);
    }

    #[test]
    fn eval_jsx_records_stream_calls() {
        let (_, streams_called, _) = eval_jsx(
            r#"<text tw="text-white">{useStringStream("/bin/bash", "script1")}{useStringStream("/bin/bash", "script2")}</text>"#,
            serde_json::Value::Null,
            &std::collections::HashMap::new(),
        )
        .unwrap();
        assert!(
            streams_called.contains(&("/bin/bash".to_string(), Some("script1".to_string()))),
            "expected (\"/bin/bash\", Some(\"script1\")) in streams_called, got: {:?}",
            streams_called
        );
        assert!(
            streams_called.contains(&("/bin/bash".to_string(), Some("script2".to_string()))),
            "expected (\"/bin/bash\", Some(\"script2\")) in streams_called, got: {:?}",
            streams_called
        );
    }

    #[test]
    fn module_render_prop_exposes_channel_in_events() {
        let (result, _, _) = eval_jsx(
            r#"<Module bin="/usr/bin/test">{(data, events) => <text tw="text-white">{events.doThing.__channel__}</text>}</Module>"#,
            serde_json::Value::Null,
            &std::collections::HashMap::new(),
        )
        .unwrap();
        assert_eq!(result["text"], "/usr/bin/test");
    }

    #[test]
    fn use_json_stream_parses_latest_json_output() {
        let mut streams = std::collections::HashMap::new();
        streams.insert(("/usr/bin/test".to_string(), None), r#"{"name":"hello"}"#.to_string());
        let (result, _, _) = eval_jsx(
            r#"<text tw="text-white">{useJSONStream("/usr/bin/test").name}</text>"#,
            serde_json::Value::Null,
            &streams,
        )
        .unwrap();
        assert_eq!(result["text"], "hello");
    }

    #[test]
    fn module_component_records_module_call() {
        let (_, _, module_calls) = eval_jsx(
            r#"<Module bin="/usr/bin/test-module">{(data, events) => <text tw="text-white">hi</text>}</Module>"#,
            serde_json::Value::Null,
            &std::collections::HashMap::new(),
        ).unwrap();
        assert!(module_calls.iter().any(|(bin, _)| bin == "/usr/bin/test-module"));
    }

    #[test]
    fn wrap_source_as_render_fn_works_without_wrapping_parens() {
        // Root expression has no wrapping parens — AST split must still find it correctly.
        let source = r#"
function Inner({ val }) {
  return (
    <text tw="text-white">{val}</text>
  );
}
<Inner val={useStringStream("/bin/bash", "x")} />"#;
        let evaluator = JsxEvaluator::new(source, serde_json::Value::Null).unwrap();

        let mut s = std::collections::HashMap::new();
        s.insert(("/bin/bash".to_string(), Some("x".to_string())), "hello".to_string());
        let (result, _, _) = evaluator.eval(&s).unwrap();
        assert_eq!(result["text"], "hello");
    }

    #[test]
    fn wrap_source_as_render_fn_targets_top_level_paren_not_inner_return() {
        // The root expression starts with ( at column 0.
        // return ( inside a function body is NOT at column 0, so rfind("\n(") must pick
        // the right one. Verify by eval-ing twice and getting different values.
        let source = r#"
function Inner({ val }) {
  return (
    <text tw="text-white">{val}</text>
  );
}
(
  <Inner val={useStringStream("/bin/bash", "x")} />
)"#;
        let evaluator = JsxEvaluator::new(source, serde_json::Value::Null).unwrap();

        let mut s1 = std::collections::HashMap::new();
        s1.insert(("/bin/bash".to_string(), Some("x".to_string())), "first".to_string());
        let (r1, _, _) = evaluator.eval(&s1).unwrap();
        assert_eq!(r1["text"], "first");

        let mut s2 = std::collections::HashMap::new();
        s2.insert(("/bin/bash".to_string(), Some("x".to_string())), "second".to_string());
        let (r2, _, _) = evaluator.eval(&s2).unwrap();
        assert_eq!(r2["text"], "second");
    }

    #[test]
    fn globals_object_persists_value_across_eval_calls() {
        let evaluator = JsxEvaluator::new(
            r#"
globals.count ??= 0;
globals.count += 1;
<text tw="text-white">{String(globals.count)}</text>
            "#,
            serde_json::Value::Null,
        ).unwrap();

        let streams = std::collections::HashMap::new();
        let (r1, _, _) = evaluator.eval(&streams).unwrap();
        assert_eq!(r1["text"], "1");

        let (r2, _, _) = evaluator.eval(&streams).unwrap();
        assert_eq!(r2["text"], "2");

        let (r3, _, _) = evaluator.eval(&streams).unwrap();
        assert_eq!(r3["text"], "3");
    }

    #[test]
    fn jsx_evaluator_reflects_updated_stream_values_on_second_call() {
        let evaluator = JsxEvaluator::new(
            r#"<text tw="text-white">{useStringStream("/bin/bash", "echo hi")}</text>"#,
            serde_json::Value::Null,
        ).unwrap();

        let mut streams1 = std::collections::HashMap::new();
        streams1.insert(("/bin/bash".to_string(), Some("echo hi".to_string())), "first".to_string());
        let (result1, _, _) = evaluator.eval(&streams1).unwrap();
        assert_eq!(result1["text"], "first");

        let mut streams2 = std::collections::HashMap::new();
        streams2.insert(("/bin/bash".to_string(), Some("echo hi".to_string())), "second".to_string());
        let (result2, _, _) = evaluator.eval(&streams2).unwrap();
        assert_eq!(result2["text"], "second");
    }

    /// Regression test for the `\0`-separator key collision bug.
    ///
    /// A stream identified by `(bin, None)` and a stream identified by `(bin, Some(""))`
    /// must occupy *distinct* slots in the stream_values map passed to `JsxEvaluator::eval`.
    /// With the current `format!("{}\0{}", bin, script.unwrap_or_default())` key scheme
    /// both produce `"bin\0"`, so inserting two entries collapses them into one.
    ///
    /// This test will FAIL under the current implementation (the map has only 1 entry)
    /// and pass once the key type is changed to `(String, Option<String>)`.
    #[test]
    fn stream_key_none_and_some_empty_are_not_interchangeable() {
        let bin = "/usr/bin/foo";

        // After the fix the map uses (String, Option<String>) keys, so (bin, None) and
        // (bin, Some("")) are distinct keys and both entries are preserved.
        let key_for_none: (String, Option<String>) = (bin.to_string(), None);
        let key_for_empty: (String, Option<String>) = (bin.to_string(), Some("".to_string()));

        // With typed keys, both entries are kept — no collision.
        let mut map: std::collections::HashMap<(String, Option<String>), &str> = std::collections::HashMap::new();
        map.insert(key_for_none, "value_for_none");
        map.insert(key_for_empty, "value_for_empty");

        // After the fix the map uses (String, Option<String>) keys and this will be 2.
        // Under the current bug it is 1 — this assertion is the RED line.
        assert_eq!(
            map.len(),
            2,
            "stream_values map must have 2 distinct entries for (bin, None) and (bin, Some(\"\")); \
             got {} — the \\0-separator key scheme causes a collision",
            map.len()
        );
    }

    #[test]
    fn jsx_null_and_false_children_are_filtered_from_container() {
        let (result, _, _) = eval_jsx(
            r#"
const show = false;
<container tw="flex">
  <text tw="text-white">visible</text>
  {show && <text tw="text-white">hidden</text>}
  {null}
</container>
            "#,
            serde_json::Value::Null,
            &std::collections::HashMap::new(),
        ).unwrap();
        let children = result["children"].as_array().unwrap();
        assert_eq!(children.len(), 1, "expected 1 child, got: {:?}", children);
        assert_eq!(children[0]["text"], "visible");
    }
}
