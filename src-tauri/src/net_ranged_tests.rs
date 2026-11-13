//! 仅本地 HTTP fixture；通过私有请求注入点测试，不修改生产网络策略或全局配置。
use super::*;
use std::future::Future;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::AtomicUsize;
use std::thread::JoinHandle;

type Span = Option<(u64, u64)>;
type TestResponse =
    std::pin::Pin<Box<dyn Future<Output = Result<reqwest::Response, String>> + Send>>;

pub(super) struct Server {
    url: String,
    pub(super) seen: Arc<Mutex<Vec<Span>>>,
    stopped: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Server {
    pub(super) fn start(handler: impl Fn(Span, &mut TcpStream) + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let stopped = Arc::new(AtomicBool::new(false));
        let requests = seen.clone();
        let stop = stopped.clone();
        let handler = Arc::new(handler);
        let worker = std::thread::spawn(move || {
            let mut workers = Vec::new();
            for incoming in listener.incoming() {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(mut stream) = incoming else { break };
                let requests = requests.clone();
                let handler = handler.clone();
                workers.push(std::thread::spawn(move || {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    stream
                        .set_write_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    let mut header = Vec::new();
                    let mut byte = [0];
                    while header.len() < 8192 {
                        if stream.read(&mut byte).unwrap_or(0) == 0 {
                            return;
                        }
                        header.push(byte[0]);
                        if header.ends_with(b"\r\n\r\n") {
                            break;
                        }
                    }
                    let text = String::from_utf8(header).unwrap();
                    let span = text.lines().find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        if !name.eq_ignore_ascii_case("range") {
                            return None;
                        }
                        let (start, end) = value.trim().strip_prefix("bytes=")?.split_once('-')?;
                        Some((start.parse().unwrap(), end.parse().unwrap()))
                    });
                    requests.lock().unwrap().push(span);
                    handler(span, &mut stream);
                }));
            }
            for worker in workers {
                worker.join().unwrap();
            }
        });
        Self {
            url: format!("http://{address}/segment"),
            seen,
            stopped,
            worker: Some(worker),
        }
    }

    pub(super) fn request(&self) -> impl Fn(Span) -> TestResponse + Clone + Send + Sync + 'static {
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let url = self.url.clone();
        move |span| -> TestResponse {
            let mut request = client
                .get(&url)
                .header(reqwest::header::ACCEPT_ENCODING, "identity");
            if let Some((start, end)) = span {
                request = request.header(reqwest::header::RANGE, format!("bytes={start}-{end}"));
            }
            Box::pin(async move { request.send().await.map_err(|error| err_text(&error)) })
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        let address = self
            .url
            .trim_start_matches("http://")
            .split('/')
            .next()
            .unwrap();
        let _ = TcpStream::connect(address);
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}

fn reply(stream: &mut TcpStream, status: u16, range: Option<String>, declared: usize, body: &[u8]) {
    let mut header = format!("HTTP/1.1 {status} fixture\r\nContent-Length: {declared}\r\nContent-Type: video/mp2t\r\nConnection: close\r\n");
    if let Some(range) = range {
        header.push_str(&format!("Content-Range: {range}\r\n"));
    }
    header.push_str("\r\n");
    // 取消测试会主动断连，fixture 写入错误属于预期。
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

pub(super) fn payload(size: usize) -> Arc<Vec<u8>> {
    Arc::new((0..size).map(|index| (index % 251) as u8).collect())
}

pub(super) fn range_reply(stream: &mut TcpStream, span: Span, data: &[u8]) {
    match span {
        Some((start, end)) => {
            let end = end.min(data.len() as u64 - 1);
            let bytes = &data[start as usize..=end as usize];
            reply(
                stream,
                206,
                Some(format!("bytes {start}-{end}/{}", data.len())),
                bytes.len(),
                bytes,
            );
        }
        None => reply(stream, 200, None, data.len(), data),
    }
}

#[tokio::test]
async fn ignored_range_reuses_200_response() {
    let data = payload(90000);
    let body = data.clone();
    let server = Server::start(move |_, stream| reply(stream, 200, None, body.len(), &body));
    let (ctype, bytes) = fetch_bytes_ranged_with(server.request(), 8).await.unwrap();
    assert_eq!(ctype.as_deref(), Some("video/mp2t"));
    assert_eq!(&bytes, data.as_ref());
    assert_eq!(
        *server.seen.lock().unwrap(),
        vec![Some((0, RANGE_PROBE_BYTES - 1))]
    );
}

#[tokio::test]
async fn tiny_object_finishes_in_first_probe() {
    let data = payload(123);
    let body = data.clone();
    let server = Server::start(move |span, stream| range_reply(stream, span, &body));
    let (_, bytes) = fetch_bytes_ranged_with(server.request(), 8).await.unwrap();
    assert_eq!(&bytes, data.as_ref());
    assert_eq!(server.seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn small_object_only_downloads_remaining_bytes() {
    let data = payload(150000);
    let body = data.clone();
    let server = Server::start(move |span, stream| range_reply(stream, span, &body));
    let (_, bytes) = fetch_bytes_ranged_with(server.request(), 8).await.unwrap();
    assert_eq!(&bytes, data.as_ref());
    assert_eq!(
        *server.seen.lock().unwrap(),
        vec![Some((0, 65535)), Some((65536, 149999))]
    );
}

#[tokio::test]
async fn parallel_ranges_cover_object_once_and_are_bounded() {
    // 超过 MAX_RANGE_BYTES * 2，迫使 parts=2 的调度器分批派发有界子块。
    let data = payload(MAX_RANGE_BYTES as usize * 2 + RANGE_PROBE_BYTES as usize + 100);
    let body = data.clone();
    let server = Server::start(move |span, stream| range_reply(stream, span, &body));
    let (_, bytes) = fetch_bytes_ranged_with(server.request(), 2).await.unwrap();
    assert_eq!(&bytes, data.as_ref());
    let mut spans: Vec<_> = server
        .seen
        .lock()
        .unwrap()
        .iter()
        .map(|span| span.unwrap())
        .collect();
    spans.sort_unstable();
    let mut next = 0;
    for (start, end) in spans {
        assert_eq!(start, next);
        assert!(end - start < MAX_RANGE_BYTES);
        next = end + 1;
    }
    assert_eq!(next, data.len() as u64);
}

#[tokio::test]
async fn malformed_probe_ranges_fall_back_once() {
    for range in [
        "bytes 1-65536/1200000",
        "bytes 0-65534/1200000",
        "bytes 0-65535/*",
        "bytes 0-65535/0",
        "bytes 0-65535/65535",
        "bytes 0-65535/+1200000",
        "items 0-65535/1200000",
        "bytes 0-65535/1200000, 0-1/1200000",
    ] {
        let range = range.to_string();
        let server = Server::start(move |span, stream| {
            if span.is_some() {
                reply(stream, 206, Some(range.clone()), 0, b"");
            } else {
                reply(stream, 200, None, 4, b"good");
            }
        });
        let (_, bytes) = fetch_bytes_ranged_with(server.request(), 8).await.unwrap();
        assert_eq!(bytes, b"good");
        assert_eq!(server.seen.lock().unwrap().len(), 2);
    }
}

#[tokio::test]
async fn truncated_probe_and_wrong_declared_length_never_return_partial_data() {
    for declared in [RANGE_PROBE_BYTES as usize, 7] {
        let server = Server::start(move |span, stream| {
            if span.is_some() {
                reply(
                    stream,
                    206,
                    Some("bytes 0-65535/1200000".into()),
                    declared,
                    b"short",
                );
            } else {
                reply(stream, 200, None, 4, b"good");
            }
        });
        let (_, bytes) = fetch_bytes_ranged_with(server.request(), 8).await.unwrap();
        assert_eq!(bytes, b"good");
        assert_eq!(server.seen.lock().unwrap().len(), 2);
    }
}

#[tokio::test]
async fn child_range_total_and_body_length_are_checked() {
    for defect in ["start", "end", "total", "truncated", "overlong"] {
        let data = payload(1200000);
        let server = Server::start(move |span, stream| {
            let Some((start, end)) = span else {
                reply(stream, 200, None, 4, b"good");
                return;
            };
            if start == 0 {
                range_reply(stream, span, &data);
                return;
            }
            let len = (end - start + 1) as usize;
            let range = match defect {
                "start" => format!("bytes {}-{end}/1200000", start + 1),
                "end" => format!("bytes {start}-{}/1200000", end - 1),
                "total" => format!("bytes {start}-{end}/1200001"),
                _ => format!("bytes {start}-{end}/1200000"),
            };
            let actual = if defect == "truncated" { len - 1 } else { len };
            let declared = if defect == "overlong" { len + 1 } else { len };
            reply(
                stream,
                206,
                Some(range),
                declared,
                &data[start as usize..start as usize + actual],
            );
        });
        let (_, bytes) = fetch_bytes_ranged_with(server.request(), 2).await.unwrap();
        assert_eq!(bytes, b"good", "{defect}");
        assert_eq!(
            server
                .seen
                .lock()
                .unwrap()
                .iter()
                .filter(|span| span.is_none())
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn forbidden_and_rate_limited_do_not_trigger_fallback() {
    for code in [403, 429] {
        for deny_probe in [true, false] {
            let data = payload(1200000);
            let server = Server::start(move |span, stream| {
                if !deny_probe && span.is_some_and(|(start, _)| start == 0) {
                    range_reply(stream, span, &data);
                } else {
                    reply(stream, code, None, 0, b"");
                }
            });
            let error = fetch_bytes_ranged_with(server.request(), 2)
                .await
                .unwrap_err();
            assert!(error.starts_with(&format!("HTTP {code}")));
            let seen = server.seen.lock().unwrap();
            assert!(seen.iter().all(Option::is_some));
            assert!(seen.len() <= if deny_probe { 1 } else { 3 });
        }
    }
}

#[tokio::test]
async fn oversized_total_and_200_body_are_terminal() {
    for status in [200, 206] {
        let server = Server::start(move |_, stream| {
            reply(
                stream,
                status,
                (status == 206).then(|| format!("bytes 0-65535/{}", MAX_RANGED_OBJECT_BYTES + 1)),
                if status == 200 {
                    MAX_RANGED_OBJECT_BYTES as usize + 1
                } else {
                    0
                },
                b"",
            );
        });
        assert!(fetch_bytes_ranged_with(server.request(), 8).await.is_err());
        assert_eq!(server.seen.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn fallback_is_attempted_only_once_even_when_it_fails() {
    let server = Server::start(move |span, stream| {
        if span.is_some() {
            reply(stream, 206, Some("broken".into()), 0, b"");
        } else {
            reply(stream, 500, None, 0, b"");
        }
    });
    assert!(fetch_bytes_ranged_with(server.request(), 8)
        .await
        .unwrap_err()
        .starts_with("HTTP 500"));
    assert_eq!(server.seen.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn streamed_lengths_are_checked_without_content_length() {
    for (actual, expected, limit) in [(4, Some(5), 10), (6, Some(5), 10), (6, None, 5)] {
        let server = Server::start(move |_, stream| {
            let body = vec![b'x'; actual];
            let _ = stream.write_all(b"HTTP/1.1 206 fixture\r\nTransfer-Encoding: chunked\r\nContent-Range: bytes 0-4/5\r\nConnection: close\r\n\r\n");
            let _ = write!(stream, "{:x}\r\n", body.len());
            let _ = stream.write_all(&body);
            let _ = stream.write_all(b"\r\n0\r\n\r\n");
            let _ = stream.flush();
        });
        let response = server.request()(Some((0, 4))).await.unwrap();
        assert_eq!(response.content_length(), None);
        let failure = read_media_body(response, expected, limit)
            .await
            .err()
            .expect("正文长度必须校验");
        assert_eq!(failure.fallback, expected.is_some());
    }
}

struct ActiveRequest(Arc<AtomicUsize>, Arc<tokio::sync::Notify>);
impl Drop for ActiveRequest {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
        self.1.notify_one();
    }
}

#[tokio::test]
async fn parent_cancellation_drops_all_inflight_children() {
    let data = payload(1200000);
    let server = Server::start(move |span, stream| range_reply(stream, span, &data));
    let request = server.request();
    let active = Arc::new(AtomicUsize::new(0));
    let changed = Arc::new(tokio::sync::Notify::new());
    let count = active.clone();
    let signal = changed.clone();
    let parent = tokio::spawn(fetch_bytes_ranged_with(
        move |span| {
            let request = request.clone();
            let count = count.clone();
            let signal = signal.clone();
            async move {
                if span.is_some_and(|(start, _)| start > 0) {
                    let _guard = ActiveRequest(count.clone(), signal.clone());
                    count.fetch_add(1, Ordering::SeqCst);
                    signal.notify_one();
                    // 收到本地响应头后保持子请求 future pending，精确观察其 Drop。
                    let response = request(span).await?;
                    std::future::pending::<()>().await;
                    Ok(response)
                } else {
                    request(span).await
                }
            }
        },
        4,
    ));
    tokio::time::timeout(Duration::from_secs(2), async {
        while active.load(Ordering::SeqCst) < 4 {
            changed.notified().await;
        }
    })
    .await
    .expect("全部子任务应已启动");
    parent.abort();
    assert!(parent.await.unwrap_err().is_cancelled());
    tokio::time::timeout(Duration::from_secs(2), async {
        while active.load(Ordering::SeqCst) != 0 {
            changed.notified().await;
        }
    })
    .await
    .expect("JoinSet drop 应取消所有子请求 future");
    assert!(server.seen.lock().unwrap().iter().all(Option::is_some));
}

#[tokio::test]
async fn first_failure_aborts_siblings_before_single_fallback() {
    let data = payload(1200000);
    let server = Server::start(move |span, stream| range_reply(stream, span, &data));
    let request = server.request();
    let active = Arc::new(AtomicUsize::new(0));
    let changed = Arc::new(tokio::sync::Notify::new());
    let count = active.clone();
    let signal = changed.clone();
    let barrier = Arc::new(tokio::sync::Barrier::new(4));
    let (_, bytes) = tokio::time::timeout(
        Duration::from_secs(2),
        fetch_bytes_ranged_with(
            move |span| {
                let request = request.clone();
                let count = count.clone();
                let signal = signal.clone();
                let barrier = barrier.clone();
                async move {
                    if span.is_some_and(|(start, _)| start > 0) {
                        let _guard = ActiveRequest(count.clone(), signal);
                        count.fetch_add(1, Ordering::SeqCst);
                        barrier.wait().await;
                        if span.unwrap().0 == RANGE_PROBE_BYTES {
                            return Err("fixture failure".into());
                        }
                        std::future::pending::<()>().await;
                    } else if span.is_none() {
                        assert_eq!(count.load(Ordering::SeqCst), 0, "回退前已回收全部子任务");
                    }
                    request(span).await
                }
            },
            4,
        ),
    )
    .await
    .expect("首个失败应立即取消 pending 兄弟任务")
    .unwrap();
    assert_eq!(bytes.len(), 1200000);
    assert_eq!(active.load(Ordering::SeqCst), 0);
    assert_eq!(
        server
            .seen
            .lock()
            .unwrap()
            .iter()
            .filter(|span| span.is_none())
            .count(),
        1
    );
}
