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
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
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
static PL_CACHE: Lazy<Mutex<HashMap<String, Arc<String>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
/// AES Key / 分片：拖进度条时同一 key、同一片会被反复拉，缓存住省一次往返。
static KEY_CACHE: Lazy<Mutex<HashMap<String, (String, Arc<Vec<u8>>)>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

const IMG_CACHE_MAX: usize = 240;
const PL_CACHE_MAX: usize = 60;
const KEY_CACHE_MAX: usize = 120;
/// 分片缓存按 LRU 淘汰（见 [`lru_put`]），容量按「整集大部分分片」估算：
/// 单集约 186 片 × 2.1MB，全缓存不现实，80 片（≈170MB）足以覆盖正常观看
/// 的前后各 30 秒，让回看与小幅前跳直接命中。
const SEG_CACHE_MAX: usize = 80;
const SEG_CACHE_MAX_BYTES: usize = 4 * 1024 * 1024;

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

static SEG_CACHE: Lazy<Mutex<HashMap<String, LruEntry<(String, Arc<Vec<u8>>)>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// 读缓存并刷新其 LRU 序号（命中即视为最近使用）。
fn lru_get<V: Clone>(
    map: &Mutex<HashMap<String, LruEntry<V>>>,
    key: &str,
) -> Option<V> {
    let seq = next_seq();
    let mut m = match map.lock() {
        Ok(m) => m,
        Err(_) => return None,
    };
    let entry = m.get_mut(key)?;
    entry.used = seq;
    Some(entry.value.clone())
}

/// 写缓存；满了就淘汰最久未使用的一条。
fn lru_put<V>(map: &Mutex<HashMap<String, LruEntry<V>>>, key: String, val: V, max: usize) {
    let seq = next_seq();    let mut m = match map.lock() {
        Ok(m) => m,
        Err(_) => return,
    };
    if m.len() >= max && !m.contains_key(&key) {
        if let Some(victim) = m
            .iter()
            .min_by_key(|(_, entry)| entry.used)
            .map(|(k, _)| k.clone())
        {
            m.remove(&victim);
        }
    }
    m.insert(key, LruEntry { value: val, used: seq });
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
    let hash = url
        .as_bytes()
        .iter()
        .fold(14695981039346656037u64, |h, b| {
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
    let bytes = bytes.clone();
    std::thread::spawn(move || {
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, bytes.as_slice()).is_err() {
            let _ = std::fs::remove_file(&tmp);
            return;
        }
        if std::fs::rename(&tmp, &path).is_err() {
            let _ = std::fs::remove_file(&tmp);
            return;
        }
        trim_disk_cache();
    });
}

fn filetime_touch(path: &std::path::Path) -> std::io::Result<()> {
    // 用 OpenOptions 以写方式打开并立即关闭，Windows 上会更新最后访问/写入时间；
    // 不重写内容，避免额外 IO。
    let file = std::fs::OpenOptions::new().append(true).open(path)?;
    drop(file);
    Ok(())
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
/// 分片 → (所属播放列表, 下标)。反查用，key 是上游原始分片 URL。
static SEG_OWNER: Lazy<Mutex<HashMap<String, (String, usize)>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
/// 正在预取的分片，避免同一片被重复排队。
static PREFETCH_ACTIVE: Lazy<Mutex<HashSet<String>>> = Lazy::new(|| Mutex::new(HashSet::new()));
static PREFETCH_INFLIGHT: AtomicUsize = AtomicUsize::new(0);
/// 预取代际号：每次前台播放位置变化（新的分片请求）就换代，
/// 旧代际排队的任务在真正下载前发现自己过期就放弃。
/// 没有它，远距离跳转后旧位置的预取仍占着并发额度，
/// 新位置的分片反而没人预取（实测跳转后第 4 片要 7 秒）。
static PREFETCH_GEN: AtomicU64 = AtomicU64::new(0);

/// 预取领先片数：**自适应**。单连接吞吐约 0.35–0.7MB/s（受 RTT 限制），
/// 总带宽被所有连接平分，所以领先片数要跟着「实测下载速度」走：
/// - 网络快（单片 < 2s）：领先 10 片，充分覆盖远跳/回看
/// - 网络一般（2–4s）：领先 5 片，兼顾带宽与覆盖
/// - 网络差（> 4s）：领先 3 片，别把前台带宽抢光
/// 依据是最近一次成功下载的耗时（见 [`observe_download_speed`]）。
const PREFETCH_AHEAD_FAST: usize = 10;
const PREFETCH_AHEAD_NORMAL: usize = 5;
const PREFETCH_AHEAD_SLOW: usize = 3;
/// 预取并发上限：3 路并行 ≈ 1.2 片/秒，跟得上播放节奏，同时给前台留一半带宽。
const PREFETCH_MAX_INFLIGHT: usize = 3;
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

/// 当前网络下应领先多少片。
fn prefetch_ahead() -> usize {
    match LAST_DOWNLOAD_MS.load(Ordering::Relaxed) {
        0 => PREFETCH_AHEAD_NORMAL,
        ms if ms < 2_000 => PREFETCH_AHEAD_FAST,
        ms if ms < 4_000 => PREFETCH_AHEAD_NORMAL,
        _ => PREFETCH_AHEAD_SLOW,
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

/// 取 m3u8 并把 key / 分片地址改写到本地协议。
pub async fn load_playlist(url: &str, prefix: &str) -> Result<Arc<String>, String> {
    // 缓存 key 要带 prefix：同一份 m3u8 在不同 scheme 下改写结果不一样
    let cache_key = format!("{prefix}|{url}");
    if let Some(hit) = PL_CACHE.lock().unwrap().get(&cache_key).cloned() {
        return Ok(hit);
    }
    let (_, bytes) = fetch_bytes(url).await?;
    let text = std::str::from_utf8(&bytes).map_err(|_| "播放列表不是 UTF-8 文本")?;
    if !text.trim_start_matches('\u{feff}').starts_with("#EXTM3U") {
        return Err("响应不是 HLS 播放列表".into());
    }
    let (rewritten, segments) = rewrite_playlist(url, text, prefix);
    // 注册「列表 → 分片序列」与反向索引，供播放时的后台预取定位后继分片
    register_playlist(url, &segments);
    // 首片预热：播放器拿到 m3u8 后还要发一次分片请求才会出画面，
    // 单分片 3–5 秒的下载全落在「点了没反应」的观感上。
    // 这里在返回播放列表的同时就把前两片排进预取队列，
    // 等 hls.js 真正来取时大概率已经命中缓存 —— 这是「点开就播」的关键。
    warmup_head(&segments);
    let arc = Arc::new(rewritten);
    cache_put(&PL_CACHE, cache_key, arc.clone(), PL_CACHE_MAX);
    Ok(arc)
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
            owner.insert(segment.clone(), (playlist_url.to_string(), index));
        }
    }
    if let Ok(mut lists) = PL_SEGMENTS.lock() {
        if lists.len() >= 64 {
            lists.clear();
        }
        lists.insert(playlist_url.to_string(), Arc::new(segments.to_vec()));
    }
}

fn rewrite_playlist(url: &str, text: &str, prefix: &str) -> (String, Vec<String>) {
    let mut out = String::with_capacity(text.len() + 512);
    let mut segments: Vec<String> = Vec::new();
    for raw in text.trim_start_matches('\u{feff}').lines() {
        let line = raw.trim_end();
        if line.is_empty() {
            out.push('\n');
            continue;
        }
        if line.starts_with('#') {
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
                out.push_str(&rewrite_uri_attr(&absolute, prefix));
            } else {
                out.push_str(line);
            }
        } else {
            let absolute = absolutize(url, line);
            segments.push(absolute.clone());
            out.push_str(&format!("{prefix}/seg?u={}", enc(&absolute)));
        }
        out.push('\n');
    }

    (out, segments)
}

fn absolutize_uri_in_attr(base: &str, line: &str) -> String {
    map_uri_attributes(line, |uri| absolutize(base, uri))
}

/// 分片 / AES Key 带内存缓存：拖动进度条时避免重复拉取同一片造成卡顿。
///
/// 未命中时走 singleflight：拿到分片级锁后再查一次缓存，避免与后台预取
/// 重复下载同一片（实测重复下载会让前台等 10 秒以上）。
pub async fn load_segment(url: &str) -> Result<(Option<String>, Arc<Vec<u8>>), String> {
    if let Some((ctype, hit)) = KEY_CACHE.lock().unwrap().get(url).cloned() {
        return Ok((Some(ctype), hit));
    }
    if let Some((ctype, hit)) = lru_get(&SEG_CACHE, url) {
        // 命中缓存也要触发后继预取：用户可能停下来反复回看同一段，
        // 只靠「下载成功」触发会让预取停摆。
        schedule_prefetch(url);
        return Ok((Some(ctype), hit));
    }

    let lock = seg_lock(url);
    let _guard = lock.lock().await;
    // 双检：等锁期间后台预取可能已经把这一片填进缓存
    if let Some((ctype, hit)) = lru_get(&SEG_CACHE, url) {
        drop(_guard);
        schedule_prefetch(url);
        return Ok((Some(ctype), hit));
    }

    // 磁盘缓存：上次播放（甚至上次启动）存过的分片直接读盘，
    // 省掉 3–5 秒下载。读盘 2.1MB 通常 <10ms，远快于走网络。
    if let Some(bytes) = disk_get(url) {
        let stored = store_segment(url, None, bytes.as_ref().clone());
        drop(_guard);
        schedule_prefetch(url);
        return Ok(stored);
    }

    // 分段并发下载：单连接吞吐被 RTT 限制在 0.35–0.7MB/s，一片 2.1MB 要 3–5 秒。
    // 8 路并发实测约 1.97s（4 路 2.65s），这是「远距离跳转」唯一能压进 2 秒的手段
    // （预取只能救已缓存区间）。CDN 不支持 Range 时自动退回整包下载。
    let download_started = std::time::Instant::now();
    FOREGROUND_ACTIVE.fetch_add(1, Ordering::Relaxed);
    let fetched = fetch_bytes_ranged(url, RANGED_PARTS).await;
    FOREGROUND_ACTIVE.fetch_sub(1, Ordering::Relaxed);
    let (ctype, bytes) = fetched?;
    observe_download_speed(download_started.elapsed());
    let stored = store_segment(url, ctype, bytes);
    drop(_guard);
    schedule_prefetch(url);
    Ok(stored)
}

/// 按大小分流写缓存（Key 很小、分片较大），返回给调用方的 (ctype, bytes)。
fn store_segment(
    url: &str,
    ctype: Option<String>,
    bytes: Vec<u8>,
) -> (Option<String>, Arc<Vec<u8>>) {
    let mime = ctype.clone().unwrap_or_else(|| {
        let p = url.split('?').next().unwrap_or(url).to_ascii_lowercase();
        if p.ends_with(".ts") {
            "video/mp2t".to_string()
        } else if p.ends_with(".m4s") || p.ends_with(".mp4") {
            "video/mp4".to_string()
        } else {
            "application/octet-stream".to_string()
        }
    });
    let len = bytes.len();
    let arc = Arc::new(bytes);
    if len <= 65536 {
        cache_put(&KEY_CACHE, url.to_string(), (mime.clone(), arc.clone()), KEY_CACHE_MAX);
    } else if len <= SEG_CACHE_MAX_BYTES {
        lru_put(&SEG_CACHE, url.to_string(), (mime.clone(), arc.clone()), SEG_CACHE_MAX);
        // 落盘：下次启动（或内存淘汰后）再看同一集就不必重新下载。
        // 2.1MB 的分片写盘约几毫秒，放在独立线程里，不阻塞本次响应。
        disk_put(url, &arc);
    }
    (Some(mime), arc)
}

/// 切集/切片预热：只拉播放列表并排队下载开头分片，不返回内容。
///
/// 前端在「相邻集播放地址预热成功」后调用它，切过去时首片往往已经就绪，
/// 表现为切集秒开而不是黑屏 3–5 秒。
pub async fn warmup_playlist(url: &str, origin: &str) -> Result<(), String> {
    // load_playlist 内部已完成 register_playlist + warmup_head
    load_playlist(url, origin).await.map(|_| ())
}

/// 后台预取当前分片之后若干片。
///
/// 为什么必须预取：单连接吞吐实测只有 0.35–0.7MB/s（hyper 小块读取被 RTT 放大），
/// 一片 2.1MB 要 3–5 秒，靠「拖动后再下载」永远达不到 1 秒内。把后继分片提前
/// 放进缓存，正常观看时进度条附近都是命中，拖动才是毫秒级。
///
/// 只由 `load_segment` 触发 —— 没有播放就没有分片请求，预取自然停摆，
/// 符合「仅播放中预取」的约定；并发上限 [`PREFETCH_MAX_INFLIGHT`] 给前台让路。
fn schedule_prefetch(current: &str) {
    let Some((playlist, index)) = SEG_OWNER
        .lock()
        .ok()
        .and_then(|owner| owner.get(current).cloned())
    else {
        return;
    };
    let Some(segments) = PL_SEGMENTS
        .lock()
        .ok()
        .and_then(|lists| lists.get(&playlist).cloned())
    else {
        return;
    };

    let start = index + 1;
    if start >= segments.len() {
        return;
    }
    let end = (start + prefetch_ahead()).min(segments.len());

    // 换代：本次播放位置之后的排队任务才算有效，旧位置的任务自动作废
    let generation = PREFETCH_GEN.fetch_add(1, Ordering::Relaxed) + 1;

    for segment in &segments[start..end] {
        if PREFETCH_INFLIGHT.load(Ordering::Relaxed) >= PREFETCH_MAX_INFLIGHT {
            break;
        }
        // 已在缓存里就不必再排（lru_get 会刷新序号，这里只看存在性）
        if SEG_CACHE.lock().is_ok_and(|cache| cache.contains_key(segment)) {
            continue;
        }
        // 同一片只排一次，避免连续拖动把队列塞满重复任务
        let queued = match PREFETCH_ACTIVE.lock() {
            Ok(mut active) => {
                if active.contains(segment) {
                    true
                } else {
                    if active.len() >= 64 {
                        active.clear();
                    }
                    active.insert(segment.clone());
                    false
                }
            }
            Err(_) => true,
        };
        if queued {
            continue;
        }

        PREFETCH_INFLIGHT.fetch_add(1, Ordering::Relaxed);
        let url = segment.clone();
        tauri::async_runtime::spawn(async move {
            prefetch_one(&url, generation).await;
            PREFETCH_INFLIGHT.fetch_sub(1, Ordering::Relaxed);
            if let Ok(mut active) = PREFETCH_ACTIVE.lock() {
                active.remove(&url);
            }
        });
    }
}

/// 首片预热：拿到播放列表后立刻排队下载开头若干片。
///
/// 与 [`schedule_prefetch`] 的区别：后者由「已经在下某一分片」触发（播放中追进度），
/// 前者在**播放还没开始**时就抢先动手，专治「点开视频后 3–5 秒黑屏」。
/// 只预热前 2 片：再往后就要靠播放中的预取接棒，避免一次性把带宽占满。
fn warmup_head(segments: &[String]) {
    if segments.is_empty() {
        return;
    }
    let generation = PREFETCH_GEN.fetch_add(1, Ordering::Relaxed) + 1;
    for segment in segments.iter().take(2) {
        if SEG_CACHE.lock().is_ok_and(|cache| cache.contains_key(segment)) {
            continue;
        }
        let queued = match PREFETCH_ACTIVE.lock() {
            Ok(mut active) => {
                if active.contains(segment) {
                    true
                } else {
                    if active.len() >= 64 {
                        active.clear();
                    }
                    active.insert(segment.clone());
                    false
                }
            }
            Err(_) => true,
        };
        if queued {
            continue;
        }
        PREFETCH_INFLIGHT.fetch_add(1, Ordering::Relaxed);
        let url = segment.clone();
        tauri::async_runtime::spawn(async move {
            prefetch_one(&url, generation).await;
            PREFETCH_INFLIGHT.fetch_sub(1, Ordering::Relaxed);
            if let Ok(mut active) = PREFETCH_ACTIVE.lock() {
                active.remove(&url);
            }
            // 首片下完把后继接上，保证起播后不会「缓冲一次又卡住」
            schedule_prefetch(&url);
        });
    }
}

/// 单个预取任务：失败静默（前台请求会自己重试，预取只是提前量）。
async fn prefetch_one(url: &str, generation: u64) {
    // 排到自己时已经换代（用户跳到别的位置了）：直接放弃，把额度让给新位置
    if PREFETCH_GEN.load(Ordering::Relaxed) != generation {
        return;
    }
    // 前台正在下载：预取让路。后台多占一路，前台就要多等一截。
    if FOREGROUND_ACTIVE.load(Ordering::Relaxed) > 0 {
        return;
    }
    if SEG_CACHE.lock().is_ok_and(|cache| cache.contains_key(url)) {
        return;
    }
    // 与前台共用同一把分片锁：前台正在下这一片时，预取直接放弃（前台优先级更高）
    let lock = seg_lock(url);
    let Ok(_guard) = lock.try_lock() else {
        return;
    };
    // 拿到锁后再确认一次代际：等锁期间用户可能已经跳走
    if PREFETCH_GEN.load(Ordering::Relaxed) != generation {
        return;
    }
    if SEG_CACHE.lock().is_ok_and(|cache| cache.contains_key(url)) {
        return;
    }
    let started = std::time::Instant::now();
    // 首片预热（generation 最新且是播放列表开头两片）走分段并发：
    // 它是用户「点开就播」的唯一等待路径，值得用满带宽。
    // 普通追进度预取仍走整包，避免后台把连接池和带宽摊薄。
    let is_head = PREFETCH_GEN.load(Ordering::Relaxed) == generation
        && SEG_OWNER
            .lock()
            .ok()
            .and_then(|owner| owner.get(url).map(|(_, index)| *index < 2))
            .unwrap_or(false);
    let fetched = if is_head {
        fetch_bytes_ranged(url, RANGED_PARTS).await
    } else {
        fetch_bytes(url).await
    };
    match fetched {
        Ok((ctype, bytes)) => {
            let len = bytes.len();
            store_segment(url, ctype, bytes);
            crate::playback_log::record(
                "prefetch.ok",
                serde_json::json!({
                    "resource": crate::playback_log::resource_id(url),
                    "bytes": len,
                    "elapsed_ms": started.elapsed().as_millis() as u64,
                }),
            );
        }
        Err(error) => crate::playback_log::record(
            "prefetch.fail",
            serde_json::json!({
                "resource": crate::playback_log::resource_id(url),
                "error_kind": crate::playback_log::error_kind(&error),
                "elapsed_ms": started.elapsed().as_millis() as u64,
            }),
        ),
    }
}

/// 清空封面 / 播放列表 / Key / 分片内存缓存
pub fn clear_caches() {
    if let Ok(mut g) = IMG_CACHE.lock() {
        g.clear();
    }
    if let Ok(mut g) = PL_CACHE.lock() {
        g.clear();
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
    if let Ok(mut g) = PREFETCH_ACTIVE.lock() {
        g.clear();
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

/* ───────────────────────── 协议响应 ───────────────────────── */

fn respond(status: u16, ctype: &str, body: Vec<u8>, cache: &str) -> Response<Vec<u8>> {
    let len = body.len().to_string();
    Response::builder()
        .status(StatusCode::from_u16(status).unwrap_or(StatusCode::OK))
        .header("Content-Type", ctype)
        // WKWebView 需要 Content-Length / Accept-Ranges 才肯复用已缓冲的分片
        .header("Content-Length", len)
        .header("Accept-Ranges", "bytes")
        .header("Access-Control-Expose-Headers", "Content-Length, Content-Type, Accept-Ranges")
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
                // 内层子清单同样登记分片序列：预取对多级 m3u8 也要能定位后继分片
                let (rewritten, segments) = rewrite_playlist(target, text, origin);
                register_playlist(target, &segments);
                respond(
                    200,
                    "application/vnd.apple.mpegurl",
                    rewritten.into_bytes(),
                    // Rewriting nested URIs does not require disabling the upstream cache policy.
                    "public, max-age=3600",
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

    /// 离线回归：不再依赖带 auth_key 时效的真实上游地址（旧地址已过期，会 403）。
    /// 与真实播放列表同构：带签名的分片地址 + 加密 KEY + 相对路径分片。
    #[test]
    fn playlist_rewrite_works() {
        let base = "https://cdn.example/videos5/abc/abc.m3u8?auth_key=9999999999-0-0-deadbeef&v=3&via=hgdj";
        let text = "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin?auth_key=9999999999-0-0-deadbeef\"\n#EXTINF:4,\nseg-1.ts?auth_key=9999999999-0-0-deadbeef\n";
        let (body, segments) = rewrite_playlist(base, text, LOCAL_ORIGIN);
        assert_eq!(segments.len(), 1, "分片序列应被收集供预取使用");
        assert!(
            segments[0].starts_with("https://cdn.example/videos5/abc/seg-1.ts"),
            "收集到的分片必须是绝对地址：{}",
            segments[0]
        );

        assert!(body.contains("#EXTM3U"), "not a playlist: {body}");
        assert!(
            body.contains(&format!("{LOCAL_ORIGIN}/seg?u=")),
            "segments not rewritten: {body}"
        );
        // 分片必须绝对化到源站，并保留签名查询串。
        assert!(
            body.contains("https%3A%2F%2Fcdn%2Eexample%2Fvideos5%2Fabc%2Fseg%2D1%2Ets%3Fauth%5Fkey%3D"),
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
