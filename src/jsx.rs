use std::path::Path;
use std::sync::{Arc, Mutex};

use oxc_allocator::Allocator;
use oxc_codegen::Codegen;
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::SourceType;
use oxc_transformer::{JsxOptions, JsxRuntime, TransformOptions, Transformer};

pub fn eval_jsx(source: &str, ctx: serde_json::Value, stream_values: &std::collections::HashMap<String, String>) -> rquickjs::Result<(serde_json::Value, Vec<(String, Option<String>)>, Vec<String>)> {
    let js = transform_jsx(source);
    let runtime = rquickjs::Runtime::new()?;
    let qjs_ctx = rquickjs::Context::full(&runtime)?;
    let sv = Arc::new(stream_values.clone());
    let calls: Arc<Mutex<Vec<(String, Option<String>)>>> = Arc::new(Mutex::new(Vec::new()));
    let module_calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    qjs_ctx.with(|qjs_ctx| {
        qjs_ctx.eval::<(), _>(r#"
            globalThis._jsx = (tag, props, ...children) => {
                if (typeof tag === 'function') {
                    return tag({ ...props, children: children.flat() });
                }
                if (tag === 'text') {
                    const flat = children.flat();
                    const text = flat.length === 1 && flat[0] !== null && typeof flat[0] === 'object'
                        ? flat[0]
                        : flat.join('');
                    return { type: tag, ...props, text };
                }
                return { type: tag, ...props, children: children.flat() };
            };
            globalThis.useJSONStream = (bin, script) => {
                const str = useStringStream(bin, script);
                if (!str) return null;
                try { return JSON.parse(str); } catch { return null; }
            };
            globalThis.Module = ({ bin, children, ...rest }) => {
                const child = Array.isArray(children) ? children[0] : children;
                if (typeof child === 'function') {
                    registerModule(bin);
                    const data = useJSONStream(bin);
                    const events = new Proxy({}, {
                        get: (_, type) => ({ __channel__: bin, type: String(type) })
                    });
                    return child(data, events);
                }
                return { "bin@": bin, ...rest };
            };
        "#)?;
        {
            let sv = Arc::clone(&sv);
            let calls_inner = Arc::clone(&calls);
            let func = rquickjs::Function::new(qjs_ctx.clone(), move |bin: String, script: Option<String>| {
                calls_inner.lock().unwrap().push((bin.clone(), script.clone()));
                let key = format!("{}\0{}", bin, script.unwrap_or_default());
                sv.get(&key).cloned().unwrap_or_default()
            })?;
            qjs_ctx.globals().set("useStringStream", func)?;
        }
        {
            let module_calls_inner = Arc::clone(&module_calls);
            let func = rquickjs::Function::new(qjs_ctx.clone(), move |bin: String| {
                let mut mc = module_calls_inner.lock().unwrap();
                if !mc.contains(&bin) {
                    mc.push(bin);
                }
            })?;
            qjs_ctx.globals().set("registerModule", func)?;
        }
        if !ctx.is_null() {
            let json_string = serde_json::to_string(&ctx).map_err(|_| rquickjs::Error::Unknown)?;
            qjs_ctx.eval::<(), _>(format!("globalThis.ctx = {json_string};").as_str())?;
        }
        let value: rquickjs::Value = qjs_ctx.eval(js.as_str())?;
        let json_str = qjs_ctx
            .json_stringify(value)?
            .ok_or(rquickjs::Error::Unknown)?
            .to_string()?;
        let json_value = serde_json::from_str(&json_str).map_err(|_| rquickjs::Error::Unknown)?;
        let recorded = calls.lock().unwrap().clone();
        let recorded_modules = module_calls.lock().unwrap().clone();
        Ok((json_value, recorded, recorded_modules))
    })
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
        streams.insert("/usr/bin/bash\0echo hi".to_string(), "hello".to_string());
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
        streams.insert("/usr/bin/test\0".to_string(), r#"{"name":"hello"}"#.to_string());
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
        assert!(module_calls.contains(&"/usr/bin/test-module".to_string()));
    }
}
