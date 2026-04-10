use costae::RenderCache;

#[test]
fn cache_miss_calls_render() {
    let mut cache = RenderCache::new(30);
    let key = serde_json::json!({"a": 1});
    let mut called = 0;
    let result = cache.get_or_render(&key, || { called += 1; vec![1, 2, 3] });
    assert_eq!(*result, vec![1, 2, 3]);
    assert_eq!(called, 1);
}

#[test]
fn cache_hit_skips_render() {
    let mut cache = RenderCache::new(30);
    let key = serde_json::json!({"a": 1});
    cache.get_or_render(&key, || vec![1, 2, 3]);
    let mut called = 0;
    let result = cache.get_or_render(&key, || { called += 1; vec![99] });
    assert_eq!(*result, vec![1, 2, 3]);
    assert_eq!(called, 0);
}

#[test]
fn different_keys_produce_independent_entries() {
    let mut cache = RenderCache::new(30);
    cache.get_or_render(&serde_json::json!({"a": 1}), || vec![1]);
    let result = cache.get_or_render(&serde_json::json!({"a": 2}), || vec![2]);
    assert_eq!(*result, vec![2]);
}

#[test]
fn object_key_order_does_not_affect_cache_key() {
    let mut cache = RenderCache::new(30);
    cache.get_or_render(&serde_json::json!({"a": 1, "b": 2}), || vec![42]);
    // Same content, different insertion order — must hit cache
    let v: serde_json::Value = serde_json::from_str(r#"{"b": 2, "a": 1}"#).unwrap();
    let mut called = 0;
    let result = cache.get_or_render(&v, || { called += 1; vec![99] });
    assert_eq!(*result, vec![42]);
    assert_eq!(called, 0);
}

#[test]
fn evicts_lru_entry_when_full() {
    let mut cache = RenderCache::new(2);
    cache.get_or_render(&serde_json::json!(1), || vec![1]);
    cache.get_or_render(&serde_json::json!(2), || vec![2]);
    // Re-access key 1 so key 2 becomes LRU
    cache.get_or_render(&serde_json::json!(1), || vec![99]);
    // Add key 3 — evicts key 2 (LRU)
    cache.get_or_render(&serde_json::json!(3), || vec![3]);

    let mut called = false;
    cache.get_or_render(&serde_json::json!(2), || { called = true; vec![2] });
    assert!(called, "key 2 should have been evicted and triggered a fresh render");
}

#[test]
fn arc_allows_holding_result_after_eviction() {
    let mut cache = RenderCache::new(1);
    let key1 = serde_json::json!(1);
    let key2 = serde_json::json!(2);
    let held = cache.get_or_render(&key1, || vec![42]);
    cache.get_or_render(&key2, || vec![99]); // evicts key1
    // `held` still valid because Arc keeps it alive
    assert_eq!(*held, vec![42]);
}
