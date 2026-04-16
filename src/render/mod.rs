use std::num::NonZeroUsize;
use std::sync::Arc;

use lru::LruCache;
use takumi::{
    GlobalContext,
    layout::{Viewport, node::Node},
    rendering::{RenderOptions, render},
    resources::{font::FontResource, image::ImageSource},
};

/// LRU cache of rendered frames keyed on the canonical JSON of `module_values`.
///
/// Canonical JSON (RFC 8785 via `json_canon`) normalises object key order so that
/// `{"a":1,"b":2}` and `{"b":2,"a":1}` resolve to the same cache entry.
///
/// Frames are stored as `Arc<Vec<u8>>` so the caller can hold onto the current
/// frame across loop iterations even after it has been evicted from the cache
/// (e.g. the bar needs to repaint on Expose while the cache has already moved on).
pub struct RenderCache {
    cache: LruCache<String, Arc<Vec<u8>>>,
}

impl RenderCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            cache: LruCache::new(NonZeroUsize::new(capacity).unwrap()),
        }
    }

    pub fn get_or_render<F>(&mut self, key: &serde_json::Value, render: F) -> Arc<Vec<u8>>
    where
        F: FnOnce() -> Vec<u8>,
    {
        let t = std::time::Instant::now();
        let canonical = json_canon::to_string(key).unwrap_or_default();
        if let Some(cached) = self.cache.get(&canonical) {
            tracing::debug!(elapsed_us = t.elapsed().as_micros(), bytes = cached.len(), "render cache HIT");
            return Arc::clone(cached);
        }
        let result = Arc::new(render());
        tracing::debug!(elapsed_ms = t.elapsed().as_millis(), bytes = result.len(), "render cache MISS");
        self.cache.put(canonical, Arc::clone(&result));
        result
    }
}

/// Render `layout` into a BGRX framebuffer.
///
/// `width` and `height` are **physical** pixels — the X11 window dimensions.
/// `dpr = dpi / 96.0` scales CSS `px` units so that `1px` in the config equals
/// one logical pixel regardless of display density, matching i3's own scaling.
/// The returned buffer is always `width × height × 4` bytes (BGRX).
pub fn render_frame(layout: Option<Node>, global: &GlobalContext, width: u32, height: u32, dpr: f32) -> Vec<u8> {
    let node = layout.unwrap_or_else(|| Node::container(vec![]));
    let options = RenderOptions::builder()
        .global(global)
        .viewport(Viewport::new((Some(width), Some(height))).with_device_pixel_ratio(dpr))
        .node(node)
        .build();
    let rgba = render(options).expect("render").into_raw();
    let mut bgrx = Vec::with_capacity(rgba.len());
    for px in rgba.chunks_exact(4) {
        bgrx.extend_from_slice(&[px[2], px[1], px[0], 0x00]);
    }
    bgrx
}

pub fn load_fonts(global: &mut GlobalContext) {
    for path in [
        "/usr/share/fonts/TTF/JetBrainsMono-Regular.ttf",
        "/usr/share/fonts/TTF/JetBrainsMono-Bold.ttf",
    ] {
        if let Ok(bytes) = std::fs::read(path) {
            let _ = global.font_context.load_and_store(FontResource::new(bytes));
        }
    }
}

pub fn preload_layout_images(layout: &serde_json::Value, global: &GlobalContext) {
    fn walk(value: &serde_json::Value, srcs: &mut Vec<String>) {
        match value {
            serde_json::Value::Object(map) => {
                if map.get("type").and_then(|t| t.as_str()) == Some("image") {
                    if let Some(src) = map.get("src").and_then(|s| s.as_str()) {
                        srcs.push(src.to_string());
                    }
                    return; // image nodes are terminal
                }
                for v in map.values() {
                    walk(v, srcs);
                }
            }
            serde_json::Value::Array(arr) => {
                for v in arr {
                    walk(v, srcs);
                }
            }
            _ => {}
        }
    }

    let mut srcs = Vec::new();
    walk(layout, &mut srcs);

    for src in srcs {
        if src.starts_with("http://") || src.starts_with("https://") || src.starts_with("data:") {
            continue;
        }
        if let Ok(bytes) = std::fs::read(&src) {
            if let Ok(image) = ImageSource::from_bytes(&bytes) {
                global.persistent_image_store.insert(src, image);
            }
        }
    }
}
