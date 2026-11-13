//! 网络层：与上游站点对话。
//!
//! 站点把全部 `/api/**` 调用收敛成 `POST /api/edge/request` 的信封，
//! 我们只实现这一个入口，其余接口都是「填 path」。
//!
//! # 代理（用户反馈「浏览器能开、HGhub 打不开」的根因就在这里）
//!
//! 客户端的流量**全部**走本文件的 reqwest，跟 WebView2 无关，所以它既不共享
//! 浏览器的代理设置，也不共享浏览器的证书库：
//!
//! * reqwest 只有开了 `system-proxy` feature 才会读 Windows 系统代理
//!   （`HKCU\...\Internet Settings` 的 `ProxyEnable`/`ProxyServer`/`ProxyOverride`）。
//!   本 crate 用 `default-features = false`，早先漏了这个 feature ⇒ 永远直连，
//!   只有 TUN/虚拟网卡这种网络层接管才「看起来能用」。
//! * 即使开了 `system-proxy`，PAC（`AutoConfigURL`）也不被支持，只认固定代理。
//! * Android 没有注册表可读，系统「Wi-Fi 代理」同样不被 reqwest 采用；
//!   安卓上只有 VPN/TUN 模式（VpnService 抓全流量）才有效。
//!
//! 排查手段：环境变量 `HGHUB_PROXY=http://127.0.0.1:7897` 可强制指定代理，
//! `net_diag` 命令会输出 DNS/HTTP/环境变量快照。

use crate::network_policy;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use once_cell::sync::Lazy;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

/// 默认上游（固定线路 1）。
pub const DEFAULT_BASE: &str = "https://huangguoai.com";

/// 当前上游基址（不带结尾斜杠）。可在设置里切到固定线路 2 或镜像线路，运行时立即生效。
static BASE: Lazy<RwLock<String>> = Lazy::new(|| RwLock::new(DEFAULT_BASE.to_string()));
/// 线路配置落盘位置（应用数据目录/line.json），由 [`bind_base`] 在启动时绑定。
static BASE_FILE: Lazy<RwLock<Option<PathBuf>>> = Lazy::new(|| RwLock::new(None));

/// 当前上游基址。
pub fn base() -> String {
    BASE.read()
        .map(|b| b.clone())
        .unwrap_or_else(|_| DEFAULT_BASE.to_string())
}

/// 线路必须是纯 HTTPS origin，不允许路径、认证、查询串或片段。
pub fn normalize_base(raw: &str) -> Option<String> {
    let parsed = network_policy::parse_target(raw.trim()).ok()?;
    if parsed.scheme() != "https"
        || parsed.path() != "/"
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return None;
    }
    Some(parsed.origin().ascii_serialization())
}

/// 哪些 origin 允许携带凭据：两个固定入口，加上发布页推荐过、且当前可能作为线路的镜像。
///
/// 国内网络连不上 `.com`，只能用镜像；镜像和主站是同一后端、同一个账号体系。
/// 只认发布页真的给过的地址（见 [`crate::lines::is_trusted_mirror`]），
/// 用户手填的任意域名依旧会被严格模式拦下。
pub fn trusted_auth_origin(origin: &str) -> bool {
    matches!(origin, "https://huangguoai.com" | "https://huangguoai.ai")
        || crate::lines::is_trusted_mirror(origin)
}

/// 启动时读回上次选择的线路（文件不存在 / 内容坏了就保持默认）。
pub fn bind_base(dir: &std::path::Path) {
    network_policy::bind_security(dir);
    let path = dir.join("line.json");
    if let Ok(text) = std::fs::read_to_string(&path) {
        if let Ok(v) = serde_json::from_str::<Value>(&text) {
            if let Some(next) = v
                .get("base")
                .and_then(|x| x.as_str())
                .and_then(normalize_base)
            {
                crate::lines::remember_line(&next);
                if let Ok(mut cur) = BASE.write() {
                    *cur = next;
                }
            }
        }
    }
    if let Ok(mut f) = BASE_FILE.write() {
        *f = Some(path);
    }
}

/// 切换线路并落盘，返回规整后的地址。
pub fn set_base(url: &str) -> Result<String, String> {
    let next = normalize_base(url)
        .ok_or_else(|| "线路地址无效（需要纯 HTTPS 站点地址，禁止内网）".to_string())?;
    let previous = base();
    crate::lines::remember_line(&next);
    if let Ok(mut cur) = BASE.write() {
        *cur = next.clone();
    }
    if let Some(path) = BASE_FILE.read().ok().and_then(|p| p.clone()) {
        let _ = std::fs::write(path, json!({ "base": next }).to_string());
    }
    if previous != next {
        crate::playback_log::record(
            "line.changed",
            json!({
                "previous_host": url::Url::parse(&previous).ok().and_then(|u| u.host_str().map(str::to_owned)),
                "line_host": url::Url::parse(&next).ok().and_then(|u| u.host_str().map(str::to_owned)),
            }),
        );
    }
    Ok(next)
}

pub const UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

/// 手动指定代理（排查 / 公司内网用）：`HGHUB_PROXY=http://127.0.0.1:7897`。
/// 不设时交给 reqwest 的 `system-proxy` 探测（注册表 + 环境变量）。
fn proxy_override() -> Option<String> {
    std::env::var("HGHUB_PROXY")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 按**当前**系统代理设置新建一个客户端。
///
/// 注意：reqwest 的 `system-proxy` 只在构建时读一次注册表 / 环境变量。代理软件被关掉后，
/// 旧客户端仍会往 `127.0.0.1:7897` 建隧道，症状是满屏 `tunnel error: failed to create
/// underlying connection`（浏览器却是通的，因为它每次都重读系统代理）。所以这里允许重建。
fn build_client(host: &str, addrs: &[SocketAddr]) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder()
        .user_agent(UA)
        .timeout(Duration::from_secs(20))
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .referer(false)
        .gzip(true)
        .brotli(true)
        .pool_idle_timeout(Duration::from_secs(60));
    if !addrs.is_empty() {
        builder = builder.resolve_to_addrs(host, addrs);
    }
    if let Some(p) = proxy_override() {
        // 错误信息不能含原始代理 URL（其中可能有用户名/密码）。
        let proxy = reqwest::Proxy::all(&p).map_err(|_| "HGHUB_PROXY 配置无效（已隐藏原始值）")?;
        builder = builder.proxy(proxy);
    }
    builder
        .build()
        .map_err(|_| "无法创建 HTTP 客户端".to_string())
}

/// DNS 复检间隔。到点后由后台任务重新解析并按结果决定是否重建客户端；
/// 客户端与连接池本身长存，不再像旧版那样每 30 秒整体重建
/// （实测温热连接池能为 8 路分段下载省 ~1.3s 的握手+慢启动）。
const CLIENT_TTL: Duration = Duration::from_secs(30);

/// 温热客户端条目：固定构建时通过策略校验的 DNS 地址，连接池长存。
struct ClientEntry {
    http: Arc<reqwest::Client>,
    /// 构建客户端时用的主机名（复检后重建客户端需要）
    host: String,
    /// 构建时通过策略校验并固定的地址（非严格模式为空）
    addrs: Vec<SocketAddr>,
    /// 上次成功校验（构建或复检答案一致）的时刻
    #[allow(dead_code)]
    validated_at: Instant,
    /// 下次允许复检的时刻
    next_check_at: Instant,
    /// 后台复检任务进行中（防止同一主机重复排队）
    revalidating: bool,
}
type ClientCache = HashMap<String, ClientEntry>;
static CLIENTS: Lazy<Mutex<ClientCache>> = Lazy::new(|| Mutex::new(HashMap::new()));
static MEDIA_DNS_ROUTES: Lazy<Mutex<crate::media_dns::Routes>> =
    Lazy::new(|| Mutex::new(crate::media_dns::Routes::default()));

/// 复检时解析地址（与首次构建同一套校验路径）。
/// 非严格模式没有固定地址可复检，返回空列表。
async fn resolve_addrs(
    target: &url::Url,
    public_dns: bool,
    strict: bool,
) -> Result<(Vec<SocketAddr>, Duration), String> {
    if public_dns {
        let resolver = build_client(crate::media_dns::HOST, &crate::media_dns::BOOTSTRAP)?;
        let host = target.host_str().ok_or("目标缺少主机名")?;
        let port = target.port_or_known_default().ok_or("目标端口无效")?;
        let resolution = crate::media_dns::resolve(&resolver, host, port).await?;
        network_policy::check_addresses_with_mode(&resolution.addresses, strict)?;
        Ok((resolution.addresses, CLIENT_TTL.min(resolution.ttl)))
    } else if strict {
        Ok((
            network_policy::resolve_target(target, strict).await?,
            CLIENT_TTL,
        ))
    } else {
        Ok((Vec::new(), CLIENT_TTL))
    }
}

/// 地址序列是否等价（DNS 答案顺序不稳定，排序去重后比较）。
fn same_addresses(a: &[SocketAddr], b: &[SocketAddr]) -> bool {
    let mut left: Vec<SocketAddr> = a.to_vec();
    let mut right: Vec<SocketAddr> = b.to_vec();
    left.sort();
    right.sort();
    left == right
}

/// 复检结果落地。返回 true 表示客户端被重建（连接池随之更换）。
///
/// 安全语义：只有复检产出且通过策略校验的新地址才会替换旧固定地址；
/// 复检失败时保留旧客户端——它的地址仍是构建时校验过的，不跟随未知答案。
#[cfg_attr(not(test), allow(dead_code))]
fn apply_revalidation(
    entry: &mut ClientEntry,
    result: Result<(Vec<SocketAddr>, Duration), String>,
    now: Instant,
) -> bool {
    entry.revalidating = false;
    match result {
        Ok((addrs, ttl)) => {
            if same_addresses(&addrs, &entry.addrs) {
                entry.next_check_at = now + ttl;
                false
            } else {
                match build_client(&entry.host, &addrs) {
                    Ok(http) => {
                        entry.http = Arc::new(http);
                        entry.addrs = addrs;
                        entry.next_check_at = now + ttl;
                        true
                    }
                    Err(_) => {
                        entry.next_check_at = now + CLIENT_TTL;
                        false
                    }
                }
            }
        }
        Err(_) => {
            entry.next_check_at = now + CLIENT_TTL;
            false
        }
    }
}

/// 到复检点后由后台任务重新解析；期间请求继续使用旧客户端（地址是构建时校验过的）。
fn spawn_revalidation(key: String, target: url::Url, public_dns: bool, strict: bool) {
    tauri::async_runtime::spawn(async move {
        let result = resolve_addrs(&target, public_dns, strict).await;
        let now = Instant::now();
        if let Ok(mut cache) = CLIENTS.lock() {
            // 复检期间条目可能被 refresh_client() 清掉；没了就直接丢弃结果
            if let Some(entry) = cache.get_mut(&key) {
                apply_revalidation(entry, result, now);
            }
        }
    });
}

/// 缓存按模式、主机和端口隔离；客户端固定已通过策略校验的地址。
/// 到复检点后由后台任务重新解析：
/// - 答案与已固定地址一致 → 保留客户端与连接池（省 8 条 TLS 连接的握手+慢启动）
/// - 出现新的合法答案 → 重建客户端并更换固定地址；旧连接池随最后一个引用释放
/// - 复检失败（含解析出被策略拒绝的地址）→ 保留旧客户端，按 30s 节奏重试
/// 系统代理变化不再依赖周期重建，沿用错误触发的 [`heal_client`] 重建路径。
async fn client_for(raw: &str) -> Result<(url::Url, Arc<reqwest::Client>), String> {
    client_for_dns(raw, false).await
}

async fn client_for_dns(
    raw: &str,
    public_dns: bool,
) -> Result<(url::Url, Arc<reqwest::Client>), String> {
    let strict = network_policy::strict_security();
    let target = network_policy::parse_target_with_mode(raw, strict)?;
    let host = target.host_str().ok_or("目标缺少主机名")?;
    let port = target.port_or_known_default().ok_or("目标端口无效")?;
    let key = format!("{strict}:{host}:{port}:{public_dns}");
    {
        let mut cache = CLIENTS.lock().map_err(|_| "HTTP 客户端锁失败")?;
        if let Some(entry) = cache.get_mut(&key) {
            let now = Instant::now();
            if now >= entry.next_check_at && !entry.revalidating {
                if !strict && entry.addrs.is_empty() {
                    // 非严格模式没有固定地址可复检：只推后复检点
                    entry.next_check_at = now + CLIENT_TTL;
                } else {
                    entry.revalidating = true;
                    spawn_revalidation(key.clone(), target.clone(), public_dns, strict);
                }
            }
            let http = entry.http.clone();
            return Ok((target, http));
        }
    }
    let (addrs, ttl) = resolve_addrs(&target, public_dns, strict).await?;
    let http = Arc::new(build_client(host, &addrs)?);
    let now = Instant::now();
    let mut cache = CLIENTS.lock().map_err(|_| "HTTP 客户端锁失败")?;
    // 同时到达的请求可能已完成解析；复用先建好的那条客户端与连接池。
    if let Some(existing) = cache.get(&key) {
        let http = existing.http.clone();
        return Ok((target, http));
    }
    if cache.len() >= 64 {
        cache.clear();
    }
    cache.insert(
        key,
        ClientEntry {
            http: http.clone(),
            host: host.to_string(),
            addrs,
            validated_at: now,
            next_check_at: now + ttl,
            revalidating: false,
        },
    );
    Ok((target, http))
}

pub fn refresh_client() {
    if let Ok(mut cache) = CLIENTS.lock() {
        cache.clear();
    }
    if let Ok(mut routes) = MEDIA_DNS_ROUTES.lock() {
        *routes = crate::media_dns::Routes::default();
    }
}

/// 不可信 GET 的统一入口；自动重定向被关闭，每一跳重新检查 URL、DNS 并固定解析地址。
pub async fn get(
    raw: &str,
    timeout: Duration,
    accept: Option<&str>,
    referer: Option<&str>,
) -> Result<reqwest::Response, String> {
    get_with_dns(raw, timeout, accept, referer, false).await
}

async fn get_with_dns(
    raw: &str,
    timeout: Duration,
    accept: Option<&str>,
    referer: Option<&str>,
    public_dns: bool,
) -> Result<reqwest::Response, String> {
    get_with_dns_range(raw, timeout, accept, referer, public_dns, None).await
}

/// 带可选 `Range` 的变体：分段下载大分片时用（见 [`fetch_bytes_ranged`]）。
async fn get_with_dns_range(
    raw: &str,
    timeout: Duration,
    accept: Option<&str>,
    referer: Option<&str>,
    public_dns: bool,
    range: Option<(u64, u64)>,
) -> Result<reqwest::Response, String> {
    tokio::time::timeout(timeout, async {
        let policy_error = |phase: &str, error: String| {
            crate::playback_log::record("network.target_failed", json!({"resource": crate::playback_log::resource_id(raw), "phase": phase, "error_kind": crate::playback_log::error_kind(&error)}));
            error
        };
        let mut next = network_policy::parse_target(raw).map_err(|e| policy_error("parse", e))?;
        for hop in 0..=5 {
            let resolving = Instant::now();
            let (target, http) = client_for_dns(next.as_str(), public_dns).await.map_err(|e| policy_error("dns_or_client", e))?;
            crate::playback_log::record("network.target_ready", json!({"resource": crate::playback_log::resource_id(raw), "target_resource": crate::playback_log::resource_id(target.as_str()), "dns_or_client_ms": resolving.elapsed().as_millis() as u64, "hop": hop}));
            let mut request = http.get(target.clone()).timeout(timeout);
            if let Some((start, end)) = range {
                request = request
                    .header(reqwest::header::RANGE, format!("bytes={start}-{end}"))
                    .header(reqwest::header::ACCEPT_ENCODING, "identity");
            }
            if let Some(value) = accept {
                request = request.header("Accept", value);
            }
            if let Some(value) = referer {
                request = request.header("Referer", value);
            }
            let response = request.send().await.map_err(|e| err_text(&e))?;
            if !matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
                return Ok(response);
            }
            if hop == 5 {
                return Err("重定向次数超过限制".into());
            }
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|h| h.to_str().ok())
                .ok_or("重定向缺少有效 Location")?;
            next = network_policy::redirect_target(&target, location).map_err(|e| policy_error("redirect", e))?;
            crate::playback_log::record("network.redirect", json!({"resource": crate::playback_log::resource_id(raw), "from": crate::playback_log::resource_id(target.as_str()), "to": crate::playback_log::resource_id(next.as_str()), "status": response.status().as_u16(), "hop": hop + 1}));
        }
        Err("重定向次数超过限制".into())
    })
    .await
    .map_err(|_| "请求超时".to_string())?
}

/// 只返回代理的协议/主机/端口；解析失败时隐藏整个值，绝不回传认证或查询参数。
pub fn redact_proxy(raw: &str) -> String {
    let parsed = url::Url::parse(raw.trim())
        .ok()
        .filter(|u| u.host_str().is_some());
    match parsed {
        Some(url) => {
            let mut safe = format!("{}://{}", url.scheme(), url.host_str().unwrap());
            if let Some(port) = url.port() {
                safe.push_str(&format!(":{port}"));
            }
            safe
        }
        None => "[已配置，原始值已隐藏]".into(),
    }
}

/// 看起来像代理 / 隧道问题（而不是目标站点本身不可达）。
pub fn is_proxy_error(e: &str) -> bool {
    let e = e.to_ascii_lowercase();
    e.contains("tunnel") || e.contains("proxy")
}

static LAST_HEAL: Lazy<std::sync::Mutex<Option<Instant>>> =
    Lazy::new(|| std::sync::Mutex::new(None));

/// 请求失败后按需重建客户端：代理隧道错误、连接被拒 / 超时都算「网络环境可能变了」。
/// 3 秒内只重建一次，批量探测不会反复重建。
pub fn heal_client(reason: &str) {
    let connectish = reason.contains("Connect")
        || reason.contains("timed out")
        || reason.contains("refused")
        || reason.contains("dns");
    if !is_proxy_error(reason) && !connectish {
        return;
    }
    let mut last = match LAST_HEAL.lock() {
        Ok(l) => l,
        Err(_) => return,
    };
    if let Some(t) = *last {
        if t.elapsed() < Duration::from_secs(3) {
            return;
        }
    }
    *last = Some(Instant::now());
    drop(last);
    refresh_client();
}

/// reqwest 的 `Display` 只有「error sending request for url (...)」，
/// 真正的原因（dns error / connection refused / invalid peer certificate）
/// 藏在 `source()` 链里。展开成一行，界面和 debug.log 才有排查价值。
pub fn err_text(e: &reqwest::Error) -> String {
    let mut out = e.to_string();
    let mut src = std::error::Error::source(e);
    while let Some(s) = src {
        let t = s.to_string();
        if !t.is_empty() && !out.contains(&t) {
            out.push_str(" ← ");
            out.push_str(&t);
        }
        src = s.source();
    }
    out
}

#[cfg(test)]
fn dns_summary(addrs: impl IntoIterator<Item = SocketAddr>) -> Value {
    let mut ips: Vec<_> = addrs.into_iter().map(|addr| addr.ip()).collect();
    ips.sort_unstable();
    ips.dedup();
    let has_fake_ip = ips.iter().any(|ip| {
        let v4 = match ip {
            std::net::IpAddr::V4(v4) => Some(*v4),
            std::net::IpAddr::V6(v6) => v6.to_ipv4_mapped(),
        };
        v4.is_some_and(|ip| {
            let [a, b, _, _] = ip.octets();
            a == 198 && (b == 18 || b == 19)
        })
    });
    json!({ "count": ips.len(), "has_fake_ip": has_fake_ip })
}

/// IPv4 地址分类。诊断要能回答「解析到的是正常公网，还是被投毒/被接管」。
fn ipv4_kind(o: [u8; 4]) -> &'static str {
    let v4 = std::net::Ipv4Addr::from(o);
    if v4.is_loopback() {
        "loopback"
    } else if o == [0, 0, 0, 0] {
        "unspecified"
    } else if o[0] == 198 && (o[1] == 18 || o[1] == 19) {
        // Clash / Surge 系 TUN 的 fake-ip 网段：TUN 一关这个地址就不可路由。
        "fake_ip"
    } else if v4.is_private() {
        "private"
    } else if v4.is_link_local() {
        "link_local"
    } else if v4.is_broadcast() {
        "broadcast"
    } else {
        "public"
    }
}

fn ip_kind(ip: std::net::IpAddr) -> &'static str {
    match ip {
        std::net::IpAddr::V4(v4) => ipv4_kind(v4.octets()),
        std::net::IpAddr::V6(v6) => {
            if v6.is_loopback() {
                "loopback"
            } else if v6.is_unspecified() {
                "unspecified"
            } else if let Some(v4) = v6.to_ipv4_mapped() {
                ipv4_kind(v4.octets())
            } else {
                "public"
            }
        }
    }
}

/// DNS 报告：数量 / 耗时 / 地址种类 / 是否 fake-ip。
///
/// **只把可疑地址原样列出**（回环、内网、保留段、fake-ip），公网答案只给数量——
/// 贴到公开 issue 里也不必泄露完整解析结果，但「被投毒到 127.0.0.1」这类
/// 一眼就能看见。
fn dns_report(addrs: &[SocketAddr], ms: u128) -> Value {
    let mut ips: Vec<std::net::IpAddr> = addrs.iter().map(|a| a.ip()).collect();
    ips.sort_unstable();
    ips.dedup();
    let mut kinds: Vec<&str> = ips.iter().map(|ip| ip_kind(*ip)).collect();
    kinds.sort_unstable();
    kinds.dedup();
    let suspicious: Vec<String> = ips
        .iter()
        .filter(|ip| ip_kind(**ip) != "public")
        .map(|ip| ip.to_string())
        .collect();
    json!({
        "ms": ms as u64,
        "count": ips.len(),
        "has_fake_ip": kinds.contains(&"fake_ip"),
        "kinds": kinds,
        "suspicious": suspicious,
    })
}

/// 拿到解析结果后直连一次，用来区分「DNS 能解析但 SYN 被黑洞」和「HTTP 层出错」。
async fn tcp_probe(addr: Option<SocketAddr>, timeout: Duration) -> Value {
    let Some(addr) = addr else {
        return json!({ "skipped": true, "reason": "没有可用的解析结果" });
    };
    let started = Instant::now();
    match tokio::time::timeout(timeout, tokio::net::TcpStream::connect(addr)).await {
        Ok(Ok(_)) => json!({
            "addr": addr.to_string(),
            "ok": true,
            "ms": started.elapsed().as_millis() as u64,
        }),
        Ok(Err(e)) => json!({
            "addr": addr.to_string(),
            "ok": false,
            "ms": started.elapsed().as_millis() as u64,
            "error": e.to_string(),
        }),
        Err(_) => json!({
            "addr": addr.to_string(),
            "ok": false,
            "timeout": true,
            "ms": timeout.as_millis() as u64,
            "error": "连接超时（SYN 无响应）",
        }),
    }
}

/// 把三组探测结果压成一句人话结论 + 可执行的建议，issue 里一眼能看懂。
fn verdict(dns: &Value, tcp: &Value, http: &Value) -> (&'static str, String, Vec<String>) {
    let dns_err = dns.get("error").and_then(|v| v.as_str());
    let fake = dns
        .get("has_fake_ip")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let kinds = dns
        .get("kinds")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let suspicious = dns
        .get("suspicious")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let status = http.get("status").and_then(|v| v.as_u64());
    let http_err = http.get("error").and_then(|v| v.as_str()).unwrap_or("");
    let tcp_ok = tcp.get("ok").and_then(|v| v.as_bool());

    // 系统 resolver 报错、但真实请求能通：诊断别把人带偏。
    if dns_err.is_some() && status.is_some() {
        return (
            "dns_false_alarm",
            "DNS 探测自身失败，但 HTTP 实际可以连通（系统 resolver 异常或被代理接管），不影响使用"
                .into(),
            vec![],
        );
    }
    if let Some(e) = dns_err {
        return (
            "dns_fail",
            format!("域名解析失败：{e}"),
            vec![
                "关掉代理 / 加速器 / VPN 的 TUN 模式后重试（最常见的元凶）".into(),
                "换网络对比：Wi-Fi 与流量互切".into(),
                "安卓「私有 DNS」改为自动，或填 223.5.5.5 / 1.1.1.1".into(),
            ],
        );
    }
    if fake {
        // 代理正常工作时 HTTP 也是通的，这种情况不能报故障，否则每个开 Clash 的用户都会被吓一跳。
        let http_ok = status.is_some_and(|c| (200..400).contains(&c));
        if http_ok {
            return (
                "ok_proxy",
                "DNS 被代理/TUN 接管（fake-ip），但连接正常，可继续使用".into(),
                vec!["若要排查网络问题，请先退出代理再诊断一次".into()],
            );
        }
        return (
            "fake_ip",
            format!(
                "解析到 fake-ip（{}），DNS 流量被代理/TUN 接管；代理一断开这个地址不可路由",
                suspicious
                    .first()
                    .and_then(|v| v.as_str())
                    .unwrap_or("198.18.x.x")
            ),
            vec![
                "关掉 Clash / Surge 等代理的 TUN（增强）模式，或直接退出代理".into(),
                "若必须用代理，请保证代理常驻且节点稳定后再开 App".into(),
            ],
        );
    }
    if kinds
        .iter()
        .any(|k| matches!(k.as_str(), Some("loopback" | "unspecified" | "private")))
    {
        return (
            "dns_hijack",
            "解析结果落在回环 / 内网 / 保留地址，DNS 被劫持或线路被污染".into(),
            vec![
                "换一个 DNS（223.5.5.5、1.1.1.1）后重启 App".into(),
                "关闭省电模式或系统的「网络加速」类功能".into(),
            ],
        );
    }
    if tcp_ok == Some(false) {
        let timeout = tcp
            .get("timeout")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        return (
            if timeout {
                "tcp_timeout"
            } else {
                "tcp_refused"
            },
            if timeout {
                "DNS 正常，但 TCP 连接无响应（SYN 被丢弃）：运营商或防火墙阻断".to_string()
            } else {
                format!(
                    "DNS 正常，但 TCP 连接被拒绝：{}",
                    tcp.get("error")
                        .and_then(|v| v.as_str())
                        .unwrap_or("未知原因")
                )
            },
            vec![
                "换网络（尤其换运营商 / 关掉网络加速器）后重试".into(),
                "确认没有防火墙或家长控制拦截该域名".into(),
            ],
        );
    }
    if !http_err.is_empty() {
        let lower = http_err.to_lowercase();
        let tls = lower.contains("certificate")
            || lower.contains("tls")
            || lower.contains("ssl")
            || lower.contains("handshake");
        return (
            if tls { "tls_intercept" } else { "http_fail" },
            if tls {
                "TLS 握手/证书校验失败：多半是代理做了中间人，或系统时间不对".to_string()
            } else {
                format!("HTTP 请求失败：{http_err}")
            },
            vec![
                "关闭代理的「HTTPS 解密 / 中间人」功能".into(),
                "检查系统时间是否准确".into(),
            ],
        );
    }
    match status {
        Some(code) if (200..400).contains(&code) => {
            ("ok", format!("连通正常（HTTP {code}），线路可用"), vec![])
        }
        Some(code) => (
            "http_status",
            format!("域名与网络都通，但服务端返回 HTTP {code}"),
            vec!["稍后重试，或换一条线路".into()],
        ),
        None => ("unknown", "诊断信息不足".into(), vec![]),
    }
}

/// 网络自检：DNS / TCP / HTTP / 代理环境变量，用于远程定位「打不开」。
///
/// 结论直接给在 `verdict` / `summary` / `advice` 里，用户复制出去就能看懂。
pub async fn diagnose() -> Value {
    let cur = base();
    let host = url::Url::parse(&cur)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
        .unwrap_or_default();

    // DNS：必须和真实请求走同一条解析路径（tokio lookup_host），
    // 否则会出现「诊断报解析失败、实际却打得开」的假阴性。
    let dns_started = Instant::now();
    let mut first_addr: Option<SocketAddr> = None;
    let dns = if host.is_empty() {
        json!({ "error": "当前线路没有主机名" })
    } else {
        match tokio::time::timeout(
            Duration::from_secs(6),
            tokio::net::lookup_host((host.as_str(), 443u16)),
        )
        .await
        {
            Ok(Ok(iter)) => {
                let addrs: Vec<SocketAddr> = iter.collect();
                let ms = dns_started.elapsed().as_millis();
                first_addr = addrs.first().copied();
                dns_report(&addrs, ms)
            }
            Ok(Err(e)) => json!({
                "error": "DNS 解析失败",
                "detail": e.to_string(),
                "ms": dns_started.elapsed().as_millis() as u64,
            }),
            Err(_) => json!({
                "error": "DNS 解析超时",
                "timeout": true,
                "ms": 6000,
            }),
        }
    };

    // TCP：拿到地址就直连一次，把「DNS 通但连不上」和「HTTP 层出错」分开。
    let tcp = tcp_probe(first_addr, Duration::from_secs(5)).await;

    let started = Instant::now();
    let http = match get(&format!("{cur}/"), Duration::from_secs(20), None, None).await {
        Ok(r) => json!({
            "status": r.status().as_u16(),
            "ms": started.elapsed().as_millis() as u64,
        }),
        Err(e) => json!({
            "kind": crate::playback_log::error_kind(&e),
            "error": e,
            "ms": started.elapsed().as_millis() as u64,
        }),
    };

    let (code, summary, advice) = verdict(&dns, &tcp, &http);

    json!({
        "verdict": code,
        "summary": summary,
        "advice": advice,
        "base": cur,
        "host": host,
        "dns": dns,
        "tcp": tcp,
        "http": http,
        "env": {
            "HTTP_PROXY": std::env::var("HTTP_PROXY").ok().map(|p| redact_proxy(&p)),
            "HTTPS_PROXY": std::env::var("HTTPS_PROXY").ok().map(|p| redact_proxy(&p)),
            "ALL_PROXY": std::env::var("ALL_PROXY").ok().map(|p| redact_proxy(&p)),
            "NO_PROXY": std::env::var("NO_PROXY").ok().map(|_| "[已配置，列表已隐藏]"),
            "HGHUB_PROXY": proxy_override().map(|p| redact_proxy(&p)),
        },
        "runtime": {
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "ver": env!("CARGO_PKG_VERSION"),
        },
    })
}

fn retryable_error(error: &str) -> bool {
    if error == "401" {
        return false;
    }
    if let Some(status) = error
        .strip_prefix("HTTP ")
        .and_then(|s| s.split_whitespace().next())
        .and_then(|s| s.parse::<u16>().ok())
    {
        return matches!(status, 408 | 425 | 429 | 500..=599);
    }
    !matches!(
        crate::playback_log::error_kind(error),
        "https_downgrade_denied"
            | "private_address_denied"
            | "target_policy_denied"
            | "redirect_error"
    )
}

/// 只重试可能恢复的错误；404/权限错误/安全拒绝直接结束，避免无效等待。
/// 站点偶发 TLS 抖动；碰上代理隧道错误就先重建客户端再试
/// （代理软件开关后旧客户端会一直卡在 tunnel error 上）。
async fn with_retry<F, Fut, T>(retry: bool, mut f: F) -> Result<T, String>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, String>>,
{
    let mut last = String::from("network error");
    let attempts = if retry { 3 } else { 1 };
    for attempt in 0..attempts {
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                last = e;
                if !retryable_error(&last) {
                    return Err(last);
                }
                heal_client(&last);
                if attempt + 1 < attempts {
                    tokio::time::sleep(Duration::from_millis(350 * (attempt as u64 + 1))).await;
                }
            }
        }
    }
    Err(last)
}

// 只记录固定的操作类型，不写搜索词、账号、内容 ID 或查询参数。
fn edge_operation(path: &str) -> &'static str {
    let path = path.split('?').next().unwrap_or("");
    if path.starts_with("/api/videos/category/") {
        "category"
    } else if path.starts_with("/api/videos/") && path.ends_with("/play") {
        "play"
    } else if path == "/api/videos" {
        "feed"
    } else if path.starts_with("/api/videos/") {
        "video"
    } else if path == "/api/posts" {
        "posts"
    } else if path.starts_with("/api/posts/") {
        "post"
    } else if path.starts_with("/api/ranks/") {
        "rank"
    } else if path == "/api/search" {
        "search"
    } else if path.starts_with("/api/account/") {
        "account"
    } else {
        "other"
    }
}

/// 仅无请求体、无凭据的逻辑 GET 可以迁移网关；信封本身仍使用 POST。
fn edge_redirect_target(
    current: &url::Url,
    status: u16,
    location: &str,
    guest_get: bool,
) -> Result<url::Url, String> {
    if !guest_get || !matches!(status, 301 | 302 | 303) {
        return Err("业务接口重定向已拒绝".into());
    }
    network_policy::redirect_target(current, location)
}

/// 业务请求：走边缘信封。
pub async fn edge(
    origin: &str,
    method: &str,
    path: &str,
    body: Option<Value>,
    token: Option<&str>,
) -> Result<Value, String> {
    let cur = normalize_base(origin).ok_or("业务请求必须使用安全 HTTPS 线路")?;
    if network_policy::strict_security()
        && (token.is_some()
            || matches!(
                path,
                "/api/account/loginByPassword" | "/api/account/registerByPassword"
            ))
        && !trusted_auth_origin(&cur)
    {
        return Err("镜像线路仅供游客浏览；登录/注册请切换到固定线路".into());
    }
    let (encoded, ctype) = match &body {
        Some(v) => (
            B64.encode(serde_json::to_vec(v).map_err(|e| e.to_string())?),
            "application/json",
        ),
        None => (String::new(), ""),
    };
    let envelope = json!({
        "method": method,
        "path": path,
        "body": encoded,
        "content_type": ctype,
    });

    let url = format!("{cur}/api/edge/request");
    let referer = format!("{cur}/");
    let token = token.map(|s| s.to_string());

    static NEXT_EDGE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let request = NEXT_EDGE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let line_host = url::Url::parse(&cur)
        .unwrap()
        .host_str()
        .unwrap()
        .to_string();
    let operation = edge_operation(path);
    let mut attempt = 0;
    let guest_get = method.eq_ignore_ascii_case("GET") && body.is_none() && token.is_none();
    let can_retry = method.eq_ignore_ascii_case("GET") && body.is_none();
    let value = with_retry(can_retry, || {
        attempt += 1;
        let attempt = attempt;
        let envelope = envelope.clone();
        let token = token.clone();
        let url = url.clone();
        let referer = referer.clone();
        let line_host = line_host.clone();
        async move {
            let started = Instant::now();
            crate::playback_log::record("api.begin", json!({"request": request, "attempt": attempt, "operation": operation, "method": method, "line_host": line_host}));
            let mut status_code = None;
            let mut headers_ms = None;
            let mut target_host = line_host.clone();
            let mut response_kind = "unknown";
            let mut gateway = "other";
            let result = async {
                let mut next = network_policy::parse_target(&url)?;
                for hop in 0..=5 {
                    let (target, client) = client_for(next.as_str()).await?;
                    target_host = target.host_str().unwrap_or("").to_string();
                    let mut req = client
                        .post(target.clone())
                        .header("Accept", "application/json")
                        .header("Referer", &referer)
                        .json(&envelope);
                    if let Some(t) = token.as_deref() {
                        if !t.is_empty() {
                            req = req.header("Authorization", format!("Bearer {t}"));
                        }
                    }
                    let resp = req.send().await.map_err(|e| err_text(&e))?;
                    let status = resp.status();
                    status_code = Some(status.as_u16());
                    headers_ms = Some(started.elapsed().as_millis() as u64);
                    let ctype = resp.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
                    response_kind = if ctype.contains("json") { "json" } else if ctype.contains("html") { "html" } else { "other" };
                    gateway = if resp.headers().contains_key("x-amz-cf-id") { "cloudfront" } else if resp.headers().contains_key("cf-ray") { "cloudflare" } else { "other" };
                    // 逐跳重建并检查客户端；不自动把信封 POST 改成无信封 GET。
                    if status.is_redirection() {
                        if hop == 5 {
                            return Err("重定向次数超过限制".into());
                        }
                        let location = resp
                            .headers()
                            .get(reqwest::header::LOCATION)
                            .and_then(|h| h.to_str().ok())
                            .ok_or("重定向缺少有效 Location")?;
                        next = edge_redirect_target(&target, status.as_u16(), location, guest_get)?;
                        continue;
                    }
                    let text = resp.text().await.map_err(|e| err_text(&e))?;
                    if status.as_u16() == 401 || status.as_u16() == 403 {
                        return Err("401".to_string());
                    }
                    if !status.is_success() {
                        return Err(format!(
                            "HTTP {status}: {}",
                            text.chars().take(160).collect::<String>()
                        ));
                    }
                    if text.trim().is_empty() {
                        return Ok(Value::Null);
                    }
                    return serde_json::from_str::<Value>(&text)
                        .map_err(|e| format!("{e}: {}", text.chars().take(160).collect::<String>()));
                }
                Err("重定向次数超过限制".into())
            }.await;
            crate::playback_log::record("api.end", json!({
                "request": request, "attempt": attempt, "operation": operation, "method": method,
                "line_host": line_host, "target_host": target_host, "status": status_code,
                "headers_ms": headers_ms, "elapsed_ms": started.elapsed().as_millis() as u64,
                "response_kind": response_kind, "gateway": gateway,
                "business_status": result.as_ref().ok().and_then(|v: &Value| v.get("status")).and_then(Value::as_i64),
                "error_kind": result.as_ref().err().map(|e: &String| crate::playback_log::error_kind(e)),
                "retryable": can_retry && result.as_ref().err().is_some_and(|e| retryable_error(e)),
            }));
            result
        }
    })
    .await?;

    if value.is_null() {
        return Ok(Value::Null);
    }

    if value.get("status").and_then(|v| v.as_i64()) == Some(1) {
        Ok(value.get("data").cloned().unwrap_or(Value::Null))
    } else {
        Err(value
            .get("msg")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("接口返回失败")
            .to_string())
    }
}

/// 拉原始字节（图片 / m3u8 / 分片）。
pub async fn fetch_bytes(url: &str) -> Result<(Option<String>, Vec<u8>), String> {
    static NEXT_REQUEST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let request = NEXT_REQUEST.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let resource = crate::playback_log::resource_id(url);
    let mut attempt = 0;
    let url = url.to_string();
    let target = url::Url::parse(&url).ok();
    let host = target
        .as_ref()
        .and_then(|target| target.host_str())
        .unwrap_or("")
        .to_string();
    let public_dns = AtomicBool::new(
        MEDIA_DNS_ROUTES
            .lock()
            .ok()
            .is_some_and(|routes| routes.prefers_public(&host, Instant::now())),
    );
    let referer = format!("{}/", base());
    with_retry(true, || {
        attempt += 1;
        let attempt = attempt;
        let resource = resource.clone();
        let url = url.clone();
        let referer = referer.clone();
        let host = &host;
        let target = &target;
        let public_dns = &public_dns;
        async move {
            let using_public_dns = public_dns.load(Ordering::Relaxed);
            let started = Instant::now();
            crate::playback_log::record("network.begin", json!({"request": request, "resource": resource, "attempt": attempt}));
            let mut status = None;
            let mut headers_ms = None;
            let result = async {
                let resp = get_with_dns(&url, Duration::from_secs(20), None, Some(&referer), using_public_dns).await?;
                status = Some(resp.status().as_u16());
                headers_ms = Some(started.elapsed().as_millis() as u64);
                if !resp.status().is_success() {
                    return Err(format!("HTTP {}", resp.status()));
                }
                let ctype = resp
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string());
                let bytes = resp.bytes().await.map_err(|e| err_text(&e))?.to_vec();
                Ok((ctype, bytes))
            }.await;
            if using_public_dns {
                if let Ok(mut routes) = MEDIA_DNS_ROUTES.lock() {
                    if result.is_ok() {
                        routes.remember(host, Instant::now());
                    } else {
                        routes.forget(host);
                    }
                }
            }
            if let Err(error) = &result {
                if target.as_ref().is_some_and(|target| crate::media_dns::eligible(target, error)) {
                    public_dns.store(!using_public_dns, Ordering::Relaxed);
                    crate::playback_log::record("network.dns_fallback", json!({"resource": resource, "attempt": attempt, "public_dns": !using_public_dns, "error_kind": crate::playback_log::error_kind(error)}));
                }
            }
            crate::playback_log::record("network.end", json!({"request": request, "resource": resource, "attempt": attempt, "public_dns": using_public_dns, "status": status, "headers_ms": headers_ms, "elapsed_ms": started.elapsed().as_millis() as u64, "bytes": result.as_ref().ok().map(|(_, bytes)| bytes.len()), "error_kind": result.as_ref().err().map(|e: &String| crate::playback_log::error_kind(e))}));
            result
        }
    })
    .await
}

/// 首块既探测 Range 支持又保留有效数据；小对象只补剩余部分，不整包重下。
const RANGE_PROBE_BYTES: u64 = 64 * 1024;
const MIN_RANGED_BYTES: u64 = 1024 * 1024;
const MAX_RANGE_BYTES: u64 = 8 * 1024 * 1024;
/// 限制单个媒体分片（不是整个视频），防止不可信 total 或无长度响应耗尽内存。
const MAX_RANGED_OBJECT_BYTES: u64 = 64 * 1024 * 1024;
const RANGED_TIMEOUT: Duration = Duration::from_secs(20);

#[path = "net_download_metrics.rs"]
mod download_metrics;
use download_metrics::{DownloadSummaryGuard, DownloadTrace, StageTimer, TailPolicy};

type MediaBytes = (Option<String>, Vec<u8>);
type RangeTasks = tokio::task::JoinSet<Result<(u64, Vec<u8>, Duration), RangeFailure>>;

struct RangeFailure {
    message: String,
    fallback: bool,
}

impl RangeFailure {
    fn recoverable(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            fallback: true,
        }
    }

    fn terminal(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            fallback: false,
        }
    }
}

fn response_type(response: &reqwest::Response) -> Option<String> {
    response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

/// 只接受单个、完整、可校验的 bytes start-end/total；不接受通配 total 或多区间。
fn checked_content_range(response: &reqwest::Response) -> Result<(u64, u64, u64), RangeFailure> {
    let invalid = || RangeFailure::recoverable("分段 Content-Range 无效");
    let header_values = response.headers().get_all(reqwest::header::CONTENT_RANGE);
    let mut values = header_values.iter();
    let value = values
        .next()
        .and_then(|v| v.to_str().ok())
        .ok_or_else(invalid)?;
    if values.next().is_some() {
        return Err(invalid());
    }
    let (span, total) = value
        .strip_prefix("bytes ")
        .and_then(|v| v.split_once('/'))
        .ok_or_else(invalid)?;
    let (start, end) = span.split_once('-').ok_or_else(invalid)?;
    let decimal = |s: &str| {
        if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
            return Err(invalid());
        }
        s.parse::<u64>().map_err(|_| invalid())
    };
    let (start, end, total) = (decimal(start)?, decimal(end)?, decimal(total)?);
    if total > MAX_RANGED_OBJECT_BYTES {
        return Err(RangeFailure::terminal("媒体分片超过大小限制"));
    }
    if start > end || end >= total {
        return Err(invalid());
    }
    Ok((start, end, total))
}

/// 流式限长：声明长度和实际长度都校验，禁止先 bytes() 收入任意大正文再检查。
async fn read_media_body(
    response: reqwest::Response,
    expected: Option<u64>,
    limit: u64,
) -> Result<Vec<u8>, RangeFailure> {
    read_media_body_prefixed(response, expected, limit, Vec::new()).await
}

async fn read_media_body_prefixed(
    mut response: reqwest::Response,
    expected: Option<u64>,
    limit: u64,
    mut bytes: Vec<u8>,
) -> Result<Vec<u8>, RangeFailure> {
    // reqwest 的 content_length() 来自 body size_hint，读过前缀后可能只剩余量。
    // 已读取前缀时按原始 identity 响应头校验完整实体，不把余量错当完整长度。
    let declared = if bytes.is_empty() {
        response.content_length()
    } else {
        response
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
    };
    if declared.is_some_and(|len| len > limit) {
        return Err(RangeFailure::terminal("媒体响应超过大小限制"));
    }
    if let (Some(actual), Some(expected)) = (declared, expected) {
        if actual != expected {
            return Err(RangeFailure::recoverable("分段 Content-Length 不匹配"));
        }
    }
    let expected = expected.or(declared);
    if bytes.len() as u64 > limit || expected.is_some_and(|len| bytes.len() as u64 > len) {
        return Err(RangeFailure::recoverable("分段前缀长度不匹配"));
    }
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| RangeFailure::recoverable(err_text(&e)))?
    {
        if chunk.len() as u64 > limit.saturating_sub(bytes.len() as u64) {
            return Err(RangeFailure::terminal("媒体响应超过大小限制"));
        }
        if expected.is_some_and(|len| bytes.len() as u64 + chunk.len() as u64 > len) {
            return Err(RangeFailure::recoverable("分段实际长度不匹配"));
        }
        bytes.extend_from_slice(&chunk);
    }
    if expected.is_some_and(|len| bytes.len() as u64 != len) {
        return Err(RangeFailure::recoverable("媒体响应截断"));
    }
    Ok(bytes)
}

fn require_partial(response: &reqwest::Response) -> Result<(), RangeFailure> {
    match response.status().as_u16() {
        206 => Ok(()),
        // HTTP 拒绝/限流直接结束，不额外发整包请求放大 403/429。
        200 | 416 => Err(RangeFailure::recoverable("服务端未保持 Range 支持")),
        _ => Err(RangeFailure::terminal(format!(
            "HTTP {}",
            response.status()
        ))),
    }
}

async fn read_partial(
    response: reqwest::Response,
    start: u64,
    end: u64,
    total: u64,
) -> Result<Vec<u8>, RangeFailure> {
    require_partial(&response)?;
    if checked_content_range(&response)? != (start, end, total) {
        return Err(RangeFailure::recoverable("分段区间或总长度不匹配"));
    }
    read_media_body(response, Some(end - start + 1), MAX_RANGE_BYTES).await
}

/// 私有注入点只替换发请求动作；生产入口始终经过原有逐跳 URL/DNS/代理策略。
async fn ranged_attempt<F, Fut>(
    request: F,
    parts: usize,
    trace: Arc<DownloadTrace>,
    tail_policy: TailPolicy,
    tasks: &mut RangeTasks,
) -> Result<MediaBytes, RangeFailure>
where
    F: Fn(Option<(u64, u64)>) -> Fut + Clone + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<reqwest::Response, String>> + Send + 'static,
{
    trace.requests.fetch_add(1, Ordering::Relaxed);
    let headers_started = Instant::now();
    let mut probe = request(Some((0, RANGE_PROBE_BYTES - 1)))
        .await
        .map_err(RangeFailure::recoverable)?;
    trace.probe_headers_ms.store(
        headers_started.elapsed().as_millis() as u64,
        Ordering::Relaxed,
    );
    let ctype = response_type(&probe);
    let body_started = Instant::now();
    if probe.status().as_u16() == 200 {
        // 服务端忽略 Range 时原响应就是整包；只发送这一次请求。
        trace.concurrency.store(1, Ordering::Relaxed);
        let bytes = read_media_body(probe, None, MAX_RANGED_OBJECT_BYTES).await?;
        trace
            .probe_read_ms
            .store(body_started.elapsed().as_millis() as u64, Ordering::Relaxed);
        return Ok((ctype, bytes));
    }
    require_partial(&probe)?;
    let (start, end, total) = checked_content_range(&probe)?;
    if start != 0 || end != (RANGE_PROBE_BYTES - 1).min(total - 1) {
        return Err(RangeFailure::recoverable("首块区间不匹配"));
    }
    if end + 1 == total {
        trace.concurrency.store(1, Ordering::Relaxed);
        let bytes = read_media_body(probe, Some(total), RANGE_PROBE_BYTES).await?;
        trace
            .probe_read_ms
            .store(body_started.elapsed().as_millis() as u64, Ordering::Relaxed);
        return Ok((ctype, bytes));
    }

    // 在扇出前先检查声明长度，并确认少量真实正文可读；明显坏响应不放大请求数。
    if probe.content_length().is_some_and(|len| len != end + 1) {
        return Err(RangeFailure::recoverable("首块 Content-Length 不匹配"));
    }
    let mut prefix = Vec::new();
    while prefix.len() < 16 * 1024 {
        let chunk = probe
            .chunk()
            .await
            .map_err(|error| RangeFailure::recoverable(err_text(&error)))?
            .ok_or_else(|| RangeFailure::recoverable("首块响应截断"))?;
        if prefix.len() as u64 + chunk.len() as u64 > end + 1 {
            return Err(RangeFailure::recoverable("首块实际长度不匹配"));
        }
        prefix.extend_from_slice(&chunk);
    }
    let concurrency = if total < MIN_RANGED_BYTES {
        1
    } else {
        parts.clamp(2, 8)
    };
    trace.concurrency.store(concurrency, Ordering::Relaxed);
    let chunk = (total - end - 1)
        .div_ceil(concurrency as u64)
        .min(MAX_RANGE_BYTES);
    let size =
        usize::try_from(total).map_err(|_| RangeFailure::terminal("媒体分片超过地址空间"))?;
    let mut buffer = Vec::new();
    buffer
        .try_reserve_exact(size)
        .map_err(|_| RangeFailure::terminal("媒体分片内存分配失败"))?;
    buffer.resize(size, 0);
    let mut next = end + 1;
    // 头校验成功即可重叠读取首块与其余区间，不再串行等首块体读完。
    // 首块也占一个槽，总在途响应（含首块）仍受 concurrency 限制。
    // JoinSet 由相位 timeout 外层持有；超时后先显式取消并回收，再进入整包回退。
    // 父 future 取消时仍由 JoinSet Drop 终止子任务；任何片失败都不提交半成品。
    let mut pending: HashMap<u64, (u64, Instant)> = HashMap::new();
    let mut completed = Vec::with_capacity(concurrency);
    // 超大对象受 MAX_RANGE_BYTES 分批时保留原路径，不在超出 parts 的请求组投机救援。
    let rescue_budgeted =
        concurrency > 1 && (total - end - 1).div_ceil(chunk) <= concurrency as u64;
    pending.insert(0, (end, body_started));
    let probe_trace = trace.clone();
    tasks.spawn(async move {
        let _read = StageTimer::since(&probe_trace.probe_read_ms, body_started);
        let bytes =
            read_media_body_prefixed(probe, Some(end + 1), RANGE_PROBE_BYTES, prefix).await?;
        Ok::<_, RangeFailure>((0u64, bytes, body_started.elapsed()))
    });
    loop {
        while tasks.len() < concurrency && next < total {
            let start = next;
            let end = (start + chunk - 1).min(total - 1);
            next = end + 1;
            let request = request.clone();
            let trace = trace.clone();
            let started = Instant::now();
            pending.insert(start, (end, started));
            tasks.spawn(async move {
                trace.requests.fetch_add(1, Ordering::Relaxed);
                let response = request(Some((start, end)))
                    .await
                    .map_err(RangeFailure::recoverable)?;
                let bytes = read_partial(response, start, end, total).await?;
                trace.range_done(started);
                Ok::<_, RangeFailure>((start, bytes, started.elapsed()))
            });
        }
        let rescue = if rescue_budgeted && next == total && pending.len() == 1 && tasks.len() == 1 {
            pending.iter().next().and_then(|(&start, &(end, started))| {
                tail_policy
                    .threshold(end - start + 1, chunk, &completed)
                    .map(|threshold| (start, end, started, threshold))
            })
        } else {
            None
        };
        let joined = if let Some((start, end, started, threshold)) = rescue {
            tokio::select! {
                // 边界同时完成时优先收已成功结果，不为刚完成的尾块额外发请求。
                biased;
                result = tasks.join_next() => result,
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(started + threshold)) => {
                    // 这一标记在取消/回收前设置；即使此刻撞到外层20s截止，也不再整包回退。
                    trace.tail_rescue.store(1, Ordering::Relaxed);
                    trace.tail_threshold_ms.store(threshold.as_millis() as u64, Ordering::Relaxed);
                    trace.tail_wait_ms.store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
                    trace.range_done(started);
                    tasks.abort_all();
                    while tasks.join_next().await.is_some() {}
                    // 原尾 future（含其 response/prefix）已 drop。只重取同一完整区间，
                    // 其它已校验字节留在 buffer；救援本身沿用外层阶段剩余时间，没有新deadline。
                    trace.requests.fetch_add(1, Ordering::Relaxed);
                    let retry_started = Instant::now();
                    let _read = StageTimer::since(&trace.tail_read_ms, retry_started);
                    let response = request(Some((start, end))).await.map_err(RangeFailure::terminal)?;
                    let bytes = read_partial(response, start, end, total).await
                        .map_err(|error| RangeFailure::terminal(error.message))?;
                    trace.range_done(retry_started);
                    if start == 0 {
                        trace.probe_read_ms.store(body_started.elapsed().as_millis() as u64, Ordering::Relaxed);
                    }
                    let start = start as usize;
                    buffer[start..start + bytes.len()].copy_from_slice(&bytes);
                    return Ok((ctype, buffer));
                }
            }
        } else {
            tasks.join_next().await
        };
        let Some(result) = joined else {
            break;
        };
        let result = result
            .map_err(|_| RangeFailure::recoverable("分段任务中断"))
            .and_then(|result| result);
        match result {
            Ok((start, bytes, elapsed)) => {
                pending.remove(&start);
                if start != 0 {
                    completed.push((bytes.len() as u64, elapsed));
                }
                let start = start as usize;
                buffer[start..start + bytes.len()].copy_from_slice(&bytes);
            }
            // 保留原始终止/可恢复分类，由 timeout 外层统一取消并回收兄弟任务。
            Err(error) => return Err(error),
        }
    }
    Ok((ctype, buffer))
}

#[cfg(test)]
async fn fetch_bytes_ranged_with<F, Fut>(request: F, parts: usize) -> Result<MediaBytes, String>
where
    F: Fn(Option<(u64, u64)>) -> Fut + Clone + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<reqwest::Response, String>> + Send + 'static,
{
    fetch_bytes_ranged_with_trace(request, parts, Arc::new(DownloadTrace::new())).await
}

async fn fetch_bytes_ranged_with_trace<F, Fut>(
    request: F,
    parts: usize,
    trace: Arc<DownloadTrace>,
) -> Result<MediaBytes, String>
where
    F: Fn(Option<(u64, u64)>) -> Fut + Clone + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<reqwest::Response, String>> + Send + 'static,
{
    fetch_bytes_ranged_with_policy(request, parts, trace, TailPolicy::default()).await
}

async fn fetch_bytes_ranged_with_policy<F, Fut>(
    request: F,
    parts: usize,
    trace: Arc<DownloadTrace>,
    tail_policy: TailPolicy,
) -> Result<MediaBytes, String>
where
    F: Fn(Option<(u64, u64)>) -> Fut + Clone + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<reqwest::Response, String>> + Send + 'static,
{
    if parts == 1 {
        trace.requests.fetch_add(1, Ordering::Relaxed);
        trace.concurrency.store(1, Ordering::Relaxed);
        return tokio::time::timeout(RANGED_TIMEOUT, async {
            let started = Instant::now();
            let response = request(None).await?;
            trace
                .probe_headers_ms
                .store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
            if response.status().as_u16() != 200 {
                return Err(format!("HTTP {}", response.status().as_u16()));
            }
            let ctype = response_type(&response);
            let body_started = Instant::now();
            let bytes = read_media_body(response, None, MAX_RANGED_OBJECT_BYTES)
                .await
                .map_err(|error| error.message)?;
            trace
                .probe_read_ms
                .store(body_started.elapsed().as_millis() as u64, Ordering::Relaxed);
            Ok((ctype, bytes))
        })
        .await
        .map_err(|_| "整包下载超时".to_string())?;
    }
    let mut tasks = RangeTasks::new();
    let attempt = tokio::time::timeout(
        tail_policy.phase_budget,
        ranged_attempt(
            request.clone(),
            parts,
            trace.clone(),
            tail_policy,
            &mut tasks,
        ),
    )
    .await;
    let failure = match attempt {
        Ok(Ok(value)) => return Ok(value),
        Ok(Err(failure)) => failure,
        Err(_) => RangeFailure::recoverable("分段下载超时"),
    };
    tasks.abort_all();
    // child 错误和普通 timeout 共用清理屏障；旧 request/response future 全部 Drop 后才回退。
    // 保留错误分类，终止类403/429也先回收兄弟任务，但不转成可恢复timeout。
    while tasks.join_next().await.is_some() {}
    if !failure.fallback || trace.tail_rescue.load(Ordering::Relaxed) != 0 {
        // 单尾救援与整包回退共用一次恢复预算；救援错误/超时不再重新下载全片。
        return Err(failure.message);
    }
    // 最多回退一次；不调用具有三次重试的 fetch_bytes，整条路径最多两个 20s 窗口。
    trace.fallback.store(1, Ordering::Relaxed);
    tokio::time::timeout(RANGED_TIMEOUT, async {
        trace.requests.fetch_add(1, Ordering::Relaxed);
        let response = request(None).await?;
        if response.status().as_u16() != 200 {
            return Err(format!("HTTP {}", response.status()));
        }
        let ctype = response_type(&response);
        let bytes = read_media_body(response, None, MAX_RANGED_OBJECT_BYTES)
            .await
            .map_err(|error| error.message)?;
        Ok((ctype, bytes))
    })
    .await
    .map_err(|_| "整包回退超时".to_string())?
}

/// 有界分段下载：64KiB 首块复用、严格校验、取消联动，异常最多单次整包回退。
pub async fn fetch_bytes_ranged(
    url: &str,
    parts: usize,
) -> Result<(Option<String>, Vec<u8>), String> {
    let referer = format!("{}/", base());
    // 受控对比档位，不做额外测速或盲目探索；无显式配置时保留既有生产档。
    let configured = std::env::var("HGHUB_MEDIA_DOWNLOAD_PARTS").ok();
    let selected_parts = download_metrics::parse_parts(configured.as_deref(), parts);
    let trace = Arc::new(DownloadTrace::new());
    let mut summary = DownloadSummaryGuard::new(
        crate::playback_log::resource_id(url),
        selected_parts,
        trace.clone(),
    );
    let public_dns =
        Arc::new(AtomicBool::new(MEDIA_DNS_ROUTES.lock().ok().is_some_and(
            |routes| routes.prefers_public(host_of(url), Instant::now()),
        )));
    let route = public_dns.clone();
    let owned_url = url.to_string();
    let result = fetch_bytes_ranged_with_trace(
        move |range| {
            let url = owned_url.clone();
            let referer = referer.clone();
            let route = route.clone();
            async move {
                let using_public_dns = route.load(Ordering::Relaxed);
                let result = get_with_dns_range(
                    &url,
                    RANGED_TIMEOUT,
                    None,
                    Some(&referer),
                    using_public_dns,
                    range,
                )
                .await;
                if let Err(error) = &result {
                    // 保留原整包路径的代理自愈及备用 DNS；只改变既有单次回退的路由，
                    // 不在子任务内部重试，备用地址仍经过同一套 DNS 安全校验。
                    heal_client(error);
                    if url::Url::parse(&url)
                        .ok()
                        .is_some_and(|target| crate::media_dns::eligible(&target, error))
                    {
                        route.store(!using_public_dns, Ordering::Relaxed);
                    }
                }
                result
            }
        },
        selected_parts,
        trace.clone(),
    )
    .await;
    summary.finish(&result);
    if public_dns.load(Ordering::Relaxed) {
        if let Ok(mut routes) = MEDIA_DNS_ROUTES.lock() {
            if result.is_ok() {
                routes.remember(host_of(url), Instant::now());
            } else {
                routes.forget(host_of(url));
            }
        }
    }
    if let Ok((_, bytes)) = &result {
        crate::playback_log::record(
            "network.ranged",
            json!({
                "resource": crate::playback_log::resource_id(url),
                "parts": selected_parts, "concurrency": trace.concurrency.load(Ordering::Relaxed),
                "request_count": trace.requests.load(Ordering::Relaxed), "bytes": bytes.len(),
            }),
        );
    }
    result
}

#[cfg(test)]
#[path = "net_download_profile_tests.rs"]
mod download_profile_tests;
#[cfg(test)]
#[path = "net_ranged_tests.rs"]
mod ranged_tests;
#[cfg(test)]
#[path = "net_tail_rescue_tests.rs"]
mod tail_rescue_tests;

fn host_of(raw: &str) -> &str {
    raw.split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .and_then(|authority| authority.split(':').next())
        .unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    #[ignore = "联网测试：需要 HGHUB_TEST_MEDIA_URL 为当前有效的媒体地址"]
    async fn media_recovers_from_failed_system_dns() {
        let raw = std::env::var("HGHUB_TEST_MEDIA_URL").expect("HGHUB_TEST_MEDIA_URL");
        let target = url::Url::parse(&raw).unwrap();
        let host = target.host_str().unwrap();
        let port = target.port_or_known_default().unwrap();
        let failed_client = reqwest::Client::builder()
            .no_proxy()
            .resolve_to_addrs(host, &["127.0.0.1:9".parse().unwrap()])
            .connect_timeout(Duration::from_millis(100))
            .build()
            .unwrap();
        refresh_client();
        CLIENTS.lock().unwrap().insert(
            format!("{}:{host}:{port}:false", network_policy::strict_security()),
            ClientEntry {
                http: Arc::new(failed_client),
                host: host.to_string(),
                addrs: Vec::new(),
                validated_at: Instant::now(),
                next_check_at: Instant::now() + CLIENT_TTL,
                revalidating: false,
            },
        );
        let (_, bytes) = fetch_bytes(&raw)
            .await
            .expect("备用 DNS 应恢复真实媒体下载");
        assert!(!bytes.is_empty());
        assert!(MEDIA_DNS_ROUTES
            .lock()
            .unwrap()
            .prefers_public(host, Instant::now()));
        let (_, again) = fetch_bytes(&raw).await.expect("后续请求应复用恢复的路径");
        if bytes.starts_with(b"#EXTM3U") {
            assert!(again.starts_with(b"#EXTM3U"));
        } else {
            assert_eq!(bytes, again);
        }
        if let Ok(path) = std::env::var("HGHUB_TEST_MEDIA_OUTPUT") {
            std::fs::write(path, bytes).unwrap();
        }
    }

    #[tokio::test]
    #[ignore = "联网测速：需要 HGHUB_TEST_MEDIA_URL 指向一个可下载的媒体分片"]
    async fn media_client_download_profile() {
        // 诊断工具：测量单连接吞吐与 chunk 分布。
        // 2026-11 实测结论（tp3.drleao.cn，RTT 27ms，2.1MB 分片）：
        //   reqwest: 91 chunks / avg 23KB / max gap 528ms / 0.35MB/s / 3-6s
        //   curl 同片: 4.2MB/s / 0.4s
        // → 瓶颈是 hyper 的小 buffer 读取被 RTT 放大（每次往返只搬 ~23KB），
        //   与压缩、HTTP/2、连接池、TLS 后端均无关。优化方向：预取 + 并发，而非调参。
        let raw = std::env::var("HGHUB_TEST_MEDIA_URL").expect("HGHUB_TEST_MEDIA_URL");
        let target = url::Url::parse(&raw).unwrap();

        let client = reqwest::Client::builder()
            .user_agent(UA)
            .timeout(Duration::from_secs(20))
            .build()
            .unwrap();
        let started = Instant::now();
        let mut resp = client.get(target.clone()).send().await.unwrap();
        let headers = started.elapsed();
        let mut chunks = 0usize;
        let mut bytes = 0usize;
        let mut max_gap = Duration::ZERO;
        let mut last = Instant::now();
        while let Some(chunk) = resp.chunk().await.unwrap() {
            let gap = last.elapsed();
            if gap > max_gap {
                max_gap = gap;
            }
            last = Instant::now();
            chunks += 1;
            bytes += chunk.len();
        }
        let total = started.elapsed();
        let avg = if chunks > 0 { bytes / chunks } else { 0 };
        eprintln!(
            "[stream] headers={}ms total={}ms chunks={chunks} bytes={bytes} avg_chunk={avg}B max_gap={}ms speed={:.2}MB/s",
            headers.as_millis(),
            total.as_millis(),
            max_gap.as_millis(),
            bytes as f64 / 1e6 / total.as_secs_f64().max(1e-6)
        );
    }

    #[tokio::test]
    async fn terminal_errors_do_not_retry_but_transient_failures_can_recover() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        for error in [
            "HTTP 404 Not Found: <html>nginx</html>",
            "HTTP 403 Forbidden",
            "401",
            "禁止访问内网",
            "重定向次数超过限制",
        ] {
            let calls = AtomicUsize::new(0);
            assert!(with_retry(true, || async {
                calls.fetch_add(1, Ordering::SeqCst);
                Err::<(), _>(error.to_string())
            })
            .await
            .is_err());
            assert_eq!(calls.load(Ordering::SeqCst), 1, "{error}");
        }
        for error in ["HTTP 503 Service Unavailable", "请求超时"] {
            let calls = AtomicUsize::new(0);
            let value = with_retry(true, || {
                let attempt = calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    if attempt == 0 {
                        Err(error.to_string())
                    } else {
                        Ok(42)
                    }
                }
            })
            .await
            .unwrap();
            assert_eq!(value, 42);
            assert_eq!(calls.load(Ordering::SeqCst), 2);
        }
    }

    #[test]
    fn api_operation_labels_exclude_user_input() {
        for (path, expected) in [
            ("/api/videos?page=1&size=10", "feed"),
            ("/api/videos/category/private?x=secret", "category"),
            ("/api/videos/42/play?ep=1", "play"),
            ("/api/search?q=private-search", "search"),
            ("/api/account/loginByPassword", "account"),
            ("/unknown/private", "other"),
        ] {
            assert_eq!(edge_operation(path), expected);
        }
    }

    /// 联网验证：客户端跨 30s DNS 复检点保活。手动运行：
    /// `cargo test --lib live_client_pool_survives_revalidation -- --ignored --nocapture`
    /// 只做 DNS 解析与客户端状态检查（不发媒体请求），验证连接池保活的三个语义：
    /// 首建 → 跨复检点后客户端仍是同一实例（连接池存活）→ 复检时间戳被后台任务刷新。
    #[tokio::test]
    #[ignore = "联网验证：需要真实 DNS 解析，手动用 --ignored --nocapture 运行"]
    async fn live_client_pool_survives_revalidation() {
        let target = format!("{}/", crate::net::base());
        refresh_client();
        let (_, first) = client_for_dns(&target, false).await.expect("first client");
        let key = format!(
            "{}:huangguoai.com:443:false",
            network_policy::strict_security()
        );
        let initial_next_check = CLIENTS
            .lock()
            .unwrap()
            .get(&key)
            .map(|e| e.next_check_at)
            .expect("entry should exist after first use");

        // 跨过复检点：下一次取用会触发后台复检，但客户端实例保持不变
        tokio::time::sleep(CLIENT_TTL + Duration::from_secs(5)).await;
        let (_, second) = client_for_dns(&target, false).await.expect("second client");
        assert!(
            Arc::ptr_eq(&first, &second),
            "DNS 复检答案一致时应保留同一客户端实例（连接池存活）"
        );

        // 后台复检完成后复检点应被推后（不再卡在初始值上反复触发）
        let mut refreshed = false;
        for _ in 0..20 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let next = CLIENTS.lock().unwrap().get(&key).map(|e| e.next_check_at);
            if let Some(next) = next {
                if next > initial_next_check {
                    refreshed = true;
                    break;
                }
            }
        }
        assert!(refreshed, "后台复检应刷新复检点时间戳");
        println!("client pool kept warm across revalidation; entry refreshed");
    }

    #[tokio::test]
    async fn non_idempotent_operations_are_attempted_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        for (retry, expected) in [(false, 1), (true, 3)] {
            let calls = AtomicUsize::new(0);
            let result = with_retry(retry, || async {
                calls.fetch_add(1, Ordering::SeqCst);
                Err::<(), _>("rejected".to_string())
            })
            .await;
            assert!(result.is_err());
            assert_eq!(calls.load(Ordering::SeqCst), expected);
        }
    }

    #[test]
    fn dns_diagnostics_only_expose_counts_and_fake_ip_presence() {
        let addrs: Vec<SocketAddr> = ["10.0.0.1:443", "198.18.0.2:443", "[::ffff:198.18.0.3]:443"]
            .into_iter()
            .map(|raw| raw.parse().unwrap())
            .collect();
        let summary = dns_summary(addrs);
        assert_eq!(summary, json!({"count": 3, "has_fake_ip": true}));
        assert!(!summary.to_string().contains("10.0.0.1"));
        assert!(!summary.to_string().contains("198.18"));
        assert_eq!(
            dns_summary(Vec::<SocketAddr>::new()),
            json!({"count":0,"has_fake_ip":false})
        );
    }

    #[test]
    fn revalidation_keeps_warm_pool_when_addresses_unchanged() {
        let now = Instant::now();
        let addr: SocketAddr = "93.184.216.34:443".parse().unwrap();
        let addr2: SocketAddr = "93.184.216.35:443".parse().unwrap();
        let mut entry = ClientEntry {
            http: Arc::new(reqwest::Client::new()),
            host: "example.com".into(),
            addrs: vec![addr, addr2],
            validated_at: now,
            next_check_at: now,
            revalidating: true,
        };

        // 相同答案 → 保留客户端（连接池不重建），推后复检点
        let rebuilt = apply_revalidation(&mut entry, Ok((vec![addr, addr2], CLIENT_TTL)), now);
        assert!(!rebuilt);
        assert_eq!(entry.next_check_at, now + CLIENT_TTL);
        assert_eq!(entry.addrs, vec![addr, addr2]);

        // 乱序的相同答案也视为一致（DNS 答案顺序不稳定）
        entry.revalidating = true;
        let rebuilt = apply_revalidation(&mut entry, Ok((vec![addr2, addr], CLIENT_TTL)), now);
        assert!(!rebuilt);
        assert_eq!(entry.addrs, vec![addr, addr2], "地址集合一致时不重建");

        // 新增了一个地址（合法答案变化）→ 重建客户端并更换固定地址
        let addr3: SocketAddr = "93.184.216.36:443".parse().unwrap();
        entry.revalidating = true;
        let before = entry.http.clone();
        let rebuilt =
            apply_revalidation(&mut entry, Ok((vec![addr, addr2, addr3], CLIENT_TTL)), now);
        assert!(rebuilt);
        assert!(!Arc::ptr_eq(&before, &entry.http));
        assert_eq!(entry.addrs, vec![addr, addr2, addr3]);
        assert_eq!(entry.next_check_at, now + CLIENT_TTL);

        // 复检失败 → 保留旧客户端（地址仍是构建时校验过的），按 30s 节奏重试
        let before = entry.http.clone();
        let rebuilt = apply_revalidation(&mut entry, Err("解析失败".into()), now);
        assert!(!rebuilt);
        assert!(Arc::ptr_eq(&before, &entry.http));
        assert_eq!(entry.next_check_at, now + CLIENT_TTL);
        assert!(!entry.revalidating);
    }

    #[test]
    fn edge_redirects_only_replay_anonymous_reads() {
        let current = url::Url::parse("https://huangguoai.ai/api/edge/request").unwrap();
        assert_eq!(
            edge_redirect_target(
                &current,
                301,
                "https://huangguoai.com/api/edge/request",
                true
            )
            .unwrap()
            .host_str(),
            Some("huangguoai.com")
        );
        assert!(edge_redirect_target(&current, 302, "/next", true).is_ok());
        for status in [301, 302, 303, 307, 308] {
            assert!(edge_redirect_target(&current, status, "/next", false).is_err());
        }
        for status in [307, 308] {
            assert!(edge_redirect_target(&current, status, "/next", true).is_err());
        }
        assert!(edge_redirect_target(&current, 301, "http://example.com/", true).is_err());
        assert!(edge_redirect_target(&current, 301, "https://127.0.0.1/", true).is_err());
    }

    #[test]
    fn proxy_credentials_and_query_are_redacted() {
        assert_eq!(
            redact_proxy("http://alice:p%40ss@127.0.0.1:7897/path?token=secret#hidden"),
            "http://127.0.0.1:7897"
        );
        assert_eq!(
            redact_proxy("socks5://alice:secret@[::1]:1080"),
            "socks5://[::1]:1080"
        );
        for raw in ["alice:secret@127.0.0.1:7897", "not a url secret", ""] {
            assert_eq!(redact_proxy(raw), "[已配置，原始值已隐藏]");
        }
    }

    #[test]
    fn base_requires_https_origin_and_auth_trust_is_static() {
        assert_eq!(
            normalize_base("https://HUANGGUOAI.COM:443/"),
            Some(DEFAULT_BASE.into())
        );
        for raw in [
            "http://huangguoai.com",
            "https://user:pass@huangguoai.com",
            "https://huangguoai.com/path",
            "https://huangguoai.com/?secret=1",
            "https://huangguoai.com/#fragment",
            "https://192.168.1.1",
        ] {
            assert_eq!(normalize_base(raw), None, "{raw}");
        }
        assert!(trusted_auth_origin(DEFAULT_BASE));
        assert!(trusted_auth_origin("https://huangguoai.ai"));
        assert!(!trusted_auth_origin("https://mirror.example"));
        assert!(!trusted_auth_origin("https://huangguoai.com.evil.example"));
    }

    #[tokio::test]
    async fn untrusted_origins_reject_credentials_before_network() {
        for (path, body, token) in [
            (
                "/api/account/loginByPassword",
                Some(json!({"password":"secret"})),
                None,
            ),
            (
                "/api/account/registerByPassword",
                Some(json!({"password":"secret"})),
                None,
            ),
            ("/api/videos", None, Some("secret-token")),
        ] {
            let err = edge("https://mirror.invalid", "POST", path, body, token)
                .await
                .unwrap_err();
            assert!(err.contains("镜像线路仅供游客浏览"));
        }
    }

    #[tokio::test]
    async fn private_get_is_rejected_before_network() {
        let err = get(
            "http://127.0.0.1:1/private",
            Duration::from_secs(1),
            None,
            None,
        )
        .await
        .unwrap_err();
        assert!(err.contains("禁止访问"));
    }

    /// 手动跑：`cargo test --lib -- --ignored net::tests::diag -- --nocapture`
    /// 关掉 TUN、只留系统代理时跑一遍，能直接看出客户端有没有走代理。
    #[tokio::test]
    #[ignore = "联网测试：请求上游做网络自检，手动用 --ignored 运行"]
    async fn diag() {
        println!(
            "{}",
            serde_json::to_string_pretty(&diagnose().await).expect("json")
        );
    }
}
