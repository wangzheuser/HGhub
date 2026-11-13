//! 媒体层：封面解密 + HLS 代理。
//!
//! 站点封面是 AES-128-CBC 密文，m3u8 内部分片/key 又可能被 WebView 的 CORS 拦住，
//! 所以图片与视频统一走本地自定义协议（Windows/Android 为 `http://hg.localhost`，
//! macOS 为 `hg://localhost`，见 [`LOCAL_ORIGIN`]）。

use crate::net::{fetch_bytes, fetch_bytes_ranged};
use aes::cipher::{BlockDecrypt, KeyInit};
use aes::Aes128;
use once_cell::sync::Lazy;
use percent_encoding::{percent_decode_str, utf8_percent_encode, NON_ALPHANUMERIC};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::http::{Response, StatusCode};

/// 本平台自定义协议的完整基址。
///
/// Tauri 的自定义协议地址随平台不同：Windows / Android 是 `http://<scheme>.localhost`，
/// macOS / iOS / Linux 是 `<scheme>://localhost`。m3u8 里的分片与 key 地址按它改写。
/// 前端 `src/api.ts` 的 `LOCAL` 必须与此一致，否则封面与视频全部加载失败。
#[cfg(any(target_os = "windows", target_os = "android"))]
pub const LOCAL_ORIGIN: &str = "http://hg.localhost";
/// Linux 走 loopback HTTP：WebKitGTK 的播放器是 GStreamer，它拿不到自定义 scheme。
#[cfg(target_os = "linux")]
pub const LOCAL_ORIGIN: &str = crate::bridge_http::ORIGIN;
#[cfg(not(any(target_os = "windows", target_os = "android", target_os = "linux")))]
pub const LOCAL_ORIGIN: &str = "hg://localhost";

const IMG_KEY: &[u8] = b"f5d965df75336270";
const IMG_IV: &[u8] = b"97b60394abc2fbe1";

static IMG_CACHE: Lazy<Mutex<HashMap<String, Arc<Vec<u8>>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
struct RawPlaylist {
    text: Arc<String>,
    expires: std::time::Instant,
}
struct RewrittenPlaylist {
    raw: Arc<String>,
    text: Arc<String>,
}
// 原文按完整签名 URL 隔离；改写按 origin 隔离，两层共用一次网络加载。
static RAW_PL_CACHE: Lazy<Mutex<HashMap<String, RawPlaylist>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
static PL_CACHE: Lazy<Mutex<HashMap<String, RewrittenPlaylist>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
static PL_LOCKS: Lazy<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

#[cfg(test)]
#[path = "media_warmup_tests.rs"]
mod warmup_tests;

/// 私有可注入边界：测试只命中自己登记的本地 fixture，不改变生产目标校验。
async fn media_fetch(url: &str, ranged: bool) -> Result<(Option<String>, Vec<u8>), String> {
    #[cfg(test)]
    if let Some(result) = warmup_tests::injected_fetch(url, ranged).await {
        return result;
    }
    if ranged {
        fetch_bytes_ranged(url, RANGED_PARTS).await
    } else {
        fetch_bytes(url).await
    }
}
/// AES Key / 分片：拖进度条时同一 key、同一片会被反复拉，缓存住省一次往返。
static KEY_CACHE: Lazy<Mutex<HashMap<String, (String, Arc<Vec<u8>>)>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

const IMG_CACHE_MAX: usize = 240;
const PL_CACHE_MAX: usize = 60;
const KEY_CACHE_MAX: usize = 120;
/// 分片缓存按 LRU 淘汰（见 [`segment_lru_put`]），容量按「整集大部分分片」估算：
/// 单集约 186 片 × 2.1MB，全缓存不现实，80 片（≈170MB）足以覆盖正常观看
/// 的前后各 30 秒，让回看与小幅前跳直接命中。
const SEG_CACHE_MAX: usize = 80;
// 8 秒高清分片实测会略超 4MiB，拒绝入缓存会使同片接管/重试重复整包下载。
const SEG_CACHE_MAX_BYTES: usize = 64 * 1024 * 1024;
const SEG_CACHE_TOTAL_BYTES: usize = 192 * 1024 * 1024;

/// 带访问序号的缓存条目。淘汰时选**最久未使用**的，而不是最早插入的：
/// 拖动进度条回看时，FIFO 往往先把用户刚看过的分片淘汰掉，白丢一次 3 秒下载。
struct LruEntry<V> {
    value: V,
    used: u64,
}

/// 全局单调递增的访问序号，只用于 LRU 排序，不参与业务判断。
static ACCESS_SEQ: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));

fn next_seq() -> u64 {
    ACCESS_SEQ.fetch_add(1, Ordering::Relaxed)
}

type SegmentCache = HashMap<String, LruEntry<(String, Arc<Vec<u8>>)>>;
static SEG_CACHE: Lazy<Mutex<SegmentCache>> = Lazy::new(|| Mutex::new(HashMap::new()));

/// 同时限制条数和总字节。下载完成后，同片等待者始终可以复用一次缓存交接。
fn segment_lru_put(
    map: &Mutex<SegmentCache>,
    key: String,
    value: (String, Arc<Vec<u8>>),
    max_entries: usize,
    max_bytes: usize,
) -> bool {
    let len = value.1.len();
    if max_entries == 0 || len > max_bytes {
        return false;
    }
    let Ok(mut cache) = map.lock() else {
        return false;
    };
    cache.remove(&key);
    let mut used: usize = cache.values().map(|entry| entry.value.1.len()).sum();
    while cache.len() >= max_entries || used.saturating_add(len) > max_bytes {
        let Some(oldest) = cache
            .iter()
            .min_by_key(|(_, entry)| entry.used)
            .map(|(key, _)| key.clone())
        else {
            break;
        };
        if let Some(old) = cache.remove(&oldest) {
            used = used.saturating_sub(old.value.1.len());
        }
    }
    cache.insert(
        key,
        LruEntry {
            value,
            used: next_seq(),
        },
    );
    true
}

/// 读缓存并刷新其 LRU 序号（命中即视为最近使用）。
fn lru_get<V: Clone>(map: &Mutex<HashMap<String, LruEntry<V>>>, key: &str) -> Option<V> {
    let seq = next_seq();
    let mut m = match map.lock() {
        Ok(m) => m,
        Err(_) => return None,
    };
    let entry = m.get_mut(key)?;
    entry.used = seq;
    Some(entry.value.clone())
}

fn cache_put<V: Clone>(map: &Mutex<HashMap<String, V>>, key: String, val: V, max: usize) {
    let mut m = map.lock().unwrap();
    if m.len() >= max {
        if let Some(k) = m.keys().next().cloned() {
            m.remove(&k);
        }
    }
    m.insert(key, val);
}

/* ───────────────────────── 分片磁盘缓存 ───────────────────────── */

/// 磁盘缓存目录（启动时 [`bind_disk_cache`] 注入；None 表示不启用）。
static DISK_DIR: Lazy<Mutex<Option<std::path::PathBuf>>> = Lazy::new(|| Mutex::new(None));
/// 磁盘缓存总量上限。500MB ≈ 240 片，够放十几集的已看片段；
/// 超限按「最后访问时间」淘汰最旧的（文件 mtime 在读取时刷新）。
#[cfg(test)]
#[path = "media_cache_budget_tests.rs"]
mod cache_budget_tests;

const DISK_CACHE_MAX_BYTES: u64 = 500 * 1024 * 1024;
/// 单文件大小上限：超过就不落盘（异常大分片不值得占空间）
const DISK_ENTRY_MAX_BYTES: usize = 8 * 1024 * 1024;

/// 绑定磁盘缓存目录（应用启动时调用一次）。目录不可写时静默关闭该功能。
pub fn bind_disk_cache(dir: std::path::PathBuf) {
    let path = dir.join("segment-cache");
    if std::fs::create_dir_all(&path).is_err() {
        return;
    }
    if let Ok(mut slot) = DISK_DIR.lock() {
        *slot = Some(path);
    }
}

/// URL → 稳定的文件名。用 FNV-1a 64 位：无需引入哈希库，
/// 且不把上游 URL（含 auth_key 签名）写进文件名。
fn disk_key(url: &str) -> String {
    let hash = url.as_bytes().iter().fold(14695981039346656037u64, |h, b| {
        (h ^ *b as u64).wrapping_mul(1099511628211)
    });
    format!("{hash:016x}.seg")
}

fn disk_path(url: &str) -> Option<std::path::PathBuf> {
    let dir = DISK_DIR.lock().ok()?.clone()?;
    Some(dir.join(disk_key(url)))
}

/// 从磁盘读分片；命中时刷新 mtime（LRU 依据），返回内容与类型。
fn disk_get(url: &str) -> Option<Arc<Vec<u8>>> {
    let path = disk_path(url)?;
    let bytes = std::fs::read(&path).ok()?;
    // 刷新访问时间，供淘汰时判断「最久没用」
    let _ = filetime_touch(&path);
    Some(Arc::new(bytes))
}

/// 落盘（异步、失败静默）。写临时文件再改名，避免读到半截文件。
fn disk_put(url: &str, bytes: &Arc<Vec<u8>>) {
    if bytes.len() > DISK_ENTRY_MAX_BYTES {
        return;
    }
    let Some(path) = disk_path(url) else { return };
    // 有界单写入队列，避免每片创建线程及并行扫描同一缓存目录。
    // 队列满时仅略过可选落盘，内存缓存与本次播放不受影响。
    static WRITER: Lazy<std::sync::mpsc::SyncSender<(std::path::PathBuf, Arc<Vec<u8>>)>> =
        Lazy::new(|| {
            let (sender, receiver) =
                std::sync::mpsc::sync_channel::<(std::path::PathBuf, Arc<Vec<u8>>)>(16);
            std::thread::spawn(move || {
                while let Ok((path, bytes)) = receiver.recv() {
                    let tmp = path.with_extension("tmp");
                    if std::fs::write(&tmp, bytes.as_slice()).is_err() {
                        let _ = std::fs::remove_file(&tmp);
                        continue;
                    }
                    if std::fs::rename(&tmp, &path).is_err() {
                        let _ = std::fs::remove_file(&tmp);
                        continue;
                    }
                    trim_disk_cache();
                }
            });
            sender
        });
    let _ = WRITER.try_send((path, bytes.clone()));
}

fn filetime_touch(path: &std::path::Path) -> std::io::Result<()> {
    // 显式 set_modified 更新 LastWriteTime。不能用「append 打开再关闭」：
    // 实测（2026-11-12，真实缓存文件）Windows 上不写入字节的打开/关闭
    // **不会**更新 LastWriteTime——旧的实现让 LRU 触碰完全失效，
    // 「按 mtime 淘汰最旧」会错杀常看分片（越常看越先被删）。
    let file = std::fs::OpenOptions::new().write(true).open(path)?;
    file.set_modified(std::time::SystemTime::now())
}

/// 超限时按 mtime 从旧到新删除，直到回到上限以下（保留一半余量避免频繁触发）。
fn trim_disk_cache() {
    let Some(dir) = DISK_DIR.lock().ok().and_then(|d| d.clone()) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, u64, std::path::PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let meta = entry.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            let modified = meta.modified().ok()?;
            Some((modified, meta.len(), entry.path()))
        })
        .collect();
    let total: u64 = files.iter().map(|(_, len, _)| *len).sum();
    if total <= DISK_CACHE_MAX_BYTES {
        return;
    }
    files.sort_by_key(|(modified, _, _)| *modified);
    let target = DISK_CACHE_MAX_BYTES / 2;
    let mut freed = 0u64;
    for (_, len, path) in files {
        if total.saturating_sub(freed) <= target {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            freed += len;
        }
    }
}

/* ───────────────────────── 预取 ───────────────────────── */

/// 播放列表 → 分片序列（原始上游地址）。预取靠它判断「当前片后面还有哪些」。
static PL_SEGMENTS: Lazy<Mutex<HashMap<String, Arc<Vec<String>>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
static PL_KEYS: Lazy<Mutex<HashMap<String, Vec<String>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
/// 分片 → (所属播放列表, 下标)。反查用，key 是上游原始分片 URL。
static SEG_OWNER: Lazy<Mutex<HashMap<String, (String, usize)>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
/// 下载状态按当前播放会话隔离；顺序取片不再使预热任务换代。
#[derive(Default)]
struct PlaybackSession {
    controlled: bool,
    active: Option<String>,
    foreground: Option<(String, std::time::Instant)>,
}
static SESSION: Lazy<Mutex<PlaybackSession>> = Lazy::new(|| Mutex::new(PlaybackSession::default()));
static SESSION_EPOCH: Lazy<tokio::sync::watch::Sender<u64>> =
    Lazy::new(|| tokio::sync::watch::channel(0).0);
static SEEK_EPOCH: Lazy<tokio::sync::watch::Sender<u64>> =
    Lazy::new(|| tokio::sync::watch::channel(0).0);
/// 子清单到父清单的关系，避免 master/media 清单被当成不同播放会话。
static PL_PARENTS: Lazy<Mutex<HashMap<String, String>>> = Lazy::new(|| Mutex::new(HashMap::new()));
/// 后台只保留一个重媒体任务。任务 Drop 同时清理名额和状态。
static BACKGROUND: Lazy<Mutex<HashMap<String, (u64, tokio::task::AbortHandle)>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
static BACKGROUND_SEQ: AtomicU64 = AtomicU64::new(0);
static PREFETCH_INFLIGHT: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone)]
struct WarmupPlan {
    playlists: Vec<String>,
    segment: String,
    dependencies: Vec<String>,
}
struct WarmupEntry {
    generation: u64,
    root: String,
    state: &'static str,
    reason: Option<&'static str>,
    plan: Option<WarmupPlan>,
    intent_deadline: std::time::Instant,
    admission: Arc<AtomicU64>,
    cancel: tokio::sync::watch::Sender<bool>,
    promoted: bool,
}
static WARMUPS: Lazy<Mutex<HashMap<String, WarmupEntry>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
static WARMUP_SEQ: AtomicU64 = AtomicU64::new(1);
struct PreparationMeta {
    generation: u64,
    root: String,
    admission: Arc<AtomicU64>,
    cancel: tokio::sync::watch::Sender<bool>,
    promoted: bool,
}
static PREPARATIONS: Lazy<Mutex<HashMap<String, PreparationMeta>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
struct PreparationGuard {
    key: String,
    generation: u64,
}
impl Drop for PreparationGuard {
    fn drop(&mut self) {
        if let Ok(mut preparations) = PREPARATIONS.lock() {
            if preparations
                .get(&self.key)
                .is_some_and(|entry| entry.generation == self.generation)
            {
                preparations.remove(&self.key);
            }
        }
    }
}
#[derive(Clone)]
struct BackgroundMeta {
    root: String,
    admission: Arc<AtomicU64>,
    warmup: Option<(String, u64)>,
    promoted: bool,
    count: Arc<CounterState>,
}
static BACKGROUND_META: Lazy<Mutex<HashMap<u64, BackgroundMeta>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

fn memory_has(url: &str) -> bool {
    let key = cache_key(url);
    KEY_CACHE.lock().is_ok_and(|cache| cache.contains_key(&key))
        || SEG_CACHE.lock().is_ok_and(|cache| cache.contains_key(&key))
}

fn update_warmup(key: &str, generation: u64, state: &'static str, reason: Option<&'static str>) {
    if let Ok(mut entries) = WARMUPS.lock() {
        if let Some(entry) = entries
            .get_mut(key)
            .filter(|entry| entry.generation == generation)
        {
            // 已撤销的一代不得被晚到的完成回调改写。
            if entry.state != "cancelled" {
                entry.state = state;
                entry.reason = reason;
            }
        }
    }
}

/// 只读内存快照，不碰磁盘、不刷新 LRU、不触发下载，也不泄漏 URL/key。
pub fn warmup_status(url: &str) -> serde_json::Value {
    let key = cache_key(url);
    let snapshot = WARMUPS.lock().ok().and_then(|entries| {
        entries.get(&key).map(|entry| {
            (
                entry.generation,
                entry.state,
                entry.reason,
                entry.plan.clone(),
                entry.admission.clone(),
            )
        })
    });
    let Some((generation, mut state, mut reason, plan, admission)) = snapshot else {
        return serde_json::json!({"state":"idle", "playlist_ready":false,
            "segment_ready":false, "dependencies_ready":false, "generation":0});
    };
    let playlist_ready = plan.as_ref().is_some_and(|plan| {
        RAW_PL_CACHE.lock().is_ok_and(|cache| {
            plan.playlists.iter().all(|url| {
                cache
                    .get(url)
                    .is_some_and(|entry| std::time::Instant::now() < entry.expires)
            })
        })
    });
    let segment_ready = plan.as_ref().is_some_and(|plan| memory_has(&plan.segment));
    let dependencies_ready = plan
        .as_ref()
        .is_some_and(|plan| plan.dependencies.iter().all(|url| memory_has(url)));
    if state != "cancelled"
        && state != "failed"
        && playlist_ready
        && segment_ready
        && dependencies_ready
    {
        state = "ready";
        reason = None;
    } else if state == "ready" {
        state = "idle";
        reason = Some("cache_evicted");
    } else if state == "busy" {
        // busy 是上次排队/抢占的结果，不是永久状态。仅观察槽位使前端廉价轮询
        // 能发现可重试时机；不改变 generation/登记状态，也不隐式重新发起下载。
        let eligibility = SESSION
            .lock()
            .map(|session| {
                if *SESSION_EPOCH.borrow() != admission.load(Ordering::Acquire) {
                    Err("session_changed")
                } else if session.controlled && session.active.is_none() {
                    Err("session_inactive")
                } else {
                    Ok(())
                }
            })
            .unwrap_or(Err("session_unavailable"));
        match eligibility {
            Err(cancel_reason) => {
                state = "cancelled";
                reason = Some(cancel_reason);
            }
            Ok(())
                if FOREGROUND_ACTIVE.load(Ordering::Relaxed) == 0
                    && BACKGROUND.lock().is_ok_and(|jobs| jobs.is_empty()) =>
            {
                state = "idle";
                reason = Some("retryable");
            }
            Ok(()) => {}
        }
    }
    let mut status = serde_json::json!({"state":state, "playlist_ready":playlist_ready,
        "segment_ready":segment_ready, "dependencies_ready":dependencies_ready, "generation":generation});
    if let Some(reason) = reason {
        status["reason"] = reason.into();
    }
    status
}

/// 精确取消这一预热代；已提升为当前播放的任务与普通后继预取不受影响。
pub fn cancel_warmup(url: &str) {
    let key = cache_key(url);
    if let Ok(mut preparations) = PREPARATIONS.lock() {
        if let Some(preparation) = preparations.get_mut(&key).filter(|entry| !entry.promoted) {
            preparation.cancel.send_replace(true);
        }
    }
    let generation = if let Ok(mut entries) = WARMUPS.lock() {
        entries.get_mut(&key).and_then(|entry| {
            if entry.promoted {
                return None;
            }
            entry.state = "cancelled";
            entry.reason = Some("requested");
            entry.cancel.send_replace(true);
            Some(entry.generation)
        })
    } else {
        None
    };
    if let Some(generation) = generation {
        cancel_background_matching(
            |_, meta| {
                meta.is_some_and(|meta| {
                    !meta.promoted && meta.warmup.as_ref() == Some(&(key.clone(), generation))
                })
            },
            "requested",
        );
    }
}

/// 取消调用与 future Drop 共享一次性 lease：取消立即归零，晚到 Drop 不下溢。
struct CounterState {
    counter: &'static AtomicUsize,
    live: AtomicBool,
}
impl CounterState {
    fn release(&self) {
        if self.live.swap(false, Ordering::AcqRel) {
            self.counter.fetch_sub(1, Ordering::Relaxed);
        }
    }
}
struct CounterGuard(Arc<CounterState>);
impl CounterGuard {
    fn new(counter: &'static AtomicUsize) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        Self(Arc::new(CounterState {
            counter,
            live: AtomicBool::new(true),
        }))
    }
}
impl Drop for CounterGuard {
    fn drop(&mut self) {
        self.0.release();
    }
}
static FOREGROUND_LEASES: Lazy<Mutex<Vec<std::sync::Weak<CounterState>>>> =
    Lazy::new(|| Mutex::new(Vec::new()));

fn foreground_counter() -> CounterGuard {
    let guard = CounterGuard::new(&FOREGROUND_ACTIVE);
    if let Ok(mut leases) = FOREGROUND_LEASES.lock() {
        leases.retain(|lease| lease.strong_count() > 0);
        leases.push(Arc::downgrade(&guard.0));
    }
    guard
}

fn release_foreground_counters() {
    if let Ok(mut leases) = FOREGROUND_LEASES.lock() {
        for lease in leases.drain(..).filter_map(|lease| lease.upgrade()) {
            lease.release();
        }
    }
}
struct BackgroundGuard {
    key: String,
    token: u64,
    _count: CounterGuard,
}
impl Drop for BackgroundGuard {
    fn drop(&mut self) {
        if let Ok(mut jobs) = BACKGROUND.lock() {
            if jobs
                .get(&self.key)
                .is_some_and(|(token, _)| *token == self.token)
            {
                jobs.remove(&self.key);
            }
        }
        if let Ok(mut metadata) = BACKGROUND_META.lock() {
            metadata.remove(&self.token);
        }
        self._count.0.release();
        #[cfg(test)]
        warmup_tests::background_finished();
    }
}

fn playlist_root(raw: &str) -> String {
    let mut key = cache_key(raw);
    if let Ok(parents) = PL_PARENTS.lock() {
        for _ in 0..8 {
            let Some(parent) = parents.get(&key) else {
                break;
            };
            if parent == &key {
                break;
            }
            key = parent.clone();
        }
    }
    key
}

fn register_children(url: &str, text: &str) {
    if let Ok(mut parents) = PL_PARENTS.lock() {
        if parents.len() > 512 {
            parents.clear();
        }
        let mut variant = false;
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with("#EXT-X-STREAM-INF") {
                variant = true;
            }
            if line.starts_with('#') && line.contains("URI=\"") {
                let rendition = line.starts_with("#EXT-X-MEDIA")
                    || line.starts_with("#EXT-X-I-FRAME-STREAM-INF");
                map_uri_attributes(line, |uri| {
                    let child = absolutize(url, uri);
                    if rendition || looks_like_playlist(&child) {
                        parents.insert(cache_key(&child), cache_key(url));
                    }
                    uri.to_string()
                });
            } else if !line.starts_with('#') && !line.is_empty() {
                let child = absolutize(url, line);
                if variant || looks_like_playlist(&child) {
                    parents.insert(cache_key(&child), cache_key(url));
                }
                variant = false;
            }
        }
    }
}

fn cancel_background_matching(
    predicate: impl Fn(&str, Option<&BackgroundMeta>) -> bool,
    reason: &'static str,
) {
    let cancelled = if let Ok(mut jobs) = BACKGROUND.lock() {
        let mut metadata = BACKGROUND_META.lock().unwrap();
        let keys: Vec<_> = jobs
            .iter()
            .filter(|(key, (token, _))| predicate(key, metadata.get(token)))
            .map(|(key, _)| key.clone())
            .collect();
        keys.into_iter()
            .filter_map(|key| {
                jobs.remove(&key)
                    .map(|(token, handle)| (handle, metadata.remove(&token)))
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    for (handle, meta) in cancelled {
        if let Some(meta) = &meta {
            meta.count.release();
        }
        if let Some((key, generation)) = meta.and_then(|meta| meta.warmup) {
            update_warmup(
                &key,
                generation,
                if reason == "foreground" {
                    "busy"
                } else {
                    "cancelled"
                },
                Some(reason),
            );
        }
        handle.abort();
    }
}

fn cancel_background_except(keep: Option<&str>) {
    cancel_background_matching(|key, _| Some(key) != keep, "foreground");
}

fn reconcile_warmups(root: Option<&str>, epoch: u64) {
    // 先重绑定 admission，再发布 SESSION_EPOCH。新启动的 future 也能通过起始校验。
    if let Ok(mut entries) = WARMUPS.lock() {
        for entry in entries.values_mut() {
            if root.is_some_and(|root| playlist_root(&entry.root) == root) {
                entry.admission.store(epoch, Ordering::Release);
                entry.promoted = true;
            } else if entry.state == "queued" || entry.state == "busy" {
                entry.state = "cancelled";
                entry.reason = Some("session_changed");
                entry.cancel.send_replace(true);
            }
        }
    }
    if let Ok(mut preparations) = PREPARATIONS.lock() {
        for preparation in preparations.values_mut() {
            if root.is_some_and(|root| playlist_root(&preparation.root) == root) {
                preparation.admission.store(epoch, Ordering::Release);
                preparation.promoted = true;
            } else {
                preparation.cancel.send_replace(true);
            }
        }
    }
    if let Ok(mut metadata) = BACKGROUND_META.lock() {
        for meta in metadata.values_mut() {
            if root.is_some_and(|root| playlist_root(&meta.root) == root) {
                meta.admission.store(epoch, Ordering::Release);
                meta.promoted = true;
            }
        }
    }
    cancel_background_matching(
        |_, meta| {
            !root.is_some_and(|root| meta.is_some_and(|meta| playlist_root(&meta.root) == root))
        },
        "session_changed",
    );
}

/// 前端换源/失活时通知后端；旧页面的延迟 cleanup 不得取消新视频。
pub fn set_session(url: &str, active: bool) {
    let key = playlist_root(url);
    // 状态、通知及取消副作用在同一串行临界区内完成，旧调用不得越过新会话。
    let Ok(mut session) = SESSION.lock() else {
        return;
    };
    session.controlled = true;
    // 父子关系可能在 set_session(child) 之后才由 master 建立，比较时再归根。
    if let Some(current) = &session.active {
        session.active = Some(playlist_root(current));
    }
    let changed = if active {
        if session.active.as_ref() == Some(&key) {
            false
        } else {
            session.active = Some(key.clone());
            session.foreground = None;
            true
        }
    } else if session.active.as_ref() == Some(&key) {
        session.active = None;
        session.foreground = None;
        true
    } else {
        false
    };
    if changed {
        let epoch = (*SESSION_EPOCH.borrow()).wrapping_add(1);
        reconcile_warmups(if active { Some(&key) } else { None }, epoch);
        SESSION_EPOCH.send_replace(epoch);
        release_foreground_counters();
        cancel_speculative(None);
        SEEK_EPOCH.send_modify(|epoch| *epoch = epoch.wrapping_add(1));
        crate::playback_log::record(
            "session.changed",
            serde_json::json!({"active": active, "resource": crate::playback_log::resource_id(url)}),
        );
    }
}

fn segment_owner(url: &str) -> Option<(String, usize)> {
    SEG_OWNER.lock().ok()?.get(&cache_key(url)).cloned()
}

fn mark_foreground(url: &str) -> bool {
    let Some((playlist, _)) = segment_owner(url) else {
        return true;
    };
    let root = playlist_root(&playlist);
    let Ok(mut session) = SESSION.lock() else {
        return false;
    };
    if session.controlled
        && !session
            .active
            .as_deref()
            .is_some_and(|active| playlist_root(active) == root)
    {
        return false;
    }
    session.foreground = Some((root, std::time::Instant::now()));
    true
}

fn chain_active_for(playlist: &str) -> bool {
    let root = playlist_root(playlist);
    SESSION.lock().is_ok_and(|session| {
        (!session.controlled
            || session
                .active
                .as_deref()
                .is_some_and(|active| playlist_root(active) == root))
            && session
                .foreground
                .as_ref()
                .is_some_and(|(owner, at)| owner == &root && at.elapsed() < Duration::from_secs(30))
    })
}

/// 前台未命中分片时的分段并发数。
/// 实测并发扩展（同一 CDN）：1→10.7s、2→6.1s、4→2.65s、**8→1.97s**，
/// 8 路仍在改善，故取 8；再高收益递减且更容易触发 CDN 风控。
const RANGED_PARTS: usize = 8;
/// 最近一次前台分片下载耗时（毫秒），用于自适应领先片数与超时判断。
/// 0 表示尚无样本。
static LAST_DOWNLOAD_MS: AtomicU64 = AtomicU64::new(0);
/// 前台正在下载的分片数。预取看到它 > 0 时主动让路，
/// 避免「后台预取把带宽占满，前台反而更慢」——实测 3 路预取 + 1 路前台时，
/// 前台分片耗时会被拉长。
static FOREGROUND_ACTIVE: AtomicUsize = AtomicUsize::new(0);

/// 记录一次前台下载耗时，供预取自适应使用。
fn observe_download_speed(elapsed: std::time::Duration) {
    let ms = elapsed.as_millis().min(u64::MAX as u128) as u64;
    // 只采信合理区间，避免异常值（超时重试、0ms 缓存穿透）带偏判断
    if ms >= 50 && ms <= 60_000 {
        LAST_DOWNLOAD_MS.store(ms, Ordering::Relaxed);
    }
}

/// 分片级下载锁（singleflight）：同一片同时只允许一次真实下载。
///
/// 没有它会出现「前台请求与预取任务各下一遍」：实测同一片被并发拉两次，
/// 前台反而要等 14 秒（连接池被占 + 重复流量）。有了它，前台遇到正在预取的
/// 分片时只需等那次下载结束，然后直接拿缓存。
static SEG_LOCKS: Lazy<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

fn seg_lock(url: &str) -> Arc<tokio::sync::Mutex<()>> {
    let mut locks = match SEG_LOCKS.lock() {
        Ok(l) => l,
        Err(_) => return Arc::new(tokio::sync::Mutex::new(())),
    };
    if let Some(existing) = locks.get(url) {
        return existing.clone();
    }
    // 防御：长时间追剧会积累大量锁，超阈值清理已无人持有的
    if locks.len() >= 512 {
        locks.retain(|_, lock| Arc::strong_count(lock) > 1);
        if locks.len() >= 512 {
            locks.clear();
        }
    }
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    locks.insert(url.to_string(), lock.clone());
    lock
}

/* ───────────────────────── AES ───────────────────────── */

fn aes_cbc_decrypt(data: &[u8]) -> Option<Vec<u8>> {
    if data.is_empty() || data.len() % 16 != 0 {
        return None;
    }
    let cipher = Aes128::new_from_slice(IMG_KEY).ok()?;
    let mut prev = [0u8; 16];
    prev.copy_from_slice(IMG_IV);
    let mut out = Vec::with_capacity(data.len());
    for chunk in data.chunks(16) {
        let mut ct = [0u8; 16];
        ct.copy_from_slice(chunk);
        let mut block = ct;
        cipher.decrypt_block((&mut block).into());
        for i in 0..16 {
            out.push(block[i] ^ prev[i]);
        }
        prev = ct;
    }
    Some(out)
}

fn strip_padding(mut buf: Vec<u8>) -> Vec<u8> {
    let pad = *buf.last().unwrap_or(&0) as usize;
    if pad > 0 && pad <= 16 && buf.len() >= pad {
        if buf[buf.len() - pad..].iter().all(|b| *b as usize == pad) {
            buf.truncate(buf.len() - pad);
        }
    }
    buf
}

fn image_kind(buf: &[u8]) -> Option<&'static str> {
    if buf.len() < 12 {
        return None;
    }
    if buf[0] == 0xFF && buf[1] == 0xD8 {
        return Some("image/jpeg");
    }
    if buf.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
        return Some("image/png");
    }
    if buf.starts_with(b"GIF8") {
        return Some("image/gif");
    }
    if buf.starts_with(b"RIFF") && &buf[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

/// 取封面：命中缓存直接返回，否则下载并按需解密。
pub async fn load_image(url: &str) -> Result<(String, Arc<Vec<u8>>), String> {
    if let Some(hit) = IMG_CACHE.lock().unwrap().get(url).cloned() {
        let kind = image_kind(&hit).unwrap_or("image/jpeg");
        return Ok((kind.to_string(), hit));
    }
    let (_, bytes) = fetch_bytes(url).await?;
    let plain = if image_kind(&bytes).is_some() {
        bytes
    } else {
        let dec = aes_cbc_decrypt(&bytes).ok_or_else(|| "封面解密失败".to_string())?;
        strip_padding(dec)
    };
    let kind = image_kind(&plain).unwrap_or("image/jpeg").to_string();
    let arc = Arc::new(plain);
    cache_put(&IMG_CACHE, url.to_string(), arc.clone(), IMG_CACHE_MAX);
    Ok((kind, arc))
}

/* ───────────────────────── HLS ───────────────────────── */

fn enc(s: &str) -> String {
    utf8_percent_encode(s, NON_ALPHANUMERIC).to_string()
}

fn dec(s: &str) -> String {
    percent_decode_str(s).decode_utf8_lossy().to_string()
}

fn absolutize(base: &str, target: &str) -> String {
    if target.starts_with("http://") || target.starts_with("https://") {
        return target.to_string();
    }
    if let Ok(b) = url::Url::parse(base) {
        if let Ok(j) = b.join(target) {
            return j.to_string();
        }
    }
    target.to_string()
}

/// 属性列表中的引号内可含逗号，不能直接 split(',')。
fn hls_attributes(line: &str) -> Option<(&str, Vec<&str>)> {
    let (tag, rest) = line.split_once(':')?;
    let mut attributes = Vec::new();
    let mut quoted = false;
    let mut start = 0;
    for (i, ch) in rest.char_indices() {
        if ch == '"' {
            quoted = !quoted;
        }
        if ch == ',' && !quoted {
            attributes.push(&rest[start..i]);
            start = i + 1;
        }
    }
    attributes.push(&rest[start..]);
    Some((tag, attributes))
}

fn map_uri_attributes(line: &str, mut rewrite: impl FnMut(&str) -> String) -> String {
    let Some((tag, attributes)) = hls_attributes(line) else {
        return line.to_string();
    };
    let mapped: Vec<String> = attributes
        .into_iter()
        .map(|attribute| {
            if let Some((key, value)) = attribute.split_once('=') {
                if key.trim() == "URI" {
                    if let Some(uri) = value
                        .trim()
                        .strip_prefix('"')
                        .and_then(|v| v.strip_suffix('"'))
                    {
                        return format!("{key}=\"{}\"", rewrite(uri));
                    }
                }
            }
            attribute.to_string()
        })
        .collect();
    format!("{tag}:{}", mapped.join(","))
}

fn rewrite_uri_attr(line: &str, prefix: &str) -> String {
    map_uri_attributes(line, |uri| format!("{prefix}/seg?u={}", enc(uri)))
}

fn playlist_lock(url: &str) -> Arc<tokio::sync::Mutex<()>> {
    let mut locks = PL_LOCKS.lock().unwrap();
    if locks.len() >= 128 {
        locks.retain(|_, lock| Arc::strong_count(lock) > 1);
    }
    locks
        .entry(url.to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

fn raw_playlist_hit(url: &str) -> Option<Arc<String>> {
    RAW_PL_CACHE
        .lock()
        .ok()?
        .get(url)
        .filter(|entry| std::time::Instant::now() < entry.expires)
        .map(|entry| entry.text.clone())
}

/// VOD 有界复用，动态清单按目标时长更新；明确 expires/expire 才扣绝对到期余量。
/// auth_key 首字段可能是签发时间，未经确认不据此推断到期；成功响应沿用有界 TTL。
/// 原文绝不使用去签名的 segment cache_key，以免旧签名污染新播放地址。
fn playlist_ttl(url: &str, text: &str) -> Duration {
    let mut seconds = if text
        .lines()
        .any(|line| line.trim() == "#EXT-X-ENDLIST" || line.starts_with("#EXT-X-STREAM-INF:"))
    {
        60
    } else {
        text.lines()
            .find_map(|line| {
                line.strip_prefix("#EXT-X-TARGETDURATION:")?
                    .parse::<u64>()
                    .ok()
            })
            .map(|target| (target / 2).clamp(1, 5))
            .unwrap_or(2)
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let mut inspect = |raw: &str| {
        if let Ok(parsed) = url::Url::parse(raw) {
            for (name, value) in parsed.query_pairs() {
                if name == "expires" || name == "expire" {
                    if let Ok(expiry) = value.parse::<u64>() {
                        seconds = seconds.min(expiry.saturating_sub(now).saturating_sub(5));
                    }
                }
            }
        }
    };
    inspect(url);
    for line in text.lines() {
        if line.starts_with('#') {
            map_uri_attributes(line, |uri| {
                inspect(&absolutize(url, uri));
                uri.to_string()
            });
        } else if !line.trim().is_empty() {
            inspect(&absolutize(url, line.trim()));
        }
    }
    Duration::from_secs(seconds)
}

fn remember_raw_playlist(url: &str, text: &str) -> Arc<String> {
    let text = Arc::new(text.trim_start_matches('\u{feff}').to_string());
    let expires = std::time::Instant::now() + playlist_ttl(url, &text);
    let (_, segments, keys) = rewrite_playlist(url, &text, "");
    register_playlist(url, &segments);
    register_children(url, &text);
    cache_put(&PL_KEYS, url.to_string(), keys, 64);
    if let Ok(mut cache) = RAW_PL_CACHE.lock() {
        if cache.len() >= PL_CACHE_MAX && !cache.contains_key(url) {
            if let Some(oldest) = cache
                .iter()
                .min_by_key(|(_, entry)| entry.expires)
                .map(|(key, _)| key.clone())
            {
                cache.remove(&oldest);
            }
        }
        cache.insert(
            url.to_string(),
            RawPlaylist {
                text: text.clone(),
                expires,
            },
        );
    }
    text
}

async fn load_raw_playlist(url: &str) -> Result<Arc<String>, String> {
    if let Some(text) = raw_playlist_hit(url) {
        return Ok(text);
    }
    let lock = playlist_lock(url);
    let _guard = lock.lock().await;
    if let Some(text) = raw_playlist_hit(url) {
        return Ok(text);
    }
    let (_, bytes) = media_fetch(url, false).await?;
    let text = std::str::from_utf8(&bytes).map_err(|_| "播放列表不是 UTF-8 文本")?;
    if !text.trim_start_matches('\u{feff}').starts_with("#EXTM3U") {
        return Err("响应不是 HLS 播放列表".into());
    }
    Ok(remember_raw_playlist(url, text))
}

fn known_playlist(url: &str) -> bool {
    looks_like_playlist(url)
        || RAW_PL_CACHE
            .lock()
            .is_ok_and(|cache| cache.contains_key(url))
        || PL_PARENTS
            .lock()
            .is_ok_and(|parents| parents.contains_key(&cache_key(url)))
}

/// 原文 singleflight 与 /seg 子清单共用；只有最终改写结果按 prefix 隔离。
pub async fn load_playlist(url: &str, prefix: &str) -> Result<Arc<String>, String> {
    let raw = load_raw_playlist(url).await?;
    let key = format!("{prefix}|{url}");
    if let Some(hit) = PL_CACHE
        .lock()
        .unwrap()
        .get(&key)
        .filter(|entry| Arc::ptr_eq(&entry.raw, &raw))
        .map(|entry| entry.text.clone())
    {
        return Ok(hit);
    }
    let text = Arc::new(rewrite_playlist(url, &raw, prefix).0);
    let mut cache = PL_CACHE.lock().unwrap();
    if cache.len() >= PL_CACHE_MAX && !cache.contains_key(&key) {
        if let Some(old) = cache.keys().next().cloned() {
            cache.remove(&old);
        }
    }
    cache.insert(
        key,
        RewrittenPlaylist {
            raw,
            text: text.clone(),
        },
    );
    Ok(text)
}

/// 记录某份播放列表的分片顺序。重新加载同一份列表时先清掉旧映射
/// （分片序号可能变化，旧下标会让预取取到错误位置）。
fn register_playlist(playlist_url: &str, segments: &[String]) {
    if let Ok(mut owner) = SEG_OWNER.lock() {
        owner.retain(|_, (owner_pl, _)| owner_pl != playlist_url);
        // 防御：长时间追剧会累积很多列表，超阈值直接清空（预取只是加速，丢了也能工作）
        if owner.len() > 4096 {
            owner.clear();
        }
        for (index, segment) in segments.iter().enumerate() {
            owner.insert(cache_key(segment), (playlist_url.to_string(), index));
        }
    }
    if let Ok(mut lists) = PL_SEGMENTS.lock() {
        if lists.len() >= 64 {
            lists.clear();
        }
        lists.insert(playlist_url.to_string(), Arc::new(segments.to_vec()));
    }
}

fn rewrite_playlist(url: &str, text: &str, prefix: &str) -> (String, Vec<String>, Vec<String>) {
    let mut out = String::with_capacity(text.len() + 512);
    let mut segments: Vec<String> = Vec::new();
    let mut keys: Vec<String> = Vec::new();
    // 多码率 master playlist：`#EXT-X-STREAM-INF` 的下一行是**子清单**而不是分片。
    // 若把它当分片收集，预热会去下载子清单文本（预热无效），预取索引也会错位。
    let mut next_line_is_variant = false;
    for raw in text.trim_start_matches('\u{feff}').lines() {
        let line = raw.trim_end();
        if line.is_empty() {
            out.push('\n');
            continue;
        }
        if line.starts_with('#') {
            if line.starts_with("#EXT-X-STREAM-INF") {
                next_line_is_variant = true;
            }
            // 禁用可派生 URL 的内容引导；DATERANGE 保留时间线/SCTE-35 元数据。
            if line.starts_with("#EXT-X-CONTENT-STEERING:") {
                continue;
            }
            if line.starts_with("#EXT-X-DATERANGE:") {
                if let Some((tag, attributes)) = hls_attributes(line) {
                    let safe: Vec<&str> = attributes
                        .into_iter()
                        .filter(|attribute| {
                            let key = attribute
                                .split('=')
                                .next()
                                .unwrap_or("")
                                .trim()
                                .to_ascii_uppercase();
                            !key.contains("URI") && key != "X-ASSET-LIST"
                        })
                        .collect();
                    out.push_str(&format!("{tag}:{}\n", safe.join(",")));
                }
                continue;
            }
            if line.contains("URI=\"") {
                // KEY/MAP/MEDIA/SESSION-KEY/PART/PRELOAD-HINT 等 URI 都必须经过目标检查。
                let absolute = absolutize_uri_in_attr(url, line);
                // AES 密钥纳入预热：播放器拿到分片后还要取 key 才能解密，
                // 冷取一次多 200-500ms 的往返；提前放进 KEY_CACHE 可以省掉。
                if line.starts_with("#EXT-X-KEY") || line.starts_with("#EXT-X-SESSION-KEY") {
                    collect_key_uris(&absolute, &mut keys);
                }
                out.push_str(&rewrite_uri_attr(&absolute, prefix));
            } else {
                out.push_str(line);
            }
        } else {
            let absolute = absolutize(url, line);
            // 子清单（master 的变体流，或后缀就是 .m3u8）不当作分片：
            // 它需要继续改写与登记，但预热/预取的目标只能是真正的媒体分片。
            let is_variant = next_line_is_variant || looks_like_playlist(&absolute);
            next_line_is_variant = false;
            if !is_variant {
                segments.push(absolute.clone());
            }
            out.push_str(&format!("{prefix}/seg?u={}", enc(&absolute)));
        }
        out.push('\n');
    }

    (out, segments, keys)
}

/// 地址后缀看起来是播放列表（`.m3u8` / `.m3u`，忽略查询串）。
fn looks_like_playlist(raw: &str) -> bool {
    let path = raw.split('?').next().unwrap_or(raw).to_ascii_lowercase();
    path.ends_with(".m3u8") || path.ends_with(".m3u")
}

/// 从已绝对化的 KEY 行里收集 URI 属性对应的原始地址（去重）。
fn collect_key_uris(line: &str, keys: &mut Vec<String>) {
    map_uri_attributes(line, |uri| {
        if !keys.iter().any(|k| k == uri) {
            keys.push(uri.to_string());
        }
        uri.to_string()
    });
}

fn absolutize_uri_in_attr(base: &str, line: &str) -> String {
    map_uri_attributes(line, |uri| absolutize(base, uri))
}

/// 归一化缓存键：剥离 `auth_key` 签名参数，并把视频 CDN 池的 `tp<N>` 轮换子域
/// 归一成 `tp`。两类变化都与内容无关：
/// - auth_key 是访问授权（实测按小时级轮换，同一集重看/跨启动必然变）；
/// - tp1/tp3/tp6… 是同一源站的轮换入口（实测同一分片路径不同时间从不同 tpN 下发）。
/// 不做这两步，按完整 URL 做键会让 SEG_CACHE 与 500MB 磁盘缓存重看/跨启动全 miss。
/// 归一规则失效的后果只是缓存 miss（退回重下载），不会拿到错误内容。
fn cache_key(url: &str) -> String {
    let parsed = match url::Url::parse(url) {
        Ok(u) => u,
        Err(_) => return url.to_string(),
    };
    // 主机名归一：tp<N>.domain → tp.domain（仅这一已知轮换模式，其余主机原样）
    let host = parsed.host_str().unwrap_or_default();
    let host = if let Some(pool) = host
        .strip_suffix(".drleao.cn")
        .and_then(|pool| pool.strip_prefix("tp"))
    {
        if !pool.is_empty() && pool.chars().all(|c| c.is_ascii_digit()) {
            "tp.drleao.cn".to_string()
        } else {
            host.to_string()
        }
    } else {
        host.to_string()
    };
    let port = match parsed.port() {
        Some(p) => format!(":{p}"),
        None => String::new(),
    };
    let path = parsed.path();
    match parsed.query() {
        None => format!("{}://{}{}{}", parsed.scheme(), host, port, path),
        Some(q) => {
            let kept: Vec<&str> = q
                .split('&')
                .filter(|kv| {
                    let name = kv.split('=').next().unwrap_or("");
                    name != "auth_key" && !name.is_empty()
                })
                .collect();
            if kept.is_empty() {
                format!("{}://{}{}{}", parsed.scheme(), host, port, path)
            } else {
                format!(
                    "{}://{}{}{}?{}",
                    parsed.scheme(),
                    host,
                    port,
                    path,
                    kept.join("&")
                )
            }
        }
    }
}

/// 分片 / AES Key 带内存缓存：拖动进度条时避免重复拉取同一片造成卡顿。
///
/// 未命中时走 singleflight：拿到分片级锁后再查一次缓存，避免与后台预取
/// 重复下载同一片（实测重复下载会让前台等 10 秒以上）。
/// 缓存键经 [`cache_key`] 归一化（剥离签名），下载仍用完整签名 URL。
pub async fn load_segment(url: &str) -> Result<(Option<String>, Arc<Vec<u8>>), String> {
    if known_playlist(url) {
        let text = load_raw_playlist(url).await?;
        return Ok((
            Some("application/vnd.apple.mpegurl".into()),
            Arc::new(text.as_bytes().to_vec()),
        ));
    }
    let started = std::time::Instant::now();
    // 播放器（或拖动推测预取）发起了分片请求 = 有真实观看行为：
    // 任何路径（缓存命中与否）都刷新预取链的活跃窗口。
    // 先捕获代际再校验会话，避免旧请求校验后借用新代际继续下载。
    let mut session_epoch = SESSION_EPOCH.subscribe();
    let epoch = *session_epoch.borrow();
    if !mark_foreground(url) {
        return Err("播放请求已取消".into());
    }
    let key = cache_key(url);
    let media_segment = segment_owner(url).is_some();
    // 只有明确的 seek 意图才换代；正常并行音视频分片不得互相取消。
    let mut seek_epoch = SEEK_EPOCH.subscribe();
    let seek_generation = *seek_epoch.borrow();
    // 服务来源打点：缓存体系（内存/磁盘/网络）的实际收益靠它量化。
    if let Some((ctype, hit)) = KEY_CACHE.lock().unwrap().get(&key).cloned() {
        crate::playback_log::record(
            "seg.serve",
            serde_json::json!({
                "resource": crate::playback_log::resource_id(url),
                "source": "key_mem",
                "elapsed_ms": started.elapsed().as_millis() as u64,
            }),
        );
        return Ok((Some(ctype), hit));
    }
    if let Some((ctype, hit)) = lru_get(&SEG_CACHE, &key) {
        // 命中缓存也要触发后继预取：用户可能停下来反复回看同一段，
        // 只靠「下载成功」触发会让预取停摆。
        schedule_prefetch(url);
        crate::playback_log::record(
            "seg.serve",
            serde_json::json!({
                "resource": crate::playback_log::resource_id(url),
                "source": "mem",
                "elapsed_ms": started.elapsed().as_millis() as u64,
            }),
        );
        return Ok((Some(ctype), hit));
    }

    // 等同片 singleflight 时保留原下载；缓存/磁盘命中不抢占无关预热。
    let lock = seg_lock(&key);
    let _guard = tokio::select! {
        guard = lock.lock() => guard,
        _ = session_epoch.wait_for(|current| *current != epoch) => return Err("播放请求已取消".into()),
        _ = seek_epoch.wait_for(|current| *current != seek_generation), if media_segment => return Err("播放请求已替代".into()),
    };
    // Key 也必须锁后双检，否则与后台 key 等待者各下载一次。
    if let Some((ctype, hit)) = KEY_CACHE.lock().unwrap().get(&key).cloned() {
        return Ok((Some(ctype), hit));
    }
    // 双检：等锁期间后台预取可能已经把这一片填进缓存
    if let Some((ctype, hit)) = lru_get(&SEG_CACHE, &key) {
        drop(_guard);
        schedule_prefetch(url);
        crate::playback_log::record(
            "seg.serve",
            serde_json::json!({
                "resource": crate::playback_log::resource_id(url),
                "source": "mem",
                "elapsed_ms": started.elapsed().as_millis() as u64,
            }),
        );
        return Ok((Some(ctype), hit));
    }

    // 磁盘缓存：上次播放（甚至上次启动）存过的分片直接读盘，
    // 省掉 3–5 秒下载。读盘 2.1MB 通常 <10ms，远快于走网络。
    if let Some(bytes) = disk_get(&key) {
        let stored = store_segment_inner(&key, None, bytes.as_ref().clone(), false);
        drop(_guard);
        schedule_prefetch(url);
        crate::playback_log::record(
            "seg.serve",
            serde_json::json!({
                "resource": crate::playback_log::resource_id(url),
                "source": "disk",
                "elapsed_ms": started.elapsed().as_millis() as u64,
            }),
        );
        return Ok(stored);
    }

    // 分段并发下载：单连接吞吐被 RTT 限制在 0.35–0.7MB/s，一片 2.1MB 要 3–5 秒。
    // 8 路并发实测约 1.97s（4 路 2.65s），这是「远距离跳转」唯一能压进 2 秒的手段
    // （预取只能救已缓存区间）。CDN 不支持 Range 时自动退回整包下载。
    if *SESSION_EPOCH.borrow() != epoch
        || (media_segment && *SEEK_EPOCH.borrow() != seek_generation)
    {
        return Err("播放请求已取消".into());
    }
    let _foreground = foreground_counter();
    cancel_background_except(Some(&key));
    let download_started = std::time::Instant::now();
    let fetched = tokio::select! {
        result = media_fetch(url, true) => result,
        _ = session_epoch.wait_for(|current| *current != epoch) => return Err("播放请求已取消".into()),
        _ = seek_epoch.wait_for(|current| *current != seek_generation), if media_segment => return Err("播放请求已替代".into()),
    };
    let (ctype, bytes) = fetched?;
    observe_download_speed(download_started.elapsed());
    let stored = store_segment(url, ctype, bytes);
    drop(_guard);
    drop(_foreground);
    schedule_prefetch(url);
    crate::playback_log::record(
        "seg.serve",
        serde_json::json!({
            "resource": crate::playback_log::resource_id(url),
            "source": "network",
            "elapsed_ms": started.elapsed().as_millis() as u64,
        }),
    );
    Ok(stored)
}

/// 按大小分流写缓存（Key 很小、分片较大），返回给调用方的 (ctype, bytes)。
/// `url` 须是 [`cache_key`] 归一化后的键（调用方传入完整 URL 也可以——
/// 存储时再归一化一次，保证跨签名命中）。
fn store_segment(
    url: &str,
    ctype: Option<String>,
    bytes: Vec<u8>,
) -> (Option<String>, Arc<Vec<u8>>) {
    store_segment_inner(url, ctype, bytes, true)
}

fn store_segment_inner(
    url: &str,
    ctype: Option<String>,
    bytes: Vec<u8>,
    persist: bool,
) -> (Option<String>, Arc<Vec<u8>>) {
    let key = cache_key(url);
    let mime = ctype.clone().unwrap_or_else(|| {
        let p = key.split('?').next().unwrap_or(&key).to_ascii_lowercase();
        if p.ends_with(".ts") {
            "video/mp2t".to_string()
        } else if p.ends_with(".m4s") || p.ends_with(".mp4") {
            "video/mp4".to_string()
        } else {
            "application/octet-stream".to_string()
        }
    });
    // 扩展名缺失的 /seg 子清单也进入同一原文缓存，而非永久混入 KEY_CACHE。
    if bytes.starts_with(b"#EXTM3U") || bytes.starts_with(b"\xef\xbb\xbf#EXTM3U") {
        if let Ok(text) = std::str::from_utf8(&bytes) {
            remember_raw_playlist(url, text);
        }
        return (ctype, Arc::new(bytes));
    }
    let len = bytes.len();
    let arc = Arc::new(bytes);
    if len <= 65536 {
        cache_put(
            &KEY_CACHE,
            key.clone(),
            (mime.clone(), arc.clone()),
            KEY_CACHE_MAX,
        );
    } else if len <= SEG_CACHE_MAX_BYTES {
        segment_lru_put(
            &SEG_CACHE,
            key.clone(),
            (mime.clone(), arc.clone()),
            SEG_CACHE_MAX,
            SEG_CACHE_TOTAL_BYTES,
        );
        // 落盘：下次启动（或内存淘汰后）再看同一集就不必重新下载。
        // 2.1MB 的分片写盘约几毫秒，放在独立线程里，不阻塞本次响应。
        if persist {
            disk_put(&key, &arc);
        }
    }
    (Some(mime), arc)
}

/// 切集/切片预热：只拉播放列表并排队下载开头分片，不返回内容。
///
/// 前端在「相邻集播放地址预热成功」后调用它，切过去时首片往往已经就绪，
/// 表现为切集秒开而不是黑屏 3–5 秒。
fn attribute(line: &str, name: &str) -> Option<String> {
    hls_attributes(line)?.1.into_iter().find_map(|item| {
        let (key, value) = item.trim().split_once('=')?;
        key.eq_ignore_ascii_case(name)
            .then(|| value.trim_matches('"').to_string())
    })
}

fn key_dependency(base: &str, line: &str) -> Result<Option<String>, &'static str> {
    match attribute(line, "METHOD").as_deref() {
        Some("NONE") => Ok(None),
        Some("AES-128")
            if attribute(line, "KEYFORMAT").is_none_or(|format| format == "identity") =>
        {
            attribute(line, "URI")
                .map(|uri| Some(absolutize(base, &uri)))
                .ok_or("missing_key_uri")
        }
        _ => Err("unsupported_encryption"),
    }
}

/// 只枚举首片适用的依赖；轮换密钥与其它变体不会被一次性拉取。
fn head_resource(base: &str, text: &str) -> Result<Option<(String, Vec<String>)>, &'static str> {
    let mut key = None;
    let mut init = None;
    let mut init_key = None;
    let mut variant = false;
    let mut ranged = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with("#EXT-X-KEY:") {
            key = key_dependency(base, line)?;
        } else if line.starts_with("#EXT-X-MAP:") {
            if attribute(line, "BYTERANGE").is_some() {
                return Err("unsupported_byte_range");
            }
            init = attribute(line, "URI").map(|uri| absolutize(base, &uri));
            // MAP 适用的是它出现时的 key；首片前再次 EXT-X-KEY 不得覆盖 init 的依赖。
            init_key = key.clone();
            if init.is_none() {
                return Err("missing_init_uri");
            }
        } else if line.starts_with("#EXT-X-BYTERANGE:") {
            ranged = true;
        } else if line.starts_with("#EXT-X-STREAM-INF:") {
            variant = true;
        } else if !line.starts_with('#') && !line.is_empty() {
            let absolute = absolutize(base, line);
            if variant || looks_like_playlist(&absolute) {
                variant = false;
                continue;
            }
            if ranged {
                return Err("unsupported_byte_range");
            }
            let mut dependencies: Vec<_> = key.into_iter().chain(init_key).chain(init).collect();
            let mut seen = std::collections::HashSet::new();
            dependencies.retain(|url| seen.insert(cache_key(url)));
            return Ok(Some((absolute, dependencies)));
        }
    }
    Ok(None)
}

async fn prepare_warmup(url: &str, origin: &str) -> Result<WarmupPlan, String> {
    let mut selected = url.to_string();
    let mut playlists = Vec::new();
    let mut inherited_keys = Vec::new();
    for _ in 0..4 {
        // 同时建立 prefix 改写缓存；下载由原文 singleflight 统一负责。
        load_playlist(&selected, origin).await?;
        let raw = raw_playlist_hit(&selected).ok_or("playlist_expired")?;
        playlists.push(selected.clone());
        if let Some((segment, mut dependencies)) =
            head_resource(&selected, &raw).map_err(str::to_string)?
        {
            inherited_keys.append(&mut dependencies);
            dependencies = inherited_keys;
            let mut seen = std::collections::HashSet::new();
            dependencies.retain(|url| seen.insert(cache_key(url)));
            return Ok(WarmupPlan {
                playlists,
                segment,
                dependencies,
            });
        }
        let mut stream = None;
        let mut variant = None;
        for line in raw.lines().map(str::trim) {
            if line.starts_with("#EXT-X-SESSION-KEY:") {
                if let Some(key) = key_dependency(&selected, line).map_err(str::to_string)? {
                    inherited_keys.push(key);
                }
            } else if line.starts_with("#EXT-X-STREAM-INF:") {
                stream = Some(line);
            } else if !line.is_empty() && !line.starts_with('#') {
                if stream.is_some() || looks_like_playlist(line) {
                    variant = Some((absolutize(&selected, line), stream));
                    break;
                }
            }
        }
        let Some((child, stream)) = variant else {
            return Err("no_media_segment".into());
        };
        // 暂不猜测独立音轨的播放器选择；明确不 ready，比漏掉必需依赖更可靠。
        if let Some(group) = stream.and_then(|line| attribute(line, "AUDIO")) {
            if raw.lines().any(|line| {
                line.starts_with("#EXT-X-MEDIA:")
                    && attribute(line, "TYPE").as_deref() == Some("AUDIO")
                    && attribute(line, "GROUP-ID").as_deref() == Some(group.as_str())
                    && attribute(line, "URI").is_some()
            }) {
                return Err("unsupported_audio_rendition".into());
            }
        }
        selected = child;
    }
    Err("playlist_depth".into())
}

fn warmup_failure(error: &str) -> &'static str {
    match error {
        "unsupported_audio_rendition" => "unsupported_audio_rendition",
        "unsupported_byte_range" => "unsupported_byte_range",
        "unsupported_encryption" => "unsupported_encryption",
        "missing_key_uri" => "missing_key_uri",
        "missing_init_uri" => "missing_init_uri",
        "no_media_segment" => "no_media_segment",
        "playlist_depth" => "playlist_depth",
        "playlist_expired" => "playlist_expired",
        error if error.contains("UTF-8") || error.contains("HLS") => "invalid_playlist",
        error if error.contains("403") || error.contains("401") => "http_auth",
        error if error.contains("HTTP") => "http",
        error if error.contains("timeout") || error.contains("超时") => "timeout",
        _ => "network",
    }
}

/// 轻准备只加载清单/计划；没有分片、key 网络请求，也不占后台重任务名额。
pub async fn prepare_playlist(url: &str, origin: &str) -> Result<bool, String> {
    let key = cache_key(url);
    let generation = WARMUP_SEQ.fetch_add(1, Ordering::Relaxed);
    let admission = Arc::new(AtomicU64::new(0));
    let (cancel, mut cancel_rx) = tokio::sync::watch::channel(false);
    let mut epoch_rx = SESSION_EPOCH.subscribe();
    {
        // 登记与 set_session 使用相同串行边界，避免刚登记就漏掉一次切集通知。
        let session = SESSION.lock().unwrap();
        let root = playlist_root(url);
        admission.store(*SESSION_EPOCH.borrow(), Ordering::Release);
        let promoted = session
            .active
            .as_deref()
            .is_some_and(|active| playlist_root(active) == root);
        let mut preparations = PREPARATIONS.lock().unwrap();
        if preparations.len() >= 64 && !preparations.contains_key(&key) {
            return Ok(false);
        }
        if let Some(previous) = preparations.insert(
            key.clone(),
            PreparationMeta {
                generation,
                root,
                admission: admission.clone(),
                cancel,
                promoted,
            },
        ) {
            previous.cancel.send_replace(true);
        }
    }
    let _preparation = PreparationGuard {
        key: key.clone(),
        generation,
    };
    let plan = tokio::select! {
        result = prepare_warmup(url, origin) => result?,
        _ = cancel_rx.wait_for(|cancelled| *cancelled) => return Ok(false),
        _ = epoch_rx.wait_for(|current| *current != admission.load(Ordering::Acquire)) => return Ok(false),
    };
    // await 与发布之间可能发生取消/新准备/重预热。发布必须复核同一 registry 代际，
    // 并与 session 切换原子排序；迟到的旧计划不得复活、覆盖新计划或 queued 任务。
    let _session = SESSION.lock().unwrap();
    let preparations = PREPARATIONS.lock().unwrap();
    let Some(preparation) = preparations
        .get(&key)
        .filter(|entry| entry.generation == generation)
    else {
        return Ok(false);
    };
    if *cancel_rx.borrow() || *SESSION_EPOCH.borrow() != admission.load(Ordering::Acquire) {
        return Ok(false);
    }
    let mut entries = WARMUPS.lock().unwrap();
    if entries
        .get(&key)
        .is_some_and(|entry| entry.state == "queued" || entry.generation > generation)
    {
        return Ok(true);
    }
    if entries.len() >= 64 && !entries.contains_key(&key) {
        let oldest = entries
            .iter()
            .filter(|(_, entry)| entry.state != "queued")
            .min_by_key(|(_, entry)| entry.generation)
            .map(|(key, _)| key.clone());
        if let Some(oldest) = oldest {
            entries.remove(&oldest);
        } else {
            return Ok(false);
        }
    }
    entries.insert(
        key.clone(),
        WarmupEntry {
            generation,
            root: preparation.root.clone(),
            state: "idle",
            reason: None,
            plan: Some(plan),
            intent_deadline: std::time::Instant::now() + Duration::from_secs(60),
            admission: admission.clone(),
            cancel: preparation.cancel.clone(),
            promoted: preparation.promoted,
        },
    );
    Ok(true)
}

pub async fn warmup_playlist(url: &str, origin: &str) -> Result<bool, String> {
    let key = cache_key(url);
    let mut epoch_rx = SESSION_EPOCH.subscribe();
    let epoch = *epoch_rx.borrow();
    let generation = WARMUP_SEQ.fetch_add(1, Ordering::Relaxed);
    let admission = Arc::new(AtomicU64::new(epoch));
    let (cancel, mut cancel_rx) = tokio::sync::watch::channel(false);
    let root = playlist_root(url);
    let promoted = SESSION.lock().is_ok_and(|session| {
        session
            .active
            .as_deref()
            .is_some_and(|active| playlist_root(active) == root)
    });
    {
        let mut entries = WARMUPS.lock().unwrap();
        if entries
            .get(&key)
            .is_some_and(|entry| entry.state == "queued")
        {
            return Ok(true);
        }
        if entries.len() >= 64 && !entries.contains_key(&key) {
            let oldest = entries
                .iter()
                .filter(|(_, entry)| entry.state != "queued")
                .min_by_key(|(_, entry)| entry.generation)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                entries.remove(&oldest);
            } else {
                return Ok(false);
            }
        }
        entries.insert(
            key.clone(),
            WarmupEntry {
                generation,
                root,
                state: "queued",
                reason: None,
                plan: None,
                intent_deadline: std::time::Instant::now() + Duration::from_secs(60),
                admission: admission.clone(),
                cancel,
                promoted,
            },
        );
    }
    let prepared = tokio::select! {
        result = prepare_warmup(url, origin) => result,
        _ = cancel_rx.wait_for(|cancelled| *cancelled) => {
            update_warmup(&key, generation, "cancelled", Some("requested"));
            return Ok(false);
        },
        _ = epoch_rx.wait_for(|current| *current != admission.load(Ordering::Acquire)) => {
            update_warmup(&key, generation, "cancelled", Some("session_changed"));
            return Ok(false);
        },
    };
    let plan = match prepared {
        Ok(plan) => plan,
        Err(error) => {
            update_warmup(&key, generation, "failed", Some(warmup_failure(&error)));
            return Err(error);
        }
    };
    if *cancel_rx.borrow() || *SESSION_EPOCH.borrow() != admission.load(Ordering::Acquire) {
        update_warmup(&key, generation, "cancelled", Some("session_changed"));
        return Ok(false);
    }
    if let Ok(mut entries) = WARMUPS.lock() {
        if let Some(entry) = entries
            .get_mut(&key)
            .filter(|entry| entry.generation == generation)
        {
            entry.plan = Some(plan.clone());
        }
    }
    if memory_has(&plan.segment) && plan.dependencies.iter().all(|url| memory_has(url)) {
        update_warmup(&key, generation, "ready", None);
        return Ok(true);
    }
    let queued = spawn_background_work(
        plan.segment.clone(),
        admission,
        Some((key.clone(), generation)),
        Some(plan),
    );
    if !queued {
        update_warmup(&key, generation, "busy", Some("background_busy"));
    }
    Ok(queued)
}

/// 已准备的当前代 warm 意图只让普通后继“不新派发”，不取消任何前台/后台工作。
/// 原文 TTL 与固定60s意图期限双约束，反复读取/续缓存也不会让旧 busy 永久占优。
fn pending_warmup_intent() -> bool {
    let now = std::time::Instant::now();
    let plans = {
        // 与 set_session 相同的 SESSION -> WARMUPS 锁顺序；缓存检查放到锁外。
        let Ok(session) = SESSION.lock() else {
            return false;
        };
        if session.active.is_none() {
            return false;
        }
        let epoch = *SESSION_EPOCH.borrow();
        let Ok(entries) = WARMUPS.lock() else {
            return false;
        };
        entries
            .values()
            .filter(|entry| {
                matches!(entry.state, "busy" | "queued")
                    && now < entry.intent_deadline
                    && entry.admission.load(Ordering::Acquire) == epoch
                    && !*entry.cancel.borrow()
            })
            .filter_map(|entry| entry.plan.clone())
            .collect::<Vec<_>>()
    };
    plans.iter().any(|plan| {
        let valid = RAW_PL_CACHE.lock().is_ok_and(|cache| {
            plan.playlists.iter().all(|url| {
                cache
                    .get(url)
                    .is_some_and(|playlist| now < playlist.expires)
            })
        });
        valid && !(memory_has(&plan.segment) && plan.dependencies.iter().all(|url| memory_has(url)))
    })
}

/// 顺序预取只从当前前台位置出发；不再递归推进旧视频，也不递增全局代际。
fn schedule_prefetch(current: &str) {
    let Some((playlist, index)) = segment_owner(current) else {
        return;
    };
    if !chain_active_for(&playlist)
        || FOREGROUND_ACTIVE.load(Ordering::Relaxed) > 0
        || pending_warmup_intent()
    {
        return;
    }
    let segments = PL_SEGMENTS
        .lock()
        .ok()
        .and_then(|lists| lists.get(&playlist).cloned());
    let Some(segments) = segments else { return };
    // 单一后台任务，不把普通预取展开成多组 Range。窗口只是候选范围。
    let ahead = match LAST_DOWNLOAD_MS.load(Ordering::Relaxed) {
        0..=1999 => 10,
        2000..=3999 => 5,
        _ => 3,
    };
    for segment in segments.iter().skip(index + 1).take(ahead) {
        let key = cache_key(segment);
        if SEG_CACHE.lock().is_ok_and(|cache| cache.contains_key(&key)) {
            continue;
        }
        let _ = spawn_background(segment.clone(), *SESSION_EPOCH.borrow(), false);
        break;
    }
}

/// 后台与前台共享缓存及分片锁；磁盘命中不再重写已经落盘的内容。
async fn background_bytes(url: &str, ranged: bool) -> Result<(), String> {
    let key = cache_key(url);
    if SEG_CACHE.lock().is_ok_and(|cache| cache.contains_key(&key))
        || KEY_CACHE.lock().is_ok_and(|cache| cache.contains_key(&key))
    {
        return Ok(());
    }
    let lock = seg_lock(&key);
    let _download = lock.lock().await;
    if SEG_CACHE.lock().is_ok_and(|cache| cache.contains_key(&key))
        || KEY_CACHE.lock().is_ok_and(|cache| cache.contains_key(&key))
    {
        return Ok(());
    }
    if let Some(bytes) = disk_get(&key) {
        store_segment_inner(&key, None, bytes.as_ref().clone(), false);
        return Ok(());
    }
    let started = std::time::Instant::now();
    let (ctype, bytes) = media_fetch(url, ranged).await?;
    let len = bytes.len();
    store_segment(url, ctype, bytes);
    crate::playback_log::record(
        "prefetch.ok",
        serde_json::json!({"resource": crate::playback_log::resource_id(url), "bytes": len, "elapsed_ms": started.elapsed().as_millis() as u64, "warmup": ranged}),
    );
    Ok(())
}

fn spawn_background(url: String, epoch: u64, _warmup: bool) -> bool {
    spawn_background_work(url, Arc::new(AtomicU64::new(epoch)), None, None)
}

fn spawn_background_work(
    url: String,
    admission: Arc<AtomicU64>,
    warmup: Option<(String, u64)>,
    plan: Option<WarmupPlan>,
) -> bool {
    let key = cache_key(&url);
    let dependencies = plan
        .as_ref()
        .map(|plan| plan.dependencies.clone())
        .unwrap_or_default();
    if memory_has(&url) && dependencies.iter().all(|url| memory_has(url)) {
        return true;
    }
    if FOREGROUND_ACTIVE.load(Ordering::Relaxed) > 0 {
        return false;
    }
    let root = segment_owner(&url)
        .map(|(playlist, _)| playlist_root(&playlist))
        .unwrap_or_else(|| cache_key(&url));
    let Ok(mut jobs) = BACKGROUND.lock() else {
        return false;
    };
    // 同片普通后继可能没有 key/init 依赖，不能把它误报成完整 warmup。
    // 返回 busy 由前端重排；下一轮会复用已完成首片并只补依赖。
    if jobs.contains_key(&key) {
        return warmup.is_none();
    }
    if !jobs.is_empty() {
        return false;
    }
    let token = BACKGROUND_SEQ.fetch_add(1, Ordering::Relaxed);
    let count = CounterGuard::new(&PREFETCH_INFLIGHT);
    BACKGROUND_META.lock().unwrap().insert(
        token,
        BackgroundMeta {
            root,
            admission: admission.clone(),
            warmup: warmup.clone(),
            promoted: false,
            count: count.0.clone(),
        },
    );
    let guard = BackgroundGuard {
        key: key.clone(),
        token,
        _count: count,
    };
    let handle = tokio::spawn(async move {
        let _guard = guard;
        if *SESSION_EPOCH.borrow() != admission.load(Ordering::Acquire) {
            if let Some((key, generation)) = &warmup {
                update_warmup(key, *generation, "cancelled", Some("session_changed"));
            }
            return;
        }
        let dependency_work = async {
            // AES key 与首片并行；init 与适用 key 组成同一个依赖分支，重任务仍只有一片。
            for dependency in &dependencies {
                background_bytes(dependency, false).await?;
            }
            Ok::<(), String>(())
        };
        let (segment, dependency) = tokio::join!(background_bytes(&url, true), dependency_work);
        let result = segment.and(dependency);
        if let Some((key, generation)) = &warmup {
            match &result {
                Ok(()) if memory_has(&url) && dependencies.iter().all(|url| memory_has(url)) => {
                    update_warmup(key, *generation, "ready", None);
                }
                Ok(()) => update_warmup(key, *generation, "failed", Some("cache_not_retained")),
                Err(error) => {
                    update_warmup(key, *generation, "failed", Some(warmup_failure(error)))
                }
            }
        }
        if let Err(error) = result {
            crate::playback_log::record(
                "prefetch.fail",
                serde_json::json!({
                    "resource": crate::playback_log::resource_id(&url), "error_kind": crate::playback_log::error_kind(&error)
                }),
            );
        }
    });
    jobs.insert(key, (token, handle.abort_handle()));
    true
}

/// 缓存体系状态快照：内存 / 磁盘两级缓存的条数与体积。
/// 设置页展示 + 远程排查用；不记录任何 URL。
pub fn cache_stats() -> serde_json::Value {
    let (mem_segments, mem_bytes) = SEG_CACHE
        .lock()
        .map(|cache| {
            cache
                .values()
                .fold((0usize, 0usize), |(count, bytes), entry| {
                    (count + 1, bytes + entry.value.1.len())
                })
        })
        .unwrap_or((0, 0));
    let keys = KEY_CACHE.lock().map(|c| c.len()).unwrap_or(0);
    let playlists = PL_CACHE.lock().map(|c| c.len()).unwrap_or(0);
    let covers = IMG_CACHE.lock().map(|c| c.len()).unwrap_or(0);
    let mut disk_files = 0usize;
    let mut disk_bytes = 0u64;
    if let Some(dir) = DISK_DIR.lock().ok().and_then(|d| d.clone()) {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                if let Ok(meta) = entry.metadata() {
                    if meta.is_file() {
                        disk_files += 1;
                        disk_bytes += meta.len();
                    }
                }
            }
        }
    }
    serde_json::json!({
        "foreground_downloads": FOREGROUND_ACTIVE.load(Ordering::Relaxed),
        "background_downloads": PREFETCH_INFLIGHT.load(Ordering::Relaxed),
        "session_active": SESSION.lock().is_ok_and(|session| session.active.is_some()),
        "mem_segments": mem_segments,
        "mem_bytes": mem_bytes,
        "keys": keys,
        "playlists": playlists,
        "covers": covers,
        "disk_files": disk_files,
        "disk_bytes": disk_bytes,
        "limits": {
            "mem_segments_max": SEG_CACHE_MAX,
            "mem_bytes_max": SEG_CACHE_TOTAL_BYTES,
            "disk_bytes_max": DISK_CACHE_MAX_BYTES,
        },
    })
}

/// 清空封面 / 播放列表 / Key / 分片内存缓存
pub fn clear_caches() {
    if let Ok(preparations) = PREPARATIONS.lock() {
        for preparation in preparations.values() {
            preparation.cancel.send_replace(true);
        }
    }
    if let Ok(mut g) = IMG_CACHE.lock() {
        g.clear();
    }
    if let Ok(mut g) = PL_CACHE.lock() {
        g.clear();
    }
    if let Ok(mut g) = RAW_PL_CACHE.lock() {
        g.clear();
    }
    if let Ok(mut entries) = WARMUPS.lock() {
        for entry in entries
            .values_mut()
            .filter(|entry| entry.state == "queued" || entry.state == "busy")
        {
            entry.state = "cancelled";
            entry.reason = Some("cache_cleared");
            entry.cancel.send_replace(true);
        }
    }
    if let Ok(mut g) = KEY_CACHE.lock() {
        g.clear();
    }
    if let Ok(mut g) = SEG_CACHE.lock() {
        g.clear();
    }
    // 预取索引跟着缓存一起清：缓存没了，索引指向的分片也不再「已就绪」
    if let Ok(mut g) = PL_SEGMENTS.lock() {
        g.clear();
    }
    if let Ok(mut g) = SEG_OWNER.lock() {
        g.clear();
    }
    cancel_background_matching(|_, _| true, "cache_cleared");
    cancel_speculative(None);
    if let Ok(mut keys) = PL_KEYS.lock() {
        keys.clear();
    }
    if let Ok(mut parents) = PL_PARENTS.lock() {
        parents.clear();
    }
    // 磁盘缓存也一起清（「清除缓存」按钮的语义是全部清掉）
    if let Some(dir) = DISK_DIR.lock().ok().and_then(|d| d.clone()) {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/* ───────────────────────── 拖动推测预取 ───────────────────────── */

/// 拖动/点击进度条时的**推测预取**：前端在拖动预览阶段就把目标位置的分片
/// 发过来，这里立刻开始下载。松手后 hls.js 的前台请求走 singleflight 等
/// 同一次下载 —— 拖动期间（通常 0.3–2 秒）已经把 8 路下载跑完大半，
/// 松手到出画的等待随之大幅缩短。实测本机网络下未缓存分片 8 路要 4 秒+，
/// 没有这一步「拖动后 1 秒内恢复」基本不可能。
///
/// 单槽语义：同一时刻只保留**最新目标**的预取任务。新目标到来时 abort 掉
/// 旧任务（半截下载不会写缓存，只浪费已跑的连接；换来的是不跟新目标抢带宽），
/// 命中缓存的请求直接返回不动任务。
static SPECULATIVE: Lazy<Mutex<Option<(String, tokio::task::JoinHandle<()>)>>> =
    Lazy::new(|| Mutex::new(None));

/// URL 匹配防止旧播放器的 cleanup 取消新目标；None 仅供会话统一退出。
pub fn cancel_speculative(url: Option<&str>) {
    let key = url.map(cache_key);
    if let Ok(mut slot) = SPECULATIVE.lock() {
        if slot
            .as_ref()
            .is_some_and(|(current, _)| key.as_ref().is_none_or(|key| key == current))
        {
            if let Some((_, handle)) = slot.take() {
                handle.abort();
            }
        }
    }
}
pub fn speculative_prefetch(url: &str) -> bool {
    let epoch = *SESSION_EPOCH.borrow();
    if !mark_foreground(url) {
        return false;
    }
    // 已有完整内存片不改变seek代际，也不撤销正在供当前播放复用的推测任务。
    if memory_has(url) {
        return true;
    }
    let key = cache_key(url);
    let Ok(mut slot) = SPECULATIVE.lock() else {
        return false;
    };
    if *SESSION_EPOCH.borrow() != epoch {
        return false;
    }
    if slot
        .as_ref()
        .is_some_and(|(current, handle)| current == &key && !handle.is_finished())
    {
        return true;
    }
    if let Some((_, old)) = slot.take() {
        old.abort();
    }
    // 仅显式定位意图换代，不影响随后正常并行请求的音视频分片。
    SEEK_EPOCH.send_modify(|epoch| *epoch = epoch.wrapping_add(1));
    let url = url.to_string();
    let handle = tokio::spawn(async move {
        let _ = load_segment(&url).await;
    });
    *slot = Some((key, handle));
    true
}

/* ───────────────────────── 协议响应 ───────────────────────── */

fn respond(status: u16, ctype: &str, body: Vec<u8>, cache: &str) -> Response<Vec<u8>> {
    let len = body.len().to_string();
    Response::builder()
        .status(StatusCode::from_u16(status).unwrap_or(StatusCode::OK))
        .header("Content-Type", ctype)
        // WKWebView 需要 Content-Length / Accept-Ranges 才肯复用已缓冲的分片
        .header("Content-Length", len)
        .header("Accept-Ranges", "bytes")
        .header(
            "Access-Control-Expose-Headers",
            "Content-Length, Content-Type, Accept-Ranges",
        )
        .header("X-Content-Type-Options", "nosniff")
        .header("Cache-Control", cache)
        .body(body)
        .unwrap_or_else(|_| Response::new(Vec::new()))
}

/// 主清单的子清单也经 /seg 返回：必须继续改写，不能把内层 URL 原样交给播放器。
fn segment_response(
    target: &str,
    ctype: Option<&str>,
    bytes: Vec<u8>,
    origin: &str,
) -> Response<Vec<u8>> {
    let playlist = ctype.is_some_and(|t| t.to_ascii_lowercase().contains("mpegurl"))
        || bytes.starts_with(b"#EXTM3U")
        || bytes.starts_with(b"\xef\xbb\xbf#EXTM3U");
    if playlist {
        return match std::str::from_utf8(&bytes) {
            Ok(text) => {
                // 内层子清单同样登记分片序列与密钥预热：预取对多级 m3u8 也要能定位后继分片
                let (rewritten, segments, keys) = rewrite_playlist(target, text, origin);
                register_playlist(target, &segments);
                register_children(target, text);
                if let Ok(mut all_keys) = PL_KEYS.lock() {
                    if all_keys.len() >= 64 {
                        all_keys.clear();
                    }
                    all_keys.insert(target.to_string(), keys);
                }
                respond(
                    200,
                    "application/vnd.apple.mpegurl",
                    rewritten.into_bytes(),
                    // 子清单与顶层同样含动态签名；浏览器不得绕过原文 TTL 长缓存。
                    "no-store",
                )
            }
            Err(_) => respond(
                502,
                "text/plain",
                b"invalid playlist encoding".to_vec(),
                "no-store",
            ),
        };
    }
    respond(
        200,
        ctype.unwrap_or("application/octet-stream"),
        bytes,
        "public, max-age=3600",
    )
}

/// 统一入口：`<LOCAL_ORIGIN>/img?u=` / `hls?u=` / `seg?u=`
///
/// `origin` 是本平台自定义协议的完整基址，见 [`LOCAL_ORIGIN`]；用于改写 m3u8 里的
/// 分片与 key 地址。正常情况下调用方直接传 [`LOCAL_ORIGIN`]。
pub async fn handle(path: &str, query: &str, origin: &str) -> Response<Vec<u8>> {
    let target = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("u="))
        .map(dec)
        .unwrap_or_default();

    if target.is_empty() {
        return respond(400, "text/plain", b"missing u".to_vec(), "no-store");
    }

    let started = std::time::Instant::now();
    let route = match path {
        "/img" => "image",
        "/hls" => "playlist",
        "/seg" => "segment_or_key",
        _ => "unknown",
    };
    crate::playback_log::record(
        "proxy.begin",
        serde_json::json!({"route": route, "resource": crate::playback_log::resource_id(&target)}),
    );
    let response = match path {
        "/img" => match load_image(&target).await {
            Ok((kind, bytes)) => {
                respond(200, &kind, bytes.as_ref().clone(), "public, max-age=86400")
            }
            Err(e) => respond(502, "text/plain", e.into_bytes(), "no-store"),
        },
        "/hls" => match load_playlist(&target, origin).await {
            Ok(text) => respond(
                200,
                "application/vnd.apple.mpegurl",
                text.as_bytes().to_vec(),
                "no-store",
            ),
            Err(e) => respond(502, "text/plain", e.into_bytes(), "no-store"),
        },
        "/seg" if known_playlist(&target) => match load_playlist(&target, origin).await {
            Ok(text) => respond(
                200,
                "application/vnd.apple.mpegurl",
                text.as_bytes().to_vec(),
                "no-store",
            ),
            Err(error) => respond(502, "text/plain", error.into_bytes(), "no-store"),
        },
        "/seg" => match load_segment(&target).await {
            Ok((ctype, bytes)) => {
                // 未命中路径上这个 Arc 的引用计数是 1（刚建好、还没进缓存），
                // try_unwrap 能零拷贝取出 Vec，省掉一次 2.1MB memcpy；
                // 命中时计数 >1（缓存持有一份），只能 clone，这是必须的。
                let body = Arc::try_unwrap(bytes).unwrap_or_else(|arc| (*arc).clone());
                segment_response(&target, ctype.as_deref(), body, origin)
            }
            Err(e) => respond(502, "text/plain", e.into_bytes(), "no-store"),
        },
        _ => respond(404, "text/plain", b"not found".to_vec(), "no-store"),
    };
    crate::playback_log::record(
        "proxy.end",
        serde_json::json!({"route": route, "resource": crate::playback_log::resource_id(&target), "status": response.status().as_u16(), "bytes": response.body().len(), "elapsed_ms": started.elapsed().as_millis() as u64}),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn dateranges_keep_timing_and_scte_metadata_but_drop_external_resources() {
        let line = "#EXTM3U\n#EXT-X-DATERANGE:ID=\"event\",START-DATE=\"2026-11-07T00:00:00Z\",DURATION=4,SCTE35-OUT=0xFC,X-TEXT=\"a,b\",X-ASSET-URI=\"https://evil.example/ad\",X-ASSET-LIST=\"https://evil.example/list.json\"\n";
        let out = rewrite_playlist("https://cdn.example/master.m3u8", line, LOCAL_ORIGIN).0;
        assert!(out.contains("DURATION=4,SCTE35-OUT=0xFC,X-TEXT=\"a,b\""));
        assert!(out.contains("START-DATE="));
        assert!(!out.contains("evil.example") && !out.contains("X-ASSET"));
    }

    #[test]
    fn every_uri_attribute_is_rewritten_without_splitting_quoted_commas() {
        let line = "#EXT-X-MEDIA:NAME=\"audio,main\",URI=\"first.m3u8\",URI=\"second.m3u8\"";
        let absolute = absolutize_uri_in_attr("https://cdn.example/master.m3u8", line);
        let rewritten = rewrite_uri_attr(&absolute, LOCAL_ORIGIN);
        assert_eq!(
            rewritten.matches(&format!("{LOCAL_ORIGIN}/seg?u=")).count(),
            2
        );
        assert!(rewritten.contains("NAME=\"audio,main\""));
        assert!(!rewritten.contains("URI=\"https://"));
    }

    #[test]
    fn nested_playlists_and_alternate_uris_stay_behind_proxy() {
        let text = "#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,URI=\"audio/list.m3u8\"\n#EXT-X-SESSION-KEY:METHOD=AES-128,URI=\"http://127.0.0.1/key\"\n#EXT-X-PART:DURATION=1,URI=\"part.ts\"\nvariant/list.m3u8\n";
        let response = segment_response(
            "https://cdn.example/master.m3u8",
            Some("application/octet-stream"),
            text.as_bytes().to_vec(),
            LOCAL_ORIGIN,
        );
        let body = std::str::from_utf8(response.body()).unwrap();
        assert_eq!(body.matches(&format!("{LOCAL_ORIGIN}/seg?u=")).count(), 4);
        // Linux 上 LOCAL_ORIGIN 本身就是 `http://127.0.0.1:<port>`，
        // 所以不能断言「不含 127.0.0.1」，要断言上游那条 key 地址已被代理包起来
        assert!(!body.contains("URI=\"http://127.0.0.1/key\""));
        assert!(body.contains("https%3A%2F%2Fcdn%2Eexample%2Faudio%2Flist%2Em3u8"));
        assert_eq!(
            response.headers()["Content-Type"],
            "application/vnd.apple.mpegurl"
        );
        assert_eq!(response.headers()["X-Content-Type-Options"], "nosniff");
        assert!(!response
            .headers()
            .contains_key("Access-Control-Allow-Origin"));
        let optional = rewrite_playlist("https://cdn.example/list.m3u8",
            "#EXTM3U\n#EXT-X-CONTENT-STEERING:SERVER-URI=\"https://other.example/list.json\"\n#EXT-X-DATERANGE:ID=\"ad\",X-ASSET-LIST=\"http://127.0.0.1/assets\"\nsegment.ts\n", LOCAL_ORIGIN).0;
        assert!(!optional.contains("CONTENT-STEERING") && !optional.contains("X-ASSET-LIST"));
        assert!(optional.contains("#EXT-X-DATERANGE:ID=\"ad\""));
        let bom = segment_response(
            "https://cdn.example/list.m3u8",
            None,
            b"\xef\xbb\xbf#EXTM3U\nsegment.ts\n".to_vec(),
            LOCAL_ORIGIN,
        );
        assert!(bom.body().starts_with(b"#EXTM3U\n"));
    }

    /// 联网测试：默认跳过，手动跑 `cargo test --lib -- --ignored`。
    /// 依赖上游 `/api/videos/117/play` 返回的实时播放地址。
    #[tokio::test]
    #[ignore = "联网测试：依赖上游实时播放地址，手动用 --ignored 运行"]
    async fn live_play_chain() {
        use crate::net::edge;
        let play = edge(
            &crate::net::base(),
            "GET",
            "/api/videos/117/play?ep=3&page_texts=0",
            None,
            None,
        )
        .await
        .expect("play api");
        let url = play["video_url"].as_str().expect("video_url").to_string();
        println!("play url: {}", &url[..url.len().min(90)]);
        let (_, bytes) = fetch_bytes(&url).await.expect("fetch playlist");
        let text = String::from_utf8_lossy(&bytes);
        println!(
            "raw playlist len={} head={}",
            text.len(),
            &text[..text.len().min(160)]
        );
        let q = format!("u={}", enc(&url));
        let resp = handle("/hls", &q, LOCAL_ORIGIN).await;
        let body = String::from_utf8_lossy(resp.body()).to_string();
        println!("proxied status={} len={}", resp.status(), body.len());
        assert!(body.contains("#EXTM3U"));
        // 取一个分片验证
        let seg_line = body
            .lines()
            .find(|l| !l.starts_with('#') && l.contains("/seg?u="))
            .expect("seg line")
            .to_string();
        println!("seg line: {}", &seg_line[..seg_line.len().min(120)]);
        let seg_q = seg_line.split("?u=").nth(1).unwrap();
        let seg = handle("/seg", &format!("u={seg_q}"), LOCAL_ORIGIN).await;
        println!("segment status={} len={}", seg.status(), seg.body().len());
        assert_eq!(seg.status(), 200);
        assert!(seg.body().len() > 1000);
    }

    /// 离线回归：多码率 master playlist 的子清单不得被当作分片收集——
    /// 否则预热会去下载子清单文本（预热无效），预取索引也会错位。
    #[test]
    fn master_playlist_variants_are_not_collected_as_segments() {
        let master = "#EXTM3U\n\
#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360\n\
low/index.m3u8\n\
#EXT-X-STREAM-INF:BANDWIDTH=2000000,RESOLUTION=1280x720\n\
high/index.m3u8?auth_key=1-0-x\n";
        let (body, segments, keys) = rewrite_playlist(
            "https://cdn.example/master.m3u8?auth_key=9-0-x",
            master,
            LOCAL_ORIGIN,
        );
        assert!(
            segments.is_empty(),
            "master 里没有真正的媒体分片：{segments:?}"
        );
        assert!(keys.is_empty());
        // 子清单地址仍要改写成本地代理（播放器要经代理拉子清单）
        assert_eq!(
            body.matches(&format!("{LOCAL_ORIGIN}/seg?u=")).count(),
            2,
            "{body}"
        );
        assert!(!body.contains("low/index.m3u8"), "子清单地址应已代理包装");

        // media playlist（真正的分片）不受影响
        let media = "#EXTM3U\n#EXTINF:4,\nseg-0.ts\n#EXTINF:4,\nseg-1.ts\n";
        let (_, media_segments, _) =
            rewrite_playlist("https://cdn.example/media.m3u8", media, LOCAL_ORIGIN);
        assert_eq!(media_segments.len(), 2);

        // 后缀兜底：即便没有 EXT-X-STREAM-INF，.m3u8 行也不当分片
        let bare = "#EXTM3U\nother/list.m3u8\nseg-0.ts\n";
        let (_, bare_segments, _) =
            rewrite_playlist("https://cdn.example/media.m3u8", bare, LOCAL_ORIGIN);
        assert_eq!(bare_segments.len(), 1, "{bare_segments:?}");
        assert!(bare_segments[0].ends_with("seg-0.ts"));
    }

    /// 离线回归：磁盘缓存 LRU 的 mtime 触碰必须真实生效——
    /// 「append 打开再关闭」在 Windows 上不更新 LastWriteTime（实测），
    /// 会让「按 mtime 淘汰最旧」错杀常看分片。
    #[test]
    fn filetime_touch_actually_updates_mtime() {
        let dir = std::env::temp_dir().join("hghub-filetime-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("touch-check.seg");
        std::fs::write(&path, b"payload").unwrap();
        // 把 mtime 拨回过去，模拟一个「久未使用」的缓存文件
        let old = std::time::SystemTime::now() - Duration::from_secs(3600);
        {
            let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            f.set_modified(old).unwrap();
        }
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(before, old, "准备阶段：mtime 应已拨回过去");

        filetime_touch(&path).expect("touch");
        let after = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert!(
            after > old,
            "touch 后 mtime 必须前进到当前时间（LRU 触碰生效）"
        );
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    /// 离线回归：缓存键剥离 auth_key 签名 + 归一 tp<N> 轮换子域——
    /// 同一内容的不同签名/不同 CDN 边缘命中同一份缓存，
    /// 这是磁盘缓存跨启动/签名轮换/边缘轮换仍然有效的关键。
    #[test]
    fn cache_key_strips_signature_and_cross_signature_hits_work() {
        // 剥离 auth_key，保留其他参数
        assert_eq!(
            cache_key(
                "https://tp1.drleao.cn/videos5/abc/abc0.ts?auth_key=1794417187-64-0-x&v=3&via=hgdj"
            ),
            "https://tp.drleao.cn/videos5/abc/abc0.ts?v=3&via=hgdj"
        );
        // tp<N> 子域归一：tp1/tp3/tp6 同一份缓存
        assert_eq!(
            cache_key("https://tp3.drleao.cn/videos5/abc/abc0.ts?v=3"),
            cache_key("https://tp6.drleao.cn/videos5/abc/abc0.ts?v=3")
        );
        // 只有 auth_key 时退化为无查询串
        assert_eq!(
            cache_key("https://tp1.drleao.cn/videos5/abc/abc0.ts?auth_key=123-0-x"),
            "https://tp.drleao.cn/videos5/abc/abc0.ts"
        );
        // 无查询串原样（含 tp 归一）
        assert_eq!(
            cache_key("https://tp4.drleao.cn/videos5/abc/abc0.ts"),
            "https://tp.drleao.cn/videos5/abc/abc0.ts"
        );
        // 非 tp 池主机不动（pic/yd-lphu-long 等各自独立）
        assert_eq!(
            cache_key("https://pic.drleao.cn/a.jpg?auth_key=1-0-x"),
            "https://pic.drleao.cn/a.jpg"
        );
        assert_eq!(
            cache_key("https://yd-lphu-long.hzjvlo.cn/v/a.ts?auth_key=1-0-x"),
            "https://yd-lphu-long.hzjvlo.cn/v/a.ts"
        );
        // 解析失败的输入原样返回
        assert_eq!(cache_key("not a url"), "not a url");

        // 跨签名 + 跨边缘命中：URL-A 下载入库，URL-B（同路径、新签名、tp3 边缘）直接命中
        let url_a = "https://tp1.drleao.cn/videos5/abc/seg-0.ts?auth_key=100-0-a";
        let url_b = "https://tp3.drleao.cn/videos5/abc/seg-0.ts?auth_key=200-0-b";
        store_segment(url_a, Some("video/mp2t".into()), vec![0u8; 70000]);
        let (ctype, hit) = lru_get(&SEG_CACHE, &cache_key(url_b)).expect("cross-signature hit");
        assert_eq!(ctype, "video/mp2t");
        assert_eq!(hit.len(), 70000);
        // 磁盘键同样归一化：URL-B 的键对应 URL-A 落盘的文件
        let dir = DISK_DIR.lock().ok().and_then(|d| d.clone());
        if let Some(dir) = dir {
            let path = dir.join(disk_key(&cache_key(url_b)));
            assert!(path.exists(), "URL-B 的归一化键应能读到 URL-A 落盘的文件");
            let _ = std::fs::remove_file(path);
        }
        // 清理内存缓存，避免影响其他测试
        SEG_CACHE.lock().unwrap().clear();
        KEY_CACHE.lock().unwrap().clear();
    }

    #[tokio::test]
    async fn foreground_counter_is_released_when_task_is_aborted() {
        static COUNT: AtomicUsize = AtomicUsize::new(0);
        let (entered, ready) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _guard = CounterGuard::new(&COUNT);
            let _ = entered.send(());
            std::future::pending::<()>().await;
        });
        ready.await.unwrap();
        assert_eq!(COUNT.load(Ordering::Relaxed), 1);
        task.abort();
        let _ = task.await;
        assert_eq!(COUNT.load(Ordering::Relaxed), 0);
    }

    // 与既有全局缓存/会话测试一样，使用 --test-threads=1。
    #[tokio::test]
    async fn cached_speculative_target_does_not_advance_seek_epoch_or_cancel_current_slot() {
        struct MarkDrop(Arc<AtomicBool>);
        impl Drop for MarkDrop {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let dropped = Arc::new(AtomicBool::new(false));
        let flag = dropped.clone();
        let (started, ready) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(async move {
            let _guard = MarkDrop(flag);
            let _ = started.send(());
            std::future::pending::<()>().await;
        });
        ready.await.unwrap();
        let slot_key = "https://cached-seek.fixture.invalid/inflight.ts".to_string();
        let old_slot = SPECULATIVE
            .lock()
            .unwrap()
            .replace((slot_key.clone(), handle));
        let url = "https://cached-seek.fixture.invalid/cached.ts";
        let key = cache_key(url);
        let previous = SEG_CACHE.lock().unwrap().insert(
            key.clone(),
            LruEntry {
                value: ("video/mp2t".into(), Arc::new(vec![7; 1024])),
                used: next_seq(),
            },
        );
        let epoch = *SEEK_EPOCH.borrow();
        assert!(speculative_prefetch(url));
        tokio::task::yield_now().await;
        assert_eq!(*SEEK_EPOCH.borrow(), epoch);
        assert!(!dropped.load(Ordering::SeqCst));
        assert_eq!(
            SPECULATIVE.lock().unwrap().as_ref().map(|(key, _)| key),
            Some(&slot_key)
        );
        let own_slot = std::mem::replace(&mut *SPECULATIVE.lock().unwrap(), old_slot).unwrap();
        own_slot.1.abort();
        let _ = own_slot.1.await;
        let mut cache = SEG_CACHE.lock().unwrap();
        if let Some(previous) = previous {
            cache.insert(key, previous);
        } else {
            cache.remove(&key);
        }
    }

    #[test]
    fn cache_pool_normalization_is_scoped_and_idempotent() {
        let a = cache_key("https://tp12.drleao.cn/a.ts?auth_key=old");
        assert_eq!(a, "https://tp.drleao.cn/a.ts");
        assert_eq!(cache_key(&a), a);
        assert_eq!(
            cache_key("https://tp12.example.com/a.ts"),
            "https://tp12.example.com/a.ts"
        );
    }

    #[test]
    fn activity_is_scoped_to_the_playlist() {
        let base = "https://activity.fixture.invalid/list.m3u8";
        let mut session = SESSION.lock().unwrap();
        let previous = std::mem::take(&mut *session);
        session.foreground = Some((cache_key(base), std::time::Instant::now()));
        drop(session);
        assert!(chain_active_for(base));
        assert!(!chain_active_for(
            "https://different.fixture.invalid/list.m3u8"
        ));
        *SESSION.lock().unwrap() = previous;
    }

    #[test]
    fn extensionless_variants_and_renditions_share_master_session() {
        let master = "https://session.fixture.invalid/master";
        register_children(master, "#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,URI=\"audio?id=1\"\n#EXT-X-STREAM-INF:BANDWIDTH=1\nvideo?id=1\n");
        assert_eq!(
            playlist_root("https://session.fixture.invalid/video?id=1"),
            cache_key(master)
        );
        assert_eq!(
            playlist_root("https://session.fixture.invalid/audio?id=1"),
            cache_key(master)
        );
    }

    #[test]
    fn stale_cleanup_does_not_deactivate_the_new_source() {
        let previous = std::mem::take(&mut *SESSION.lock().unwrap());
        set_session("https://cleanup.fixture.invalid/old.m3u8", true);
        set_session("https://cleanup.fixture.invalid/new.m3u8", true);
        let epoch = *SESSION_EPOCH.borrow();
        set_session("https://cleanup.fixture.invalid/old.m3u8", false);
        assert_eq!(*SESSION_EPOCH.borrow(), epoch);
        assert_eq!(
            SESSION.lock().unwrap().active.as_deref(),
            Some("https://cleanup.fixture.invalid/new.m3u8")
        );
        *SESSION.lock().unwrap() = previous;
    }

    #[tokio::test]
    async fn cancelled_background_drops_its_slot_and_counter() {
        let key = "cancelled-background.fixture.invalid".to_string();
        let token = BACKGROUND_SEQ.fetch_add(1, Ordering::Relaxed);
        let before = PREFETCH_INFLIGHT.load(Ordering::Relaxed);
        let guard = BackgroundGuard {
            key: key.clone(),
            token,
            _count: CounterGuard::new(&PREFETCH_INFLIGHT),
        };
        let (entered, ready) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _guard = guard;
            let _ = entered.send(());
            std::future::pending::<()>().await;
        });
        BACKGROUND
            .lock()
            .unwrap()
            .insert(key.clone(), (token, task.abort_handle()));
        ready.await.unwrap();
        cancel_background_except(None);
        let _ = task.await;
        assert_eq!(PREFETCH_INFLIGHT.load(Ordering::Relaxed), before);
        assert!(!BACKGROUND.lock().unwrap().contains_key(&key));
    }

    #[tokio::test]
    async fn background_reads_disk_without_a_second_download_or_write() {
        let dir =
            std::env::temp_dir().join(format!("hghub-background-cache-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let previous = DISK_DIR.lock().unwrap().replace(dir.clone());
        let url = "https://disk-only.fixture.invalid/test.ts";
        let key = cache_key(url);
        let path = dir.join(disk_key(&key));
        let bytes = vec![17u8; 70000];
        std::fs::write(&path, &bytes).unwrap();
        background_bytes(url, true).await.unwrap();
        assert_eq!(
            lru_get(&SEG_CACHE, &key).unwrap().1.as_slice(),
            bytes.as_slice()
        );
        assert!(!path.with_extension("tmp").exists());
        SEG_CACHE.lock().unwrap().remove(&key);
        *DISK_DIR.lock().unwrap() = previous;
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_dir(dir);
    }

    /// 离线回归：不再依赖带 auth_key 时效的真实上游地址（旧地址已过期，会 403）。
    /// 与真实播放列表同构：带签名的分片地址 + 加密 KEY + 相对路径分片。
    #[test]
    fn playlist_rewrite_works() {
        let base = "https://cdn.example/videos5/abc/abc.m3u8?auth_key=9999999999-0-0-deadbeef&v=3&via=hgdj";
        let text = "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin?auth_key=9999999999-0-0-deadbeef\"\n#EXTINF:4,\nseg-1.ts?auth_key=9999999999-0-0-deadbeef\n";
        let (body, segments, keys) = rewrite_playlist(base, text, LOCAL_ORIGIN);
        assert_eq!(segments.len(), 1, "分片序列应被收集供预取使用");
        assert!(
            segments[0].starts_with("https://cdn.example/videos5/abc/seg-1.ts"),
            "收集到的分片必须是绝对地址：{}",
            segments[0]
        );
        // AES 密钥地址纳入预热收集（绝对化 + 保留签名）
        assert_eq!(keys.len(), 1, "KEY URI 应被收集供预热使用");
        assert!(
            keys[0].starts_with("https://cdn.example/videos5/abc/key.bin"),
            "收集到的密钥必须是绝对地址：{}",
            keys[0]
        );

        assert!(body.contains("#EXTM3U"), "not a playlist: {body}");
        assert!(
            body.contains(&format!("{LOCAL_ORIGIN}/seg?u=")),
            "segments not rewritten: {body}"
        );
        // 分片必须绝对化到源站，并保留签名查询串。
        assert!(
            body.contains(
                "https%3A%2F%2Fcdn%2Eexample%2Fvideos5%2Fabc%2Fseg%2D1%2Ets%3Fauth%5Fkey%3D"
            ),
            "segment not absolutized with query: {body}"
        );
        // 加密 KEY 也要走本地代理，否则 hls.js 会直接打源站被 CORS 拦住。
        assert!(
            body.contains("URI=\"") && body.contains("%2Fkey%2Ebin%3Fauth%5Fkey%3D"),
            "key uri not rewritten: {body}"
        );
        assert!(!body.contains("URI=\"key.bin"), "key uri left relative");
    }

    /// 离线回归：用已知明文 + 项目内置 key/IV 构造密文，验证解密、去填充与类型识别。
    /// 真实封面抓取那条仍保留为 `#[ignore]` 联网测试。
    #[test]
    fn cover_decrypt_roundtrip() {
        use aes::cipher::BlockEncrypt;

        let plain = b"\xFF\xD8\xFF\xE0cover-payload-for-test".to_vec();
        let mut buf = plain.clone();
        let pad = 16 - plain.len() % 16;
        buf.extend(std::iter::repeat_n(pad as u8, pad));

        let cipher = Aes128::new_from_slice(IMG_KEY).expect("key");
        let mut prev = [0u8; 16];
        prev.copy_from_slice(IMG_IV);
        let mut encrypted = Vec::with_capacity(buf.len());
        for chunk in buf.chunks(16) {
            let mut block = [0u8; 16];
            block.copy_from_slice(chunk);
            for i in 0..16 {
                block[i] ^= prev[i];
            }
            let mut out_block = block;
            cipher.encrypt_block((&mut out_block).into());
            encrypted.extend_from_slice(&out_block);
            prev = out_block;
        }

        let decrypted = aes_cbc_decrypt(&encrypted).expect("decrypt");
        let stripped = strip_padding(decrypted);
        assert_eq!(stripped, plain);
        assert_eq!(image_kind(&stripped), Some("image/jpeg"));
    }

    /// 联网基准：默认跳过，手动运行
    /// `cargo test --lib live_segment_download_benchmark -- --ignored --nocapture`
    /// 测量当前网络下真实 CDN 分片的下载耗时分布（单连接 / 4 路 / 8 路 /
    /// 温热连接池 / 冷客户端），为分段并发数与连接池策略调优提供数据。
    /// 样本取播放列表中间三片，避免与首片预热路径相互干扰。
    #[tokio::test]
    #[ignore = "联网基准：依赖上游播放地址与 CDN 可达，手动用 --ignored --nocapture 运行"]
    async fn live_segment_download_benchmark() {
        use crate::net::edge;

        // 线路：默认线路失败时退到备用固定线路（国内连不上 .com 是常态）。
        // 视频从首页 Feed 取真实存在的 id（老测试里硬编码的 117 已下架），
        // 集数从 1 起逐个试，兼容 video_url 与 streams.main 两种返回字段。
        let mut playlist_url = None;
        'outer: for candidate in [crate::net::base(), "https://huangguoai.ai".to_string()] {
            let feed = match edge(&candidate, "GET", "/api/videos?page=1&size=5", None, None).await
            {
                Ok(v) => v,
                Err(e) => {
                    println!("line {candidate} feed failed: {e}");
                    continue;
                }
            };
            let ids: Vec<u64> = feed["items"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item["id"].as_u64())
                        .collect()
                })
                .unwrap_or_default();
            println!("line {candidate} feed ids={ids:?}");
            for id in ids {
                // 与 api.rs::api_play 一致：优先 POST episodes/{ep}/playback，退回旧 GET 接口
                for (method, path) in [
                    ("POST", format!("/api/videos/{id}/episodes/1/playback")),
                    ("GET", format!("/api/videos/{id}/play?ep=1&page_texts=0")),
                ] {
                    let body = if method == "POST" {
                        Some(serde_json::json!({}))
                    } else {
                        None
                    };
                    match edge(&candidate, method, &path, body, None).await {
                        Ok(play) => {
                            let main = play["video_url"]
                                .as_str()
                                .map(str::to_string)
                                .or_else(|| play["streams"]["main"].as_str().map(str::to_string));
                            match main.filter(|u| u.starts_with("http")) {
                                Some(u) => {
                                    println!(
                                        "play ok: line={candidate} video={id} {method} url={}",
                                        &u[..u.len().min(100)]
                                    );
                                    playlist_url = Some(u);
                                    break 'outer;
                                }
                                None => {
                                    println!("play empty: line={candidate} video={id} {method}")
                                }
                            }
                        }
                        Err(e) => println!("play fail: line={candidate} video={id} {method}: {e}"),
                    }
                }
            }
        }
        let url = playlist_url.expect("需要至少一条可用线路");

        // 直接拉原始 m3u8（不经代理缓存），取中间三片作为样本
        let (_, bytes) = fetch_bytes(&url).await.expect("fetch playlist");
        let text = String::from_utf8_lossy(&bytes);
        let segments: Vec<String> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(|l| absolutize(&url, l))
            .collect();
        let mid = segments.len() / 2;
        assert!(
            segments.len() > mid + 7,
            "样本播放列表分片过少: {}",
            segments.len()
        );
        println!(
            "segments={} sample_host={}",
            segments.len(),
            url::Url::parse(&segments[mid])
                .ok()
                .and_then(|u| u.host_str().map(str::to_string))
                .unwrap_or_default()
        );

        // 方法学（实测教训 2026-11-12）：同一 URL 连下 4 遍后第 5 遍触发 CDN 403 风控。
        // 因此**每个阶段用不同分片、一次一拉**（与真实播放每片只下一次的形态一致），
        // 阶段间留 1.5s 间隔。分段下载内部有 206 区间校验，不完整自动回退整包。
        let stages: [(&str, usize, usize); 6] = [
            ("single", mid, 0),          // 单连接整包（历史基线）
            ("ranged4", mid + 1, 4),     // 4 路分段
            ("ranged8", mid + 2, 8),     // 8 路分段（当前前台路径）
            ("ranged12", mid + 3, 12),   // 12 路（试验档）
            ("warm_pool", mid + 4, 8),   // 温热连接池（前面几轮已把池填热）
            ("cold_client", mid + 5, 8), // 冷客户端（强制重建后的握手+慢启动代价）
        ];
        let mut sizes: Vec<usize> = Vec::new();
        for (index, (name, sample, parts)) in stages.iter().enumerate() {
            if index == 0 {
                crate::net::refresh_client(); // 单连接基线从冷客户端开始，与前几轮口径一致
            }
            if name.eq(&"cold_client") {
                crate::net::refresh_client();
            }
            let started = std::time::Instant::now();
            let result = if *parts == 0 {
                fetch_bytes(&segments[*sample]).await
            } else {
                fetch_bytes_ranged(&segments[*sample], *parts).await
            };
            match result {
                Ok((_, bytes)) => {
                    println!(
                        "{name}={}ms bytes={}",
                        started.elapsed().as_millis(),
                        bytes.len()
                    );
                    sizes.push(bytes.len());
                }
                Err(e) => println!("{name} FAILED: {e}"),
            }
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        }
        assert!(
            sizes.iter().all(|s| *s > 100_000),
            "各阶段分片下载均应成功: {sizes:?}"
        );
    }

    /// 联网端到端基准：走完整代理链路（`handle` → 缓存/预取/singleflight），
    /// 手动运行：`cargo test --lib live_proxy_chain_benchmark -- --ignored --nocapture`
    /// 量化真实播放形态下的各路径耗时：
    /// ① 播放列表（含首片预热排队）→ ② 首片（大概率命中预热缓存，模拟「点开就播」）
    /// → ③ 未缓存中部片（8 路下载，模拟远距离跳转）→ ④ 同片再拉（内存缓存命中，
    /// 模拟「拖动回看」）→ ⑤ 另一片两次（第一次网络、第二次命中）。
    /// 每个网络分片只拉一次，避免触发 CDN 频率风控。
    #[tokio::test]
    #[ignore = "联网端到端基准：依赖上游播放地址与 CDN 可达，手动用 --ignored --nocapture 运行"]
    async fn live_proxy_chain_benchmark() {
        use crate::net::edge;

        // 线路与播放地址：与 live_segment_download_benchmark 相同的获取方式
        let mut playlist_url = None;
        'outer: for candidate in [crate::net::base(), "https://huangguoai.ai".to_string()] {
            let feed = match edge(&candidate, "GET", "/api/videos?page=1&size=5", None, None).await
            {
                Ok(v) => v,
                Err(e) => {
                    println!("line {candidate} feed failed: {e}");
                    continue;
                }
            };
            let ids: Vec<u64> = feed["items"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item["id"].as_u64())
                        .collect()
                })
                .unwrap_or_default();
            for id in ids {
                let path = format!("/api/videos/{id}/episodes/1/playback");
                if let Ok(play) =
                    edge(&candidate, "POST", &path, Some(serde_json::json!({})), None).await
                {
                    if let Some(u) = play["video_url"]
                        .as_str()
                        .map(str::to_string)
                        .or_else(|| play["streams"]["main"].as_str().map(str::to_string))
                        .filter(|u| u.starts_with("http"))
                    {
                        println!("play ok: line={candidate} video={id}");
                        playlist_url = Some(u);
                        break 'outer;
                    }
                }
            }
        }
        let url = playlist_url.expect("需要至少一条可用线路");
        clear_caches();

        // ① 播放列表（内部完成改写 + 注册分片序列 + 首片预热排队）
        let q = format!("u={}", enc(&url));
        let started = std::time::Instant::now();
        let playlist = handle("/hls", &q, LOCAL_ORIGIN).await;
        println!(
            "playlist={}ms status={} len={}",
            started.elapsed().as_millis(),
            playlist.status(),
            playlist.body().len()
        );
        assert!(playlist.body().starts_with(b"#EXTM3U"));

        // 从改写后的播放列表里取本地代理分片地址
        let body = String::from_utf8_lossy(playlist.body()).to_string();
        let seg_lines: Vec<String> = body
            .lines()
            .filter(|l| !l.trim().is_empty() && !l.trim().starts_with('#'))
            .map(str::to_string)
            .collect();
        assert!(seg_lines.len() > 4, "分片过少: {}", seg_lines.len());
        let first_seg_query = seg_lines[0]
            .split("?u=")
            .nth(1)
            .expect("first seg query")
            .to_string();
        let mid_seg_query = seg_lines[seg_lines.len() / 2]
            .split("?u=")
            .nth(1)
            .expect("mid seg query")
            .to_string();
        let mid2_seg_query = seg_lines[seg_lines.len() / 2 + 1]
            .split("?u=")
            .nth(1)
            .expect("mid2 seg query")
            .to_string();

        // ② 首片：warmup_head 已在 ① 里排队下载，这里应命中缓存（点开就播的关键路径）
        let started = std::time::Instant::now();
        let first = handle("/seg", &format!("u={first_seg_query}"), LOCAL_ORIGIN).await;
        println!(
            "first_segment={}ms status={} bytes={} (命中预热缓存应接近 0)",
            started.elapsed().as_millis(),
            first.status(),
            first.body().len()
        );
        assert_eq!(first.status(), 200);

        // ③ 未缓存中部片：8 路 ranged 下载（远距离跳转路径）
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
        let started = std::time::Instant::now();
        let mid = handle("/seg", &format!("u={mid_seg_query}"), LOCAL_ORIGIN).await;
        let mid_ms = started.elapsed().as_millis();
        println!(
            "mid_segment_cold={mid_ms}ms status={} bytes={}",
            mid.status(),
            mid.body().len()
        );
        assert_eq!(mid.status(), 200);

        // ④ 同片再拉：内存缓存命中（拖动回看路径）
        let started = std::time::Instant::now();
        let mid_again = handle("/seg", &format!("u={mid_seg_query}"), LOCAL_ORIGIN).await;
        println!(
            "mid_segment_cached={}ms bytes={}",
            started.elapsed().as_millis(),
            mid_again.body().len()
        );
        assert_eq!(mid_again.body().len(), mid.body().len());

        // ⑤ 另一片两次：第一次网络、第二次缓存命中
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
        let started = std::time::Instant::now();
        let mid2 = handle("/seg", &format!("u={mid2_seg_query}"), LOCAL_ORIGIN).await;
        println!(
            "mid2_cold={}ms bytes={}",
            started.elapsed().as_millis(),
            mid2.body().len()
        );
        let started = std::time::Instant::now();
        let mid2_again = handle("/seg", &format!("u={mid2_seg_query}"), LOCAL_ORIGIN).await;
        println!(
            "mid2_cached={}ms bytes={}",
            started.elapsed().as_millis(),
            mid2_again.body().len()
        );
        assert_eq!(mid2_again.body().len(), mid2.body().len());
    }

    /// 联网验证：上游 m3u8 的分片签名是否跨请求轮换。手动运行：
    /// `cargo test --lib live_playlist_signature_stability -- --ignored --nocapture`
    /// 只拉两次播放列表（各 ~16KB），不下载分片。若分片 auth_key 每次都变，
    /// 则 SEG_CACHE/磁盘缓存按完整 URL 做键会在重看/跨启动时全部 miss，
    /// 需要引入「剥离签名」的归一化缓存键；若稳定则现状即正确。
    #[tokio::test]
    #[ignore = "联网验证：依赖上游播放地址，手动用 --ignored --nocapture 运行"]
    async fn live_playlist_signature_stability() {
        use crate::net::edge;
        let play = edge(
            &crate::net::base(),
            "POST",
            "/api/videos/12/episodes/1/playback",
            Some(serde_json::json!({})),
            None,
        )
        .await
        .expect("play api");
        let url = play["video_url"]
            .as_str()
            .map(str::to_string)
            .or_else(|| play["streams"]["main"].as_str().map(str::to_string))
            .filter(|u| u.starts_with("http"))
            .expect("video_url");
        let (_, b1) = fetch_bytes(&url).await.expect("m3u8 #1");
        let seg1 = String::from_utf8_lossy(&b1)
            .lines()
            .find(|l| !l.trim().is_empty() && !l.trim().starts_with('#'))
            .expect("segment line #1")
            .trim()
            .to_string();
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let (_, b2) = fetch_bytes(&url).await.expect("m3u8 #2");
        let seg2 = String::from_utf8_lossy(&b2)
            .lines()
            .find(|l| !l.trim().is_empty() && !l.trim().starts_with('#'))
            .expect("segment line #2")
            .trim()
            .to_string();
        let same_content = b1 == b2;
        println!("m3u8_same_content={same_content}");
        println!("seg1={}", &seg1[..seg1.len().min(140)]);
        println!("seg2={}", &seg2[..seg2.len().min(140)]);
        println!("segment_signature_stable={}", seg1 == seg2);
        // 两次拉取内容应当一致（同一 m3u8），签名稳定性单独看分片行
        assert_eq!(b1.len(), b2.len(), "两次 m3u8 大小应一致");
    }

    /// 联网测试：默认跳过。封面地址带时效签名，不能硬编码进仓库，
    /// 需要用当次上游返回的地址显式传入：
    /// `HGHUB_TEST_COVER_URL='https://…?auth_key=…' cargo test --lib -- --ignored`
    #[tokio::test]
    #[ignore = "联网测试：需要 HGHUB_TEST_COVER_URL，手动用 --ignored 运行"]
    async fn cover_decrypt_works() {
        let url = std::env::var("HGHUB_TEST_COVER_URL")
            .expect("需要设置 HGHUB_TEST_COVER_URL 为当前有效的封面地址");
        match load_image(&url).await {
            Ok((kind, bytes)) => {
                println!("kind={kind} len={}", bytes.len());
                assert!(kind.starts_with("image/"));
            }
            Err(e) => panic!("image failed: {e}"),
        }
    }
}
