use super::*;

#[test]
fn over_four_mib_segment_remains_available_for_singleflight_handoff() {
    let url = "https://cache-budget.fixture.invalid/large-head.ts";
    let bytes = vec![0x47; 4_207_456]; // 真实长尾现场的分片大小，略大于旧 4MiB 上限。
    let (_, saved) = store_segment_inner(url, Some("video/mp2t".into()), bytes, false);
    let hit = lru_get(&SEG_CACHE, &cache_key(url)).expect("同片接管必须命中缓存而非再次下载");
    assert!(Arc::ptr_eq(&hit.1, &saved));
    SEG_CACHE.lock().unwrap().remove(&cache_key(url));
}

#[test]
fn byte_budget_evicts_least_recently_used_not_recently_read() {
    let map = Mutex::new(SegmentCache::new());
    let value = || ("video/mp2t".to_string(), Arc::new(vec![1; 4]));
    assert!(segment_lru_put(&map, "a".into(), value(), 80, 8));
    assert!(segment_lru_put(&map, "b".into(), value(), 80, 8));
    assert!(lru_get(&map, "a").is_some());
    assert!(segment_lru_put(&map, "c".into(), value(), 80, 8));
    let cache = map.lock().unwrap();
    assert!(cache.contains_key("a") && cache.contains_key("c"));
    assert!(!cache.contains_key("b"));
    assert_eq!(cache.values().map(|v| v.value.1.len()).sum::<usize>(), 8);
}

#[test]
fn replacements_and_oversized_values_preserve_budget() {
    let map = Mutex::new(SegmentCache::new());
    let value = |size| ("video/mp2t".to_string(), Arc::new(vec![1; size]));
    assert!(segment_lru_put(&map, "a".into(), value(6), 2, 10));
    assert!(segment_lru_put(&map, "b".into(), value(4), 2, 10));
    assert!(segment_lru_put(&map, "a".into(), value(5), 2, 10));
    assert!(!segment_lru_put(&map, "a".into(), value(11), 2, 10));
    assert_eq!(lru_get(&map, "a").unwrap().1.len(), 5);
    assert!(segment_lru_put(&map, "c".into(), value(1), 2, 10));
    let cache = map.lock().unwrap();
    assert_eq!(cache.len(), 2);
    assert!(cache.values().map(|v| v.value.1.len()).sum::<usize>() <= 10);
}
