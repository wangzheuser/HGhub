//! 离线注入回归：与 net_ranged_tests 的私有请求注入模式一致，不改变生产网络策略。
//! 调度状态为进程全局，运行此组及现有 media 状态测试时使用 --test-threads=1。
use super::*;
use tokio::sync::{Notify, Semaphore};

type FetchResult = Result<(Option<String>, Vec<u8>), String>;
#[derive(Clone)]
struct Fixture {
    result: FetchResult,
    calls: Arc<AtomicUsize>,
    entered: Arc<Semaphore>,
    release: Option<Arc<Semaphore>>,
}
static FETCHES: Lazy<Mutex<HashMap<String, Fixture>>> = Lazy::new(|| Mutex::new(HashMap::new()));
static FINISHED: Notify = Notify::const_new();

pub(super) fn background_finished() {
    FINISHED.notify_waiters();
}

pub(super) async fn injected_fetch(url: &str, _ranged: bool) -> Option<FetchResult> {
    let fixture = { FETCHES.lock().unwrap().get(url).cloned() };
    let Some(fixture) = fixture else {
        // fixture 漏登记必须立即失败，而不是访问网络。
        return url
            .contains(".warmup.fixture.invalid")
            .then(|| Err("missing fixture".into()));
    };
    fixture.calls.fetch_add(1, Ordering::Relaxed);
    fixture.entered.add_permits(1);
    if let Some(release) = &fixture.release {
        release.acquire().await.unwrap().forget();
    }
    Some(fixture.result)
}

impl Fixture {
    fn bytes(url: &str, bytes: Vec<u8>, gated: bool) -> Self {
        let fixture = Self {
            result: Ok((None, bytes)),
            calls: Arc::new(AtomicUsize::new(0)),
            entered: Arc::new(Semaphore::new(0)),
            release: gated.then(|| Arc::new(Semaphore::new(0))),
        };
        FETCHES.lock().unwrap().insert(url.into(), fixture.clone());
        fixture
    }
    fn playlist(url: &str, body: &str, gated: bool) -> Self {
        Self::bytes(url, body.as_bytes().to_vec(), gated)
    }
    async fn entered(&self) {
        tokio::time::timeout(Duration::from_secs(2), self.entered.acquire())
            .await
            .expect("fixture should start")
            .unwrap()
            .forget();
    }
    fn release(&self) {
        if let Some(release) = &self.release {
            release.add_permits(4);
        }
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }
}

async fn idle() {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let changed = FINISHED.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if BACKGROUND.lock().unwrap().is_empty() {
                break;
            }
            changed.await;
        }
    })
    .await
    .expect("background must settle");
}

fn reset() {
    for (_, entry) in PREPARATIONS.lock().unwrap().drain() {
        entry.cancel.send_replace(true);
    }
    cancel_background_matching(|_, _| true, "test_reset");
    for (_, entry) in WARMUPS.lock().unwrap().drain() {
        entry.cancel.send_replace(true);
    }
    RAW_PL_CACHE.lock().unwrap().clear();
    PL_CACHE.lock().unwrap().clear();
    KEY_CACHE.lock().unwrap().clear();
    SEG_CACHE.lock().unwrap().clear();
    PL_SEGMENTS.lock().unwrap().clear();
    PL_KEYS.lock().unwrap().clear();
    PL_PARENTS.lock().unwrap().clear();
    SEG_OWNER.lock().unwrap().clear();
    FETCHES.lock().unwrap().clear();
    *SESSION.lock().unwrap() = PlaybackSession::default();
}

#[test]
fn head_dependencies_are_applicable_only_and_unsupported_shapes_are_explicit() {
    let base = "https://parse.warmup.fixture.invalid/list.m3u8";
    let text = "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"first.key\"\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4,\n0.m4s\n#EXT-X-KEY:METHOD=AES-128,URI=\"later.key\"\n#EXTINF:4,\n1.m4s\n#EXT-X-ENDLIST\n";
    let (segment, dependencies) = head_resource(base, text).unwrap().unwrap();
    assert!(segment.ends_with("/0.m4s"));
    assert_eq!(dependencies.len(), 2);
    assert!(dependencies.iter().any(|url| url.ends_with("first.key")));
    assert!(dependencies.iter().any(|url| url.ends_with("init.mp4")));
    assert!(!dependencies.iter().any(|url| url.ends_with("later.key")));
    let rotated = "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"map.key\"\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXT-X-KEY:METHOD=AES-128,URI=\"head.key\"\n0.m4s\n#EXT-X-ENDLIST\n";
    let (_, dependencies) = head_resource(base, rotated).unwrap().unwrap();
    assert_eq!(dependencies.len(), 3);
    assert!(dependencies.iter().any(|url| url.ends_with("map.key")));
    assert!(dependencies.iter().any(|url| url.ends_with("head.key")));
    assert!(dependencies.iter().any(|url| url.ends_with("init.mp4")));
    let clear_head = rotated.replace("METHOD=AES-128,URI=\"head.key\"", "METHOD=NONE");
    let (_, dependencies) = head_resource(base, &clear_head).unwrap().unwrap();
    assert_eq!(dependencies.len(), 2, "首片 METHOD=NONE 仍需 MAP 的旧 key");
    assert!(dependencies.iter().any(|url| url.ends_with("map.key")));
    assert_eq!(
        head_resource(base, "#EXTM3U\n#EXT-X-BYTERANGE:20@0\n0.ts\n").unwrap_err(),
        "unsupported_byte_range"
    );
    assert_eq!(
        head_resource(
            base,
            "#EXTM3U\n#EXT-X-MAP:URI=\"init\",BYTERANGE=\"20@0\"\n0.m4s\n"
        )
        .unwrap_err(),
        "unsupported_byte_range"
    );
    assert_eq!(
        head_resource(
            base,
            "#EXTM3U\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"key\"\n0.ts\n"
        )
        .unwrap_err(),
        "unsupported_encryption"
    );
    assert!(
        playlist_ttl(base, "#EXTM3U\n#EXT-X-TARGETDURATION:8\n0.ts\n") <= Duration::from_secs(4)
    );
    let vod = "#EXTM3U\n#EXT-X-ENDLIST\n";
    assert_eq!(
        playlist_ttl(&format!("{base}?auth_key=1-0-issued"), vod),
        Duration::from_secs(60)
    );
    assert_eq!(
        playlist_ttl(
            &format!("{base}?auth_key=1-0-issued"),
            "#EXTM3U\n#EXT-X-TARGETDURATION:8\n0.ts?auth_key=1-0-issued\n"
        ),
        Duration::from_secs(4)
    );
    assert_eq!(
        playlist_ttl(&format!("{base}?expires=1"), vod),
        Duration::ZERO
    );
    assert_eq!(
        playlist_ttl(&format!("{base}?expire=1"), vod),
        Duration::ZERO
    );
    assert_eq!(
        playlist_ttl(base, "#EXTM3U\n0.ts?expires=1\n#EXT-X-ENDLIST\n"),
        Duration::ZERO
    );
}

#[tokio::test]
async fn warmup_promotion_dependencies_preemption_and_prefix_reuse() {
    let previous_session = std::mem::take(&mut *SESSION.lock().unwrap());
    let previous_disk = DISK_DIR.lock().unwrap().take();
    reset();

    // 成功媒体响应中的 auth_key 旧时间可为签发时间，不能被误判为立即过期。
    let signed = "https://ttl.warmup.fixture.invalid/list.m3u8?auth_key=1-0-issued";
    let head_url = "https://ttl.warmup.fixture.invalid/0.ts?auth_key=1-0-issued";
    let key_url = "https://ttl.warmup.fixture.invalid/a.key?auth_key=1-0-issued";
    let playlist = Fixture::playlist(signed,
        "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"a.key?auth_key=1-0-issued\"\n0.ts?auth_key=1-0-issued\n#EXT-X-ENDLIST\n", false);
    let segment = Fixture::bytes(head_url, vec![0x47; 70000], true);
    let key = Fixture::bytes(key_url, vec![5; 16], true);
    assert!(prepare_playlist(signed, "http://prefix-a").await.unwrap());
    assert_eq!(warmup_status(signed)["state"], "idle");
    assert_eq!(warmup_status(signed)["playlist_ready"], true);
    assert!(warmup_playlist(signed, "http://prefix-a").await.unwrap());
    segment.entered().await;
    key.entered().await;
    assert_eq!(warmup_status(signed)["state"], "queued");
    assert_eq!(warmup_status(signed)["playlist_ready"], true);
    segment.release();
    key.release();
    idle().await;
    assert_eq!(warmup_status(signed)["state"], "ready");
    assert_eq!(playlist.calls(), 1);
    assert_eq!(segment.calls(), 1);
    assert_eq!(key.calls(), 1);
    // 同路径的新签名仍必须加载自己的原文，不能因 segment cache_key 归一而串用。
    let other_signed = "https://ttl.warmup.fixture.invalid/list.m3u8?auth_key=2-0-issued";
    let other = Fixture::playlist(other_signed, "#EXTM3U\n1.ts\n#EXT-X-ENDLIST\n", false);
    assert!(prepare_playlist(other_signed, "http://prefix-b")
        .await
        .unwrap());
    assert_eq!(other.calls(), 1);
    assert_eq!(playlist.calls(), 1);
    assert!(raw_playlist_hit(signed).unwrap().contains("0.ts?auth_key="));
    assert!(raw_playlist_hit(other_signed).unwrap().contains("1.ts"));
    assert_eq!(warmup_status(other_signed)["state"], "idle");

    reset();
    // 轻准备的根清单在途也可提升；失活/显式取消则终止且不发布旧计划。
    let light = "https://light.warmup.fixture.invalid/list.m3u8";
    let pending = Fixture::playlist(light, "#EXTM3U\n0.ts\n#EXT-X-ENDLIST\n", true);
    set_session("https://light.warmup.fixture.invalid/old.m3u8", true);
    let task = tokio::spawn(async move { prepare_playlist(light, "http://prefix-a").await });
    pending.entered().await;
    set_session(light, true);
    cancel_warmup(light); // 提升后的轻根清单也不被旧候选 cleanup 撤销。
    pending.release();
    assert!(tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap());
    assert!(PREPARATIONS.lock().unwrap().is_empty());
    assert_eq!(warmup_status(light)["state"], "idle");
    assert_eq!(warmup_status(light)["playlist_ready"], true);
    assert_eq!(PREFETCH_INFLIGHT.load(Ordering::Relaxed), 0);
    assert_eq!(pending.calls(), 1);

    reset();
    let current = "https://light.warmup.fixture.invalid/current.m3u8";
    set_session(current, true);
    let pending = Fixture::playlist(light, "#EXTM3U\nold.ts\n#EXT-X-ENDLIST\n", true);
    let task = tokio::spawn(async move { prepare_playlist(light, "http://prefix-a").await });
    pending.entered().await;
    set_session(current, false);
    assert!(!tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap());
    assert!(PREPARATIONS.lock().unwrap().is_empty());
    assert!(!WARMUPS.lock().unwrap().contains_key(&cache_key(light)));
    assert!(raw_playlist_hit(light).is_none());

    reset();
    let pending = Fixture::playlist(light, "#EXTM3U\nold.ts\n#EXT-X-ENDLIST\n", true);
    let task = tokio::spawn(async move { prepare_playlist(light, "http://prefix-a").await });
    pending.entered().await;
    cancel_warmup("https://light.warmup.fixture.invalid/other.m3u8");
    tokio::task::yield_now().await;
    assert!(!task.is_finished());
    cancel_warmup(light);
    assert!(!tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap());
    assert!(PREPARATIONS.lock().unwrap().is_empty());
    assert!(!WARMUPS.lock().unwrap().contains_key(&cache_key(light)));

    // 同目标较新准备使旧 registry 代失效；旧 guard 不得移除新代/覆盖新计划。
    reset();
    let old = Fixture::playlist(light, "#EXTM3U\nold.ts\n#EXT-X-ENDLIST\n", true);
    let first = tokio::spawn(async move { prepare_playlist(light, "http://prefix-a").await });
    old.entered().await;
    let old_generation = PREPARATIONS.lock().unwrap()[&cache_key(light)].generation;
    let new = Fixture::playlist(light, "#EXTM3U\nnew.ts\n#EXT-X-ENDLIST\n", true);
    let second = tokio::spawn(async move { prepare_playlist(light, "http://prefix-a").await });
    new.entered().await;
    assert!(!tokio::time::timeout(Duration::from_secs(2), first)
        .await
        .unwrap()
        .unwrap()
        .unwrap());
    let new_generation = PREPARATIONS.lock().unwrap()[&cache_key(light)].generation;
    assert!(new_generation > old_generation);
    new.release();
    assert!(tokio::time::timeout(Duration::from_secs(2), second)
        .await
        .unwrap()
        .unwrap()
        .unwrap());
    assert!(PREPARATIONS.lock().unwrap().is_empty());
    let entries = WARMUPS.lock().unwrap();
    assert_eq!(entries[&cache_key(light)].generation, new_generation);
    assert!(entries[&cache_key(light)]
        .plan
        .as_ref()
        .unwrap()
        .segment
        .ends_with("/new.ts"));
    drop(entries);

    // 轻准备完成时已有较新重预热 queued，保留后者代际并复用同一次原文下载。
    reset();
    let pending = Fixture::playlist(light, "#EXTM3U\n0.ts\n#EXT-X-ENDLIST\n", true);
    let head = Fixture::bytes(
        "https://light.warmup.fixture.invalid/0.ts",
        vec![1; 70000],
        true,
    );
    let first = tokio::spawn(async move { prepare_playlist(light, "http://prefix-a").await });
    pending.entered().await;
    let heavy = tokio::spawn(async move { warmup_playlist(light, "http://prefix-a").await });
    tokio::task::yield_now().await;
    let heavy_generation = warmup_status(light)["generation"].clone();
    assert_eq!(warmup_status(light)["state"], "queued");
    pending.release();
    assert!(tokio::time::timeout(Duration::from_secs(2), first)
        .await
        .unwrap()
        .unwrap()
        .unwrap());
    assert!(tokio::time::timeout(Duration::from_secs(2), heavy)
        .await
        .unwrap()
        .unwrap()
        .unwrap());
    head.entered().await;
    assert_eq!(warmup_status(light)["generation"], heavy_generation);
    assert_eq!(warmup_status(light)["state"], "queued");
    assert_eq!(pending.calls(), 1);
    head.release();
    idle().await;
    assert_eq!(warmup_status(light)["state"], "ready");

    reset();
    // master 只选首个视频变体；queued 与 ready 严格区分，切过去保留正在下载的首片/key。
    let master = "https://promote.warmup.fixture.invalid/master.m3u8";
    let child = "https://promote.warmup.fixture.invalid/video.m3u8";
    let head = "https://promote.warmup.fixture.invalid/0.ts";
    let key = "https://promote.warmup.fixture.invalid/a.key";
    Fixture::playlist(master, "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nvideo.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=2\nnever.m3u8\n#EXT-X-ENDLIST\n", false);
    Fixture::playlist(
        child,
        "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"a.key\"\n#EXTINF:4,\n0.ts\n#EXT-X-ENDLIST\n",
        false,
    );
    let segment = Fixture::bytes(head, vec![0x47; 70000], true);
    let aes = Fixture::bytes(key, vec![3; 16], true);
    set_session("https://old.warmup.fixture.invalid/list.m3u8", true);
    assert!(prepare_playlist(master, "http://prefix-a").await.unwrap());
    assert_eq!(warmup_status(master)["state"], "idle");
    assert_eq!(warmup_status(master)["playlist_ready"], true);
    assert_eq!(warmup_status(master)["segment_ready"], false);
    assert_eq!(PREFETCH_INFLIGHT.load(Ordering::Relaxed), 0);
    assert_eq!(segment.calls(), 0);
    assert_eq!(aes.calls(), 0);
    assert!(warmup_playlist(master, "http://prefix-a").await.unwrap());
    segment.entered().await;
    aes.entered().await;
    assert_eq!(warmup_status(master)["state"], "queued");
    assert_eq!(warmup_status(master)["segment_ready"], false);
    let generation = warmup_status(master)["generation"].clone();
    set_session(master, true);
    cancel_warmup(master); // 已提升的当前播放下载不受候选 cleanup 影响。
    assert!(BACKGROUND.lock().unwrap().contains_key(&cache_key(head)));
    segment.release();
    aes.release();
    idle().await;
    assert_eq!(warmup_status(master)["state"], "ready");
    assert_eq!(warmup_status(master)["generation"], generation);
    load_segment(head).await.unwrap();
    assert_eq!(segment.calls(), 1, "会话提升后复用同一首片下载");
    assert_eq!(aes.calls(), 1);
    let status = warmup_status(master).to_string();
    assert!(!status.contains("https://") && !status.contains("auth_key"));
    SEG_CACHE.lock().unwrap().remove(&cache_key(head));
    assert_eq!(warmup_status(master)["state"], "idle");
    assert_eq!(warmup_status(master)["reason"], "cache_evicted");
    set_session(master, false);
    assert_eq!(PREFETCH_INFLIGHT.load(Ordering::Relaxed), 0);
    assert_eq!(FOREGROUND_ACTIVE.load(Ordering::Relaxed), 0);

    // future 尚未获得执行机会时切集，起始 epoch 校验也必须接受提升。
    reset();
    let list = "https://queued.warmup.fixture.invalid/list.m3u8";
    let head = "https://queued.warmup.fixture.invalid/0.ts";
    Fixture::playlist(list, "#EXTM3U\n0.ts\n#EXT-X-ENDLIST\n", false);
    let segment = Fixture::bytes(head, vec![1; 70000], false);
    assert!(warmup_playlist(list, "http://prefix-a").await.unwrap());
    set_session(list, true);
    idle().await;
    assert_eq!(warmup_status(list)["state"], "ready");
    assert_eq!(segment.calls(), 1);
    set_session(list, false);

    // 首片已缓存仍需补 key；后台 key 和前台 key 的锁后双检必须复用一次下载。
    reset();
    let list = "https://key.warmup.fixture.invalid/list.m3u8";
    let head = "https://key.warmup.fixture.invalid/0.ts";
    let key = "https://key.warmup.fixture.invalid/a.key";
    Fixture::playlist(
        list,
        "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"a.key\"\n#EXTINF:4,\n0.ts\n#EXT-X-ENDLIST\n",
        false,
    );
    let segment = Fixture::bytes(head, vec![0; 70000], false);
    let aes = Fixture::bytes(key, vec![9; 16], true);
    store_segment_inner(head, None, vec![0x47; 70000], false);
    assert!(warmup_playlist(list, "http://prefix-a").await.unwrap());
    aes.entered().await;
    assert_eq!(warmup_status(list)["state"], "queued");
    assert_eq!(warmup_status(list)["segment_ready"], true);
    assert_eq!(warmup_status(list)["dependencies_ready"], false);
    let key_waiter = tokio::spawn(async move { load_segment(key).await });
    tokio::task::yield_now().await;
    aes.release();
    let key_result = key_waiter.await.unwrap().unwrap();
    assert_eq!(key_result.1.as_slice(), &[9; 16]);
    idle().await;
    assert_eq!(warmup_status(list)["state"], "ready");
    assert_eq!(segment.calls(), 0);
    assert_eq!(aes.calls(), 1);

    // init-map 也是真实依赖：首片命中并不足以 ready。
    reset();
    let list = "https://init.warmup.fixture.invalid/list.m3u8";
    let head = "https://init.warmup.fixture.invalid/0.m4s";
    let init = "https://init.warmup.fixture.invalid/init.mp4";
    Fixture::playlist(
        list,
        "#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n0.m4s\n#EXT-X-ENDLIST\n",
        false,
    );
    store_segment_inner(head, None, vec![1; 70000], false);
    let init_fixture = Fixture::bytes(init, vec![2; 100], true);
    assert!(warmup_playlist(list, "http://prefix-a").await.unwrap());
    init_fixture.entered().await;
    assert_eq!(warmup_status(list)["state"], "queued");
    assert_eq!(warmup_status(list)["dependencies_ready"], false);
    init_fixture.release();
    idle().await;
    assert_eq!(warmup_status(list)["state"], "ready");

    // MAP 用 keyA、首片用 keyB：两把 key 都就绪才 ready；首片明文也仍依赖 MAP key。
    reset();
    let list = "https://map-key.warmup.fixture.invalid/list.m3u8";
    let head = "https://map-key.warmup.fixture.invalid/0.m4s";
    let init = "https://map-key.warmup.fixture.invalid/init.mp4";
    let map_key = "https://map-key.warmup.fixture.invalid/map.key";
    let head_key = "https://map-key.warmup.fixture.invalid/head.key";
    Fixture::playlist(list, "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"map.key\",IV=0x00000000000000000000000000000001\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXT-X-KEY:METHOD=AES-128,URI=\"head.key\"\n0.m4s\n#EXT-X-ENDLIST\n", false);
    store_segment_inner(head, None, vec![1; 70000], false);
    store_segment_inner(init, None, vec![2; 100], false);
    let first_key = Fixture::bytes(head_key, vec![3; 16], false);
    let init_key = Fixture::bytes(map_key, vec![4; 16], true);
    assert!(warmup_playlist(list, "http://prefix-a").await.unwrap());
    init_key.entered().await;
    assert!(memory_has(head_key));
    assert!(!memory_has(map_key));
    assert_eq!(warmup_status(list)["state"], "queued");
    assert_eq!(warmup_status(list)["dependencies_ready"], false);
    init_key.release();
    idle().await;
    assert_eq!(warmup_status(list)["state"], "ready");
    assert_eq!(first_key.calls(), 1);
    assert_eq!(init_key.calls(), 1);

    // 内存命中不取消无关 warmup；真正前台网络抢占后状态 busy，可重新排队。
    reset();
    let current = "https://preempt.warmup.fixture.invalid/current.m3u8";
    let current_seg = "https://preempt.warmup.fixture.invalid/current.ts";
    let next = "https://preempt.warmup.fixture.invalid/next.m3u8";
    let next_seg = "https://preempt.warmup.fixture.invalid/next.ts";
    remember_raw_playlist(current, "#EXTM3U\ncurrent.ts\n#EXT-X-ENDLIST\n");
    set_session(current, true);
    Fixture::playlist(next, "#EXTM3U\nnext.ts\n#EXT-X-ENDLIST\n", false);
    let pending = Fixture::bytes(next_seg, vec![4; 70000], true);
    let foreground = Fixture::bytes(current_seg, vec![5; 70000], true);
    assert!(warmup_playlist(next, "http://prefix-a").await.unwrap());
    pending.entered().await;
    store_segment_inner(current_seg, None, vec![6; 70000], false);
    load_segment(current_seg).await.unwrap();
    assert_eq!(warmup_status(next)["state"], "queued");
    SEG_CACHE.lock().unwrap().remove(&cache_key(current_seg));
    let disk_dir = std::env::temp_dir().join(format!("hghub-warmup-disk-{}", std::process::id()));
    std::fs::create_dir_all(&disk_dir).unwrap();
    let disk_file = disk_dir.join(disk_key(&cache_key(current_seg)));
    std::fs::write(&disk_file, vec![6; 70000]).unwrap();
    *DISK_DIR.lock().unwrap() = Some(disk_dir.clone());
    load_segment(current_seg).await.unwrap();
    assert_eq!(
        warmup_status(next)["state"],
        "queued",
        "磁盘命中不得白杀预热"
    );
    *DISK_DIR.lock().unwrap() = None;
    std::fs::remove_file(&disk_file).unwrap();
    std::fs::remove_dir(&disk_dir).unwrap();
    SEG_CACHE.lock().unwrap().remove(&cache_key(current_seg));
    let front_task = tokio::spawn(async move { load_segment(current_seg).await });
    foreground.entered().await;
    assert_eq!(warmup_status(next)["state"], "busy");
    assert_eq!(PREFETCH_INFLIGHT.load(Ordering::Relaxed), 0);
    let previous_generation = warmup_status(next)["generation"].as_u64().unwrap();
    foreground.release();
    tokio::time::timeout(Duration::from_secs(2), front_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let retry = warmup_status(next);
    assert_eq!(
        retry["state"], "idle",
        "前台释放后只读轮询必须发出可重排状态"
    );
    assert_eq!(retry["reason"], "retryable");
    assert_eq!(retry["generation"], previous_generation);
    assert_eq!(
        WARMUPS.lock().unwrap()[&cache_key(next)].state,
        "busy",
        "status 不修改登记状态"
    );
    assert_eq!(pending.calls(), 1, "status 不启动下一次下载");
    pending.release();
    assert!(warmup_playlist(next, "http://prefix-a").await.unwrap());
    idle().await;
    assert_eq!(warmup_status(next)["state"], "ready");
    assert!(warmup_status(next)["generation"].as_u64().unwrap() > previous_generation);

    // 后台槽拒绝：前端只轮询 status，槽释放后必须看到 idle 再重新 warmup。
    reset();
    let current = "https://slots.warmup.fixture.invalid/current.m3u8";
    let first = "https://slots.warmup.fixture.invalid/first.m3u8";
    let second = "https://slots.warmup.fixture.invalid/second.m3u8";
    let second_head = "https://slots.warmup.fixture.invalid/second.ts";
    set_session(current, true);
    Fixture::playlist(first, "#EXTM3U\nfirst.ts\n#EXT-X-ENDLIST\n", false);
    Fixture::playlist(second, "#EXTM3U\nsecond.ts\n#EXT-X-ENDLIST\n", false);
    let occupied = Fixture::bytes(
        "https://slots.warmup.fixture.invalid/first.ts",
        vec![1; 70000],
        true,
    );
    let waiting = Fixture::bytes(second_head, vec![2; 70000], false);
    assert!(warmup_playlist(first, "http://prefix-a").await.unwrap());
    occupied.entered().await;
    assert!(!warmup_playlist(second, "http://prefix-a").await.unwrap());
    let generation = warmup_status(second)["generation"].clone();
    assert_eq!(warmup_status(second)["state"], "busy");
    assert_eq!(warmup_status(second)["state"], "busy");
    assert_eq!(waiting.calls(), 0);
    occupied.release();
    idle().await;
    assert_eq!(warmup_status(second)["state"], "idle");
    assert_eq!(warmup_status(second)["reason"], "retryable");
    assert_eq!(warmup_status(second)["generation"], generation);
    assert_eq!(waiting.calls(), 0, "释放槽后 status 仍不自行下载");
    assert!(BACKGROUND.lock().unwrap().is_empty());
    assert!(warmup_playlist(second, "http://prefix-a").await.unwrap());
    idle().await;
    assert_eq!(warmup_status(second)["state"], "ready");
    assert_eq!(waiting.calls(), 1);

    // 前台槽拒绝走同一契约；过代/失活的 busy 则不冒充可重试的当前 idle。
    SEG_CACHE.lock().unwrap().remove(&cache_key(second_head));
    let front_slot = foreground_counter();
    assert!(!warmup_playlist(second, "http://prefix-a").await.unwrap());
    assert_eq!(warmup_status(second)["state"], "busy");
    assert_eq!(waiting.calls(), 1);
    drop(front_slot);
    assert_eq!(warmup_status(second)["state"], "idle");
    assert_eq!(waiting.calls(), 1);
    SESSION_EPOCH.send_modify(|epoch| *epoch = epoch.wrapping_add(1));
    assert_eq!(warmup_status(second)["state"], "cancelled");
    assert_eq!(warmup_status(second)["reason"], "session_changed");
    assert!(warmup_playlist(second, "http://prefix-a").await.unwrap());
    idle().await;
    assert_eq!(warmup_status(second)["state"], "ready");
    assert_eq!(waiting.calls(), 2);
    SEG_CACHE.lock().unwrap().remove(&cache_key(second_head));
    let front_slot = foreground_counter();
    assert!(!warmup_playlist(second, "http://prefix-a").await.unwrap());
    drop(front_slot);
    set_session(current, false);
    assert_eq!(warmup_status(second)["state"], "cancelled");
    assert_eq!(waiting.calls(), 2);

    // 错误 root 必须取消，失活在调用返回时立即归还名额，晚到 Drop 不下溢。
    reset();
    let list = "https://cancel.warmup.fixture.invalid/list.m3u8";
    let head = "https://cancel.warmup.fixture.invalid/0.ts";
    Fixture::playlist(list, "#EXTM3U\n0.ts\n#EXT-X-ENDLIST\n", false);
    let pending = Fixture::bytes(head, vec![7; 70000], true);
    assert!(warmup_playlist(list, "http://prefix-a").await.unwrap());
    pending.entered().await;
    set_session("https://cancel.warmup.fixture.invalid/other.m3u8", true);
    assert_eq!(warmup_status(list)["state"], "cancelled");
    assert_eq!(PREFETCH_INFLIGHT.load(Ordering::Relaxed), 0);
    assert!(!BACKGROUND.lock().unwrap().contains_key(&cache_key(head)));
    set_session("https://cancel.warmup.fixture.invalid/other.m3u8", false);
    tokio::task::yield_now().await;
    assert_eq!(PREFETCH_INFLIGHT.load(Ordering::Relaxed), 0);

    // 真实前台在途也响应失活；计数可同步归零，future 以取消结果结束。
    reset();
    remember_raw_playlist(list, "#EXTM3U\n0.ts\n#EXT-X-ENDLIST\n");
    set_session(list, true);
    let foreground = Fixture::bytes(head, vec![8; 70000], true);
    let task = tokio::spawn(async move { load_segment(head).await });
    foreground.entered().await;
    assert_eq!(FOREGROUND_ACTIVE.load(Ordering::Relaxed), 1);
    set_session(list, false);
    assert_eq!(FOREGROUND_ACTIVE.load(Ordering::Relaxed), 0);
    assert!(task.await.unwrap().is_err());
    assert_eq!(FOREGROUND_ACTIVE.load(Ordering::Relaxed), 0);

    // 原文跨 prefix + /seg 子清单 singleflight；改写不交叉污染。
    reset();
    let child = "https://prefix.warmup.fixture.invalid/video?id=1";
    register_children(
        "https://prefix.warmup.fixture.invalid/master",
        "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nvideo?id=1\n",
    );
    let raw = Fixture::playlist(child, "#EXTM3U\n0.ts\n#EXT-X-ENDLIST\n", true);
    let first = tokio::spawn(async move { load_playlist(child, "http://prefix-a").await.unwrap() });
    raw.entered().await;
    let second = tokio::spawn(async move {
        handle("/seg", &format!("u={}", enc(child)), "http://prefix-b").await
    });
    tokio::task::yield_now().await;
    raw.release();
    let a = first.await.unwrap();
    let b = second.await.unwrap();
    let b = String::from_utf8(b.body().clone()).unwrap();
    assert!(a.contains("http://prefix-a/seg?u=") && !a.contains("prefix-b"));
    assert!(b.contains("http://prefix-b/seg?u=") && !b.contains("prefix-a"));
    assert_eq!(raw.calls(), 1);

    // 独立音轨明确不 ready，不误下载整个 master 的所有变体。
    reset();
    let master = "https://audio.warmup.fixture.invalid/master.m3u8";
    Fixture::playlist(master, "#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\",URI=\"audio.m3u8\"\n#EXT-X-STREAM-INF:BANDWIDTH=1,AUDIO=\"audio\"\nvideo.m3u8\n#EXT-X-ENDLIST\n", false);
    assert!(warmup_playlist(master, "http://prefix-a").await.is_err());
    assert_eq!(warmup_status(master)["state"], "failed");
    assert_eq!(
        warmup_status(master)["reason"],
        "unsupported_audio_rendition"
    );
    assert_eq!(PREFETCH_INFLIGHT.load(Ordering::Relaxed), 0);

    reset();
    *DISK_DIR.lock().unwrap() = previous_disk;
    *SESSION.lock().unwrap() = previous_session;
}

#[tokio::test]
async fn prepared_warm_intent_yields_only_new_ordinary_slot_and_expires() {
    let previous_session = std::mem::take(&mut *SESSION.lock().unwrap());
    let previous_disk = DISK_DIR.lock().unwrap().take();
    reset();
    let current = "https://yield.warmup.fixture.invalid/current.m3u8";
    let first = "https://yield.warmup.fixture.invalid/0.ts";
    let successor = "https://yield.warmup.fixture.invalid/1.ts";
    let candidate = "https://yield.warmup.fixture.invalid/next.m3u8";
    let candidate_head = "https://yield.warmup.fixture.invalid/next.ts";
    remember_raw_playlist(current, "#EXTM3U\n0.ts\n1.ts\n#EXT-X-ENDLIST\n");
    set_session(current, true);
    let foreground = Fixture::bytes(first, vec![1; 70000], true);
    let ordinary = Fixture::bytes(successor, vec![2; 70000], true);
    Fixture::playlist(candidate, "#EXTM3U\nnext.ts\n#EXT-X-ENDLIST\n", false);
    let warm = Fixture::bytes(candidate_head, vec![3; 70000], true);
    let task = tokio::spawn(async move { load_segment(first).await });
    foreground.entered().await;
    assert!(!warmup_playlist(candidate, "http://prefix-a").await.unwrap());
    assert_eq!(warmup_status(candidate)["state"], "busy");
    assert!(pending_warmup_intent());
    assert_eq!(FOREGROUND_ACTIVE.load(Ordering::Relaxed), 1);
    assert!(!task.is_finished(), "只让新ordinary槽，绝不取消当前前台");
    foreground.release();
    let result = tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(result.1.as_slice(), vec![1; 70000].as_slice());
    assert_eq!(foreground.calls(), 1);
    assert_eq!(
        ordinary.calls(),
        0,
        "当前FG自然结束后不能先派ordinary挡住warm候选"
    );
    assert!(BACKGROUND.lock().unwrap().is_empty());
    assert_eq!(warmup_status(candidate)["state"], "idle");
    assert!(warmup_playlist(candidate, "http://prefix-a").await.unwrap());
    warm.entered().await;
    load_segment(first).await.unwrap(); // 缓存命中也不取消已有 warm 后台。
    assert_eq!(ordinary.calls(), 0);
    assert!(BACKGROUND
        .lock()
        .unwrap()
        .contains_key(&cache_key(candidate_head)));
    warm.release();
    idle().await;
    assert_eq!(warmup_status(candidate)["state"], "ready");
    assert!(!pending_warmup_intent());
    schedule_prefetch(first);
    ordinary.entered().await;
    assert_eq!(ordinary.calls(), 1, "warm完成后ordinary立即恢复");
    assert!(BACKGROUND
        .lock()
        .unwrap()
        .contains_key(&cache_key(successor)));
    cancel_warmup(candidate);
    assert!(
        BACKGROUND
            .lock()
            .unwrap()
            .contains_key(&cache_key(successor)),
        "warm撤销不取消ordinary在途"
    );
    ordinary.release();
    idle().await;

    // 保存的状态、实际ready、代际、原文TTL和意图年龄分别失效后都不能长期抑制ordinary。
    let candidate_key = cache_key(candidate);
    SEG_CACHE.lock().unwrap().remove(&cache_key(candidate_head));
    for state in ["failed", "cancelled", "ready"] {
        {
            let mut entries = WARMUPS.lock().unwrap();
            let entry = entries.get_mut(&candidate_key).unwrap();
            entry.state = state;
            entry.cancel = tokio::sync::watch::channel(false).0;
            entry.intent_deadline = std::time::Instant::now() + Duration::from_secs(60);
        }
        assert!(!pending_warmup_intent());
        SEG_CACHE.lock().unwrap().remove(&cache_key(successor));
        schedule_prefetch(first);
        ordinary.entered().await;
        ordinary.release();
        idle().await;
    }
    {
        let mut entries = WARMUPS.lock().unwrap();
        entries.get_mut(&candidate_key).unwrap().state = "busy";
    }
    assert!(pending_warmup_intent());
    for defect in ["age", "epoch", "playlist_ttl", "actually_ready"] {
        let epoch = *SESSION_EPOCH.borrow();
        {
            let mut entries = WARMUPS.lock().unwrap();
            let entry = entries.get_mut(&candidate_key).unwrap();
            entry.intent_deadline = std::time::Instant::now() + Duration::from_secs(60);
            entry.admission.store(epoch, Ordering::Release);
            if defect == "age" {
                entry.intent_deadline = std::time::Instant::now() - Duration::from_secs(1);
            }
            if defect == "epoch" {
                entry
                    .admission
                    .store(epoch.wrapping_sub(1), Ordering::Release);
            }
        }
        RAW_PL_CACHE
            .lock()
            .unwrap()
            .get_mut(candidate)
            .unwrap()
            .expires = if defect == "playlist_ttl" {
            std::time::Instant::now() - Duration::from_secs(1)
        } else {
            std::time::Instant::now() + Duration::from_secs(60)
        };
        if defect == "actually_ready" {
            store_segment_inner(candidate_head, None, vec![3; 70000], false);
        }
        assert!(!pending_warmup_intent(), "{defect} 不得阻止ordinary恢复");
        SEG_CACHE.lock().unwrap().remove(&cache_key(successor));
        schedule_prefetch(first);
        ordinary.entered().await;
        ordinary.release();
        idle().await;
    }
    set_session(current, false);
    assert!(!pending_warmup_intent());
    reset();
    *DISK_DIR.lock().unwrap() = previous_disk;
    *SESSION.lock().unwrap() = previous_session;
}
