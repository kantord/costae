use std::path::Path;

use oxc_allocator::Allocator;
use oxc_codegen::Codegen;
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::SourceType;
use oxc_transformer::{JsxOptions, JsxRuntime, TransformOptions, Transformer};

pub fn eval_jsx(source: &str, ctx: serde_json::Value) -> rquickjs::Result<serde_json::Value> {
    let js = transform_jsx(source);
    let runtime = rquickjs::Runtime::new()?;
    let qjs_ctx = rquickjs::Context::full(&runtime)?;
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
            globalThis.useStringStream = (bin, script) => ({ "bin@": bin, script });
            globalThis.useJSONStream = (bin, script) => ({ "bin@": bin, script });
            globalThis.Module = ({ bin, children, ...rest }) => ({ "bin@": bin, ...rest });
        "#)?;
        if !ctx.is_null() {
            let json_string = serde_json::to_string(&ctx).map_err(|_| rquickjs::Error::Unknown)?;
            qjs_ctx.eval::<(), _>(format!("globalThis.ctx = {json_string};").as_str())?;
        }
        let value: rquickjs::Value = qjs_ctx.eval(js.as_str())?;
        let json_str = qjs_ctx
            .json_stringify(value)?
            .ok_or(rquickjs::Error::Unknown)?
            .to_string()?;
        serde_json::from_str(&json_str).map_err(|_| rquickjs::Error::Unknown)
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
        let result = eval_jsx(r#"<text tw="flex">{"hello"}</text>"#, serde_json::Value::Null).unwrap();
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
        let result = eval_jsx(
            r#"<container tw="flex flex-col"><text tw="text-white">{"hello"}</text></container>"#,
            serde_json::Value::Null,
        )
        .unwrap();
        let node = crate::parse_layout(&result);
        assert!(node.is_ok(), "parse_layout failed: {:?}", node);
    }

    #[test]
    fn eval_jsx_injects_ctx_into_script() {
        let ctx = serde_json::json!({
            "output": "DP-4",
            "dpi": 96.0,
            "width": 250,
            "outer_gap": 8
        });
        let result = eval_jsx(
            r#"<text tw="text-white">{ctx.output}</text>"#,
            ctx,
        );
        let value = result.expect("eval_jsx should not error");
        let node = crate::parse_layout(&value);
        assert!(node.is_ok(), "parse_layout failed: {:?}", node);
    }
}
