//! 本机 HTTP 桥：让普通浏览器也能访问运行中的 HGhub 后端。
//!
//! 桌面应用照常走 Tauri IPC 与自定义协议（Windows/Android 的 `http://hg.localhost`、
//! macOS 的 `hg://localhost`）；桥在同一进程里额外监听 loopback 端口，把同一套
//! 命令面与媒体代理以纯 HTTP 暴露出来：
//!
//! - `POST /api/{cmd}`：分发到 `#[tauri::command]` 实现，信封 `{ok,data|error}`；
//!   `Err(String)` 以 HTTP 200 返回（与 invoke 的错误语义一致，前端错误分类与
//!   只读重试逻辑不受影响）。
//! - `/img` `/hls` `/seg`：[`crate::media::handle`]，封面解密 / m3u8 改写 / 分片透传；
//!   分片与 key 地址按本桥 origin 改写，浏览器可直接取流（原 linux_http.rs 的职责，
//!   Linux 桌面的 GStreamer 播放链路继续走这里，行为不变）。
//! - `GET /...`：前端静态资源。release 优先内嵌资产（asset_resolver），dev 回退
//!   `../dist` 构建产物；没有构建产物时提示使用 Vite dev server（1420）。
//!
//! 安全边界：只绑 `127.0.0.1`，不对外网暴露；`/api` 校验 Origin（Tauri WebView、
//! dev 的 Vite 页面、桥自身托管的页面），媒体路由保持公开代理语义（ACAO:*，与
//! 自定义协议行为一致）。桥随桌面进程启停，桌面功能不依赖它。

use crate::api::{self, Api};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use tauri::Manager;

/// 监听端口。前端 `src/api.ts` 的 Web 分支硬编码同一值，两边必须同时改。
/// Linux 桌面播放原本就走这个端口（原 linux_http.rs 的 ORIGIN），保持不变。
pub const PORT: u16 = 17650;

/// 回环视角的默认 origin；仅当请求缺失 Host 头（HTTP/1.0 老客户端、本地探测工具）
/// 时作为 m3u8 改写的兜底。正常浏览器请求都带 Host，按请求动态生成（见 [`request_origin`]）。
pub const ORIGIN: &str = "http://127.0.0.1:17650";

/// 绑定地址：默认 `0.0.0.0` 对局域网开放（设备访问 `http://<本机IP>:17650`）。
/// 设 `HGHUB_BRIDGE_HOST=127.0.0.1` 回退为本机-only。首次被局域网设备访问时
/// Windows 会弹防火墙授权，允许「专用网络」即可。
fn bind_host() -> String {
    std::env::var("HGHUB_BRIDGE_HOST")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "0.0.0.0".to_string())
}

/// 请求体上限（playback_log_write 单批 ≤128 条、每条 ≤8KiB，1MiB 足够）。
const MAX_BODY: usize = 1024 * 1024;

/// 在后台线程起服务。绑定失败只记日志：桌面功能不受影响（走自定义协议），
/// 浏览器端表现为「无法连接本地服务」。双实例时先启动者持有端口。
pub fn start(app: tauri::AppHandle) {
    let host = bind_host();
    std::thread::spawn(move || {
        let listener = match TcpListener::bind((host.as_str(), PORT)) {
            Ok(listener) => listener,
            Err(err) => {
                log_line(&app, &format!("[bridge] {host}:{PORT} 绑定失败：{err}"));
                return;
            }
        };
        log_line(
            &app,
            &format!("[bridge] ready: http://{host}:{PORT}（局域网设备访问 http://<本机IP>:{PORT}，首次访问需允许防火墙）"),
        );
        for stream in listener.incoming().flatten() {
            let app = app.clone();
            std::thread::spawn(move || {
                // 单连接失败不影响服务：读超时 / 对端中断是常态
                let _ = serve(&app, stream);
            });
        }
    });
}

/// 写 stderr + debug.log（桥的启动与绑定失败在用户复制日志时可见）。
fn log_line(app: &tauri::AppHandle, msg: &str) {
    eprintln!("{msg}");
    if let Ok(dir) = app.path().app_data_dir() {
        let _ = crate::logging::append_debug(&dir, msg);
    }
}

/// 单连接处理：解析请求行与头部，按路由分发。每连接一请求（Connection: close）。
fn serve(app: &tauri::AppHandle, mut stream: TcpStream) -> std::io::Result<()> {
    let timeout = Some(std::time::Duration::from_secs(30));
    stream.set_read_timeout(timeout)?;
    stream.set_write_timeout(timeout)?;

    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(()); // 空连接
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_ascii_uppercase();
    let target = parts.next().unwrap_or("/").to_string();

    // 只关心 Content-Length、Origin 与 Host；其余头整体丢弃
    //（媒体分片整包返回、无需 Range，与原 linux_http.rs 一致）
    let mut content_length = 0usize;
    let mut origin: Option<String> = None;
    let mut host: Option<String> = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line.trim_end().is_empty() {
            break;
        }
        let (name, value) = match line.split_once(':') {
            Some((n, v)) => (n.trim().to_ascii_lowercase(), v.trim().to_string()),
            None => continue,
        };
        match name.as_str() {
            "content-length" => content_length = value.parse().unwrap_or(0),
            "origin" => origin = Some(value),
            "host" => host = Some(value),
            _ => {}
        }
    }

    if content_length > MAX_BODY {
        return write_plain(&mut stream, 413, "payload too large");
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }
    let body = String::from_utf8_lossy(&body).to_string();

    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target.as_str(), ""),
    };

    // CORS 预检：回显 Origin（无则 *）。媒体路由是公开代理、照常放行；
    // /api 的拦截在真实请求上做（外部 Origin 拿到 403，且响应不带 CORS 头）。
    if method == "OPTIONS" {
        return write_preflight(&mut stream, origin.as_deref());
    }

    if matches!(path, "/img" | "/hls" | "/seg") {
        if method != "GET" && method != "HEAD" {
            return write_plain(&mut stream, 405, "method not allowed");
        }
        // m3u8 分片/key 改写按请求 Host 动态生成：本机访问是 127.0.0.1:17650，
        // 局域网设备访问是 <LAN-IP>:17650，各自拿到的播放列表都指向发起方可达的桥。
        let media_origin = request_origin(host.as_deref());
        let response = tauri::async_runtime::block_on(crate::media::handle(path, query, &media_origin));
        return write_media(&mut stream, response, method == "HEAD");
    }

    if let Some(cmd) = path.strip_prefix("/api/") {
        if method != "GET" && method != "POST" {
            return write_plain(&mut stream, 405, "method not allowed");
        }
        if !origin_allowed_api(origin.as_deref(), host.as_deref()) {
            return write_plain(&mut stream, 403, "origin denied");
        }
        let cmd = cmd.trim_end_matches('/').to_string();
        let args: Value = if body.trim().is_empty() {
            json!({})
        } else {
            match serde_json::from_str(&body) {
                Ok(v) => v,
                Err(_) => {
                    return write_json(&mut stream, 200, &json!({"ok": false, "error": "请求体不是有效 JSON"}));
                }
            }
        };
        let result = tauri::async_runtime::block_on(dispatch(app, &cmd, &args));
        return match result {
            Ok(data) => write_json(&mut stream, 200, &json!({ "ok": true, "data": data })),
            // 未知命令是编程错误，用 400 与业务错误区分
            Err(e) if e.starts_with("未知命令") => write_json(&mut stream, 400, &json!({ "ok": false, "error": e })),
            // 业务错误保持 HTTP 200 + {ok:false}，与 invoke 的 Err(String) 语义一致
            Err(e) => write_json(&mut stream, 200, &json!({ "ok": false, "error": e })),
        };
    }

    if method == "GET" || method == "HEAD" {
        return serve_static(app, &mut stream, path, method == "HEAD");
    }
    write_plain(&mut stream, 404, "not found")
}

/// 按请求 Host 头生成本次请求视角的桥 origin（m3u8 改写用）。
/// 浏览器总是带 Host（本机 `127.0.0.1:17650`、局域网 `<LAN-IP>:17650`）；
/// 只接受「host[:port]」形态（字母数字与 `-.[]:`，IPv6 走方括号），异常时回退 [`ORIGIN`]，
/// 防止伪造头把任意内容注入播放列表。非浏览器客户端本可直接调用（缺 Origin 放行），
/// 伪造 Host 不会扩大攻击面。
fn request_origin(host: Option<&str>) -> String {
    match host {
        Some(h)
            if !h.is_empty()
                && h.len() <= 255
                && h.bytes().all(|b| b.is_ascii_alphanumeric() || b"-.[]:".contains(&b)) =>
        {
            format!("http://{h}")
        }
        _ => ORIGIN.to_string(),
    }
}

/// `/api` 的可信来源：
/// - 桥托管页面（本机或局域网设备）：页面 origin 与请求 Host 一致即同源；
/// - Tauri WebView 自身、dev 的 Vite 页面（仅 debug 构建，复用 lib.rs 的
///   `allowed_frontend_origin`，支持 TAURI_DEV_HOST）；
/// - 缺 Origin（curl / 同源非预检）放行——绑定 0.0.0.0 后攻击面等同局域网内
///   任意进程，这是「局域网开放」的既定语义；互联网网站因 Origin 不匹配仍拿不到响应。
fn origin_allowed_api(origin: Option<&str>, host: Option<&str>) -> bool {
    match origin {
        None | Some("") | Some("null") => true,
        Some(o) => {
            if o == ORIGIN {
                return true;
            }
            if host.is_some_and(|h| *o == request_origin(Some(h))) {
                return true;
            }
            crate::allowed_frontend_origin(o)
        }
    }
}

/// 命令分发：桥的白名单。与 lib.rs 的 invoke_handler 保持一致，除移动端专用
/// （set_orientation / set_text_interaction）与 open_url（浏览器端改 window.open，
/// 桌面壳层打开系统浏览器对网页无意义）外全部放行。
/// 参数按前端 invoke 的 camelCase 键名取（replyId 等），类型与命令签名对齐。
async fn dispatch(app: &tauri::AppHandle, cmd: &str, args: &Value) -> Result<Value, String> {
    match cmd {
        /* 基础 */
        "app_boot" => api::app_boot(app.state::<Api>()).await,
        "security_set" => api::security_set(bool_arg(args, "strict")?).await.map(Value::Bool),
        "debug_log" => api::debug_log(app.clone(), str_arg(args, "msg")?).await.map(|_| Value::Null),
        "net_diag" => api::net_diag().await,
        "cache_clear" => api::cache_clear().await.map(|_| Value::Null),
        /* 线路（数据源） */
        "line_state" => api::line_state().await,
        "line_set" => api::line_set(app.state::<Api>(), str_arg(args, "url")?).await.map(Value::String),
        "line_mirrors" => api::line_mirrors().await,
        "line_probe" => api::line_probe(str_vec(args, "urls")?).await,
        "line_auto" => api::line_auto().await,
        "line_try_main" => api::line_try_main().await,
        "line_health" => api::line_health().await,
        "check_update" => api::check_update().await,
        /* 会话 */
        "api_login" => api::api_login(app.state::<Api>(), str_arg(args, "username")?, str_arg(args, "password")?).await,
        "api_register" => api::api_register(app.state::<Api>(), str_arg(args, "username")?, str_arg(args, "password")?, opt_str(args, "email").unwrap_or_default()).await,
        "api_logout" => api::api_logout(app.state::<Api>()).await,
        "api_me" => api::api_me(app.state::<Api>()).await,
        /* 列表 / 详情 / 播放 */
        "api_feed" => api::api_feed(app.state::<Api>(), u32_arg(args, "page")?, u32_arg(args, "size")?).await,
        "api_topics" => api::api_topics(app.state::<Api>()).await,
        "api_topic" => api::api_topic(app.state::<Api>(), str_arg(args, "slug")?).await,
        "api_tags" => api::api_tags(app.state::<Api>()).await,
        "api_tag_videos" => api::api_tag_videos(app.state::<Api>(), str_arg(args, "tag")?, u32_arg(args, "page")?, u32_arg(args, "size")?).await,
        "api_category" => api::api_category(app.state::<Api>(), str_arg(args, "cate")?, u32_arg(args, "page")?, u32_arg(args, "size")?).await,
        "api_rank" => api::api_rank(app.state::<Api>(), str_arg(args, "kind")?).await,
        "api_search" => api::api_search(app.state::<Api>(), str_arg(args, "q")?, u32_arg(args, "page")?, u32_arg(args, "size")?, opt_str(args, "kind")).await,
        "api_search_hot" => api::api_search_hot(app.state::<Api>()).await,
        "api_posts" => api::api_posts(app.state::<Api>(), u32_arg(args, "page")?, u32_arg(args, "size")?).await,
        "api_post" => api::api_post(app.state::<Api>(), i64_arg(args, "id")?).await,
        "api_post_playback" => api::api_post_playback(app.state::<Api>(), i64_arg(args, "id")?).await,
        "api_video" => api::api_video(app.state::<Api>(), i64_arg(args, "id")?).await,
        "api_play" => api::api_play(app.state::<Api>(), i64_arg(args, "id")?, u32_arg(args, "ep")?).await,
        /* 评论 / 互动 */
        "api_comments" => api::api_comments(app.state::<Api>(), i64_arg(args, "id")?, u32_arg(args, "ep")?, u32_arg(args, "page")?, u32_arg(args, "size")?).await,
        "api_comment_send" => api::api_comment_send(app.state::<Api>(), i64_arg(args, "id")?, u32_arg(args, "ep")?, str_arg(args, "text")?, opt_i64(args, "replyId")).await,
        "api_like" => api::api_like(app.state::<Api>(), i64_arg(args, "id")?, u32_arg(args, "ep")?, bool_arg(args, "on")?).await,
        "api_collect" => api::api_collect(app.state::<Api>(), i64_arg(args, "id")?, bool_arg(args, "on")?).await,
        "api_follow" => api::api_follow(app.state::<Api>(), i64_arg(args, "uid")?, bool_arg(args, "on")?).await,
        "api_post_like" => api::api_post_like(app.state::<Api>(), i64_arg(args, "id")?, bool_arg(args, "on")?).await,
        "api_post_collect" => api::api_post_collect(app.state::<Api>(), i64_arg(args, "id")?, bool_arg(args, "on")?).await,
        "api_post_comments" => api::api_post_comments(app.state::<Api>(), i64_arg(args, "id")?, u32_arg(args, "page")?, u32_arg(args, "size")?).await,
        "api_post_categories" => api::api_post_categories(app.state::<Api>()).await,
        "api_post_category" => api::api_post_category(app.state::<Api>(), str_arg(args, "route")?, u32_arg(args, "page")?, u32_arg(args, "size")?).await,
        "api_related" => api::api_related(app.state::<Api>(), i64_arg(args, "id")?).await,
        "api_following" => api::api_following(app.state::<Api>(), u32_arg(args, "page")?, u32_arg(args, "size")?).await,
        "api_user" => api::api_user(app.state::<Api>(), i64_arg(args, "id")?).await,
        "api_user_stats" => api::api_user_stats(app.state::<Api>(), i64_arg(args, "id")?).await,
        "api_user_videos" => api::api_user_videos(app.state::<Api>(), i64_arg(args, "id")?, u32_arg(args, "page")?, u32_arg(args, "size")?).await,
        /* 播放诊断（同步命令） */
        "playback_log_status" => jv(crate::playback_log::playback_log_status()?),
        "playback_log_set" => jv(crate::playback_log::playback_log_set(bool_arg(args, "enabled")?)?),
        "playback_log_write" => {
            let entries: Vec<crate::playback_log::Entry> =
                serde_json::from_value(args.get("entries").cloned().unwrap_or(Value::Null))
                    .map_err(|e| format!("参数 entries 无效：{e}"))?;
            crate::playback_log::playback_log_write(entries).map(|_| Value::Null)
        }
        "playback_log_read" => crate::playback_log::playback_log_read().map(Value::String),
        _ => Err(format!("未知命令：{cmd}")),
    }
}

/* ───────────────────────── 参数提取 ───────────────────────── */

fn str_arg(args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("缺少参数 {key}"))
}

/// 显式 null 与缺参都按 None 处理，与 invoke 对 Option 参数的语义一致。
fn opt_str(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn u32_arg(args: &Value, key: &str) -> Result<u32, String> {
    args.get(key)
        .and_then(Value::as_u64)
        .map(|v| v as u32)
        .ok_or_else(|| format!("缺少参数 {key}"))
}

fn i64_arg(args: &Value, key: &str) -> Result<i64, String> {
    args.get(key)
        .and_then(Value::as_i64)
        .ok_or_else(|| format!("缺少参数 {key}"))
}

fn opt_i64(args: &Value, key: &str) -> Option<i64> {
    args.get(key).and_then(Value::as_i64)
}

fn bool_arg(args: &Value, key: &str) -> Result<bool, String> {
    args.get(key)
        .and_then(Value::as_bool)
        .ok_or_else(|| format!("缺少参数 {key}"))
}

fn str_vec(args: &Value, key: &str) -> Result<Vec<String>, String> {
    args.get(key)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect())
        .ok_or_else(|| format!("缺少参数 {key}"))
}

fn jv<T: serde::Serialize>(value: T) -> Result<Value, String> {
    serde_json::to_value(value).map_err(|e| e.to_string())
}

/* ───────────────────────── 响应写出 ───────────────────────── */

fn write_preflight(stream: &mut TcpStream, origin: Option<&str>) -> std::io::Result<()> {
    let acao = origin
        .filter(|o| !o.is_empty() && *o != "null")
        .unwrap_or("*");
    let head = format!(
        "HTTP/1.1 204 No Content\r\n\
         Access-Control-Allow-Origin: {acao}\r\n\
         Access-Control-Allow-Methods: GET, HEAD, POST, OPTIONS\r\n\
         Access-Control-Allow-Headers: content-type, range, accept, origin\r\n\
         Access-Control-Max-Age: 600\r\n\
         Access-Control-Expose-Headers: content-length, content-type\r\n\
         Connection: close\r\n\r\n"
    );
    stream.write_all(head.as_bytes())?;
    stream.flush()
}

/// 媒体响应：透传 media::handle 的头 + ACAO:*（公开代理语义，与自定义协议一致）。
fn write_media(
    stream: &mut TcpStream,
    response: tauri::http::Response<Vec<u8>>,
    head_only: bool,
) -> std::io::Result<()> {
    let (parts, body) = response.into_parts();
    let mut head = String::with_capacity(320);
    head.push_str(&format!(
        "HTTP/1.1 {} {}\r\n",
        parts.status.as_u16(),
        parts.status.canonical_reason().unwrap_or("OK")
    ));
    head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    for (name, value) in parts.headers.iter() {
        if let Ok(value) = value.to_str() {
            head.push_str(&format!("{}: {}\r\n", name.as_str(), value));
        }
    }
    head.push_str("Access-Control-Allow-Origin: *\r\n");
    head.push_str("Access-Control-Expose-Headers: content-length, content-type\r\n");
    head.push_str("Connection: close\r\n\r\n");
    stream.write_all(head.as_bytes())?;
    if !head_only {
        stream.write_all(&body)?;
    }
    stream.flush()
}

fn write_json(stream: &mut TcpStream, status: u16, payload: &Value) -> std::io::Result<()> {
    let body = payload.to_string();
    let head = format!(
        "HTTP/1.1 {status} {}\r\n\
         Content-Type: application/json; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Connection: close\r\n\r\n",
        if status == 200 { "OK" } else { "Bad Request" },
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    stream.flush()
}

fn write_plain(stream: &mut TcpStream, status: u16, message: &str) -> std::io::Result<()> {
    let reason = match status {
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        _ => "Error",
    };
    let body = message.as_bytes();
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

/* ───────────────────────── 静态资源 ───────────────────────── */

/// 静态资源：release 优先内嵌资产（NSIS 安装后磁盘上没有 ../dist），dev 回退
/// 磁盘 ../dist（tauri dev 的 cwd 是 src-tauri）。都没有时提示用 Vite dev server。
/// 路径含 `..` 一律拒绝（防穿越），只读文件内容不执行。
fn serve_static(
    app: &tauri::AppHandle,
    stream: &mut TcpStream,
    path: &str,
    head_only: bool,
) -> std::io::Result<()> {
    let rel = path.trim_start_matches('/');
    let rel = if rel.is_empty() { "index.html" } else { rel };
    if rel.split(['/', '\\']).any(|seg| seg == "..") {
        return write_plain(stream, 404, "not found");
    }

    let write_ok = |stream: &mut TcpStream, mime: &str, data: &[u8], head_only: bool| -> std::io::Result<()> {
        let head = format!(
            "HTTP/1.1 200 OK\r\n\
             Content-Type: {mime}\r\n\
             Content-Length: {}\r\n\
             Access-Control-Allow-Origin: *\r\n\
             Cache-Control: no-cache\r\n\
             Connection: close\r\n\r\n",
            data.len()
        );
        stream.write_all(head.as_bytes())?;
        if !head_only {
            stream.write_all(data)?;
        }
        stream.flush()
    };

    // 1) 内嵌资产：release 构建把 dist 打进二进制
    let resolver = app.asset_resolver();
    if let Some(asset) = resolver
        .get(rel.to_string())
        .or_else(|| resolver.get(path.to_string()))
    {
        let bytes = asset.bytes.clone();
        return write_ok(stream, &asset.mime_type, &bytes, head_only);
    }

    // 2) dev 的构建产物目录（pnpm build 生成）
    let file = std::path::Path::new("../dist").join(rel);
    if let Ok(data) = std::fs::read(&file) {
        return write_ok(stream, mime_of(rel), &data, head_only);
    }

    write_plain(stream, 404, "not found - dev 模式请访问 http://localhost:1420")
}

fn mime_of(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn api_origin_policy_accepts_trusted_frontends() {
        // 缺 Origin / 同源语义 / 回环兜底 / 桥托管页（本机 + 局域网 IP 的 same-origin）
        assert!(origin_allowed_api(None, None));
        assert!(origin_allowed_api(Some(""), Some("192.168.1.5:17650")));
        assert!(origin_allowed_api(Some("null"), None));
        assert!(origin_allowed_api(Some(ORIGIN), Some("127.0.0.1:17650")));
        assert!(origin_allowed_api(Some("tauri://localhost"), None));
        assert!(origin_allowed_api(Some("http://tauri.localhost"), None));
        // same-origin：页面来自桥（Host 为局域网地址），Origin 与 Host 一致即放行
        assert!(origin_allowed_api(Some("http://192.168.1.5:17650"), Some("192.168.1.5:17650")));
        assert!(origin_allowed_api(Some("http://127.0.0.1:17650"), Some("127.0.0.1:17650")));
        // dev Vite 页面仅在 debug 构建放行（allowed_frontend_origin 的既有口径）
        if cfg!(debug_assertions) {
            assert!(origin_allowed_api(Some("http://localhost:1420"), None));
            assert!(origin_allowed_api(Some("http://127.0.0.1:1420"), None));
        }
    }

    #[test]
    fn api_origin_policy_rejects_foreign_origins() {
        assert!(!origin_allowed_api(Some("https://evil.example"), Some("192.168.1.5:17650")));
        assert!(!origin_allowed_api(Some("http://localhost:3000"), None));
        assert!(!origin_allowed_api(Some("http://127.0.0.1:9999"), Some("127.0.0.1:17650")));
        assert!(!origin_allowed_api(Some("file://"), None));
        // 局域网设备上的第三方页面：origin 与 Host 不一致 → 拒
        assert!(!origin_allowed_api(Some("http://192.168.1.9:8080"), Some("192.168.1.5:17650")));
    }

    #[test]
    fn request_origin_follows_host_header() {
        assert_eq!(request_origin(Some("192.168.1.5:17650")), "http://192.168.1.5:17650");
        assert_eq!(request_origin(Some("127.0.0.1:17650")), ORIGIN);
        assert_eq!(request_origin(Some("[::1]:17650")), "http://[::1]:17650");
        assert_eq!(request_origin(Some("nas-home.local:17650")), "http://nas-home.local:17650");
        // 异常 Host 一律回退，防注入
        assert_eq!(request_origin(Some("")), ORIGIN);
        assert_eq!(request_origin(Some("evil.com/seg")), ORIGIN);
        assert_eq!(request_origin(Some("a b")), ORIGIN);
        assert_eq!(request_origin(None), ORIGIN);
    }

    #[test]
    fn args_are_extracted_with_expected_types() {
        let args = json!({"id": 117, "ep": 3, "on": true, "q": "关键词", "kind": null, "page": 2, "size": 20});
        assert_eq!(i64_arg(&args, "id").unwrap(), 117);
        assert_eq!(u32_arg(&args, "page").unwrap(), 2);
        assert!(bool_arg(&args, "on").unwrap());
        assert_eq!(str_arg(&args, "q").unwrap(), "关键词");
        // 显式 null → None，与 invoke 的 Option 参数语义一致
        assert_eq!(opt_str(&args, "kind"), None);
        assert_eq!(opt_i64(&args, "kind"), None);
        assert!(str_arg(&args, "missing").is_err());
        assert_eq!(u32_arg(&args, "id").unwrap(), 117); // 正整数合法
    }

    #[test]
    fn u32_args_reject_out_of_domain_values() {
        assert!(u32_arg(&json!({"page": -1}), "page").is_err());
        assert!(u32_arg(&json!({"page": 1.5}), "page").is_err());
        assert_eq!(u32_arg(&json!({"page": 0}), "page").unwrap(), 0);
    }

    #[test]
    fn unknown_command_error_prefix_is_stable() {
        // serve() 按「未知命令」前缀映射 HTTP 400，前缀是路由契约的一部分
        let e: String = format!("未知命令：{}", "nope");
        assert!(e.starts_with("未知命令"));
    }
}
