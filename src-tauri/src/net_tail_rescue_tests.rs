//! 仅 loopback HTTP + 私有请求 future 门控。没有上游调用，也没有秒级固定 sleep。
use super::ranged_tests::{payload, range_reply, Server};
use super::*;
use std::future::Future;
use std::io::{Read, Write};
use std::pin::Pin;
use std::sync::atomic::AtomicUsize;
use tokio::sync::{Notify, Semaphore};

type Span = Option<(u64, u64)>;
type ResponseFuture = Pin<Box<dyn Future<Output = Result<reqwest::Response, String>> + Send>>;

#[derive(Clone, Copy)]
enum RetryMode {
    Success,
    TransportFailure,
    Http(u16),
    WrongRange,
    Gated,
    NoStall,
}

struct Activity {
    active: AtomicUsize,
    peak: AtomicUsize,
    original_active: AtomicUsize,
    original_dropped: AtomicBool,
    retry_dropped: AtomicBool,
    attempts: AtomicUsize,
    calls: Mutex<Vec<Span>>,
    changed: Notify,
    original_entered: Semaphore,
    retry_entered: Semaphore,
    retry_release: Semaphore,
}

// Semaphore 没有 Default；显式构造，所有 Drop/取消路径共享同一计数观察器。
impl Activity {
    fn new() -> Self {
        Self {
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            original_active: AtomicUsize::new(0),
            original_dropped: AtomicBool::new(false),
            retry_dropped: AtomicBool::new(false),
            attempts: AtomicUsize::new(0),
            calls: Mutex::new(Vec::new()),
            changed: Notify::new(),
            original_entered: Semaphore::new(0),
            retry_entered: Semaphore::new(0),
            retry_release: Semaphore::new(0),
        }
    }
    async fn entered(signal: &Semaphore) {
        tokio::time::timeout(Duration::from_secs(2), signal.acquire())
            .await
            .unwrap()
            .unwrap()
            .forget();
    }
    async fn idle(&self) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let changed = self.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if self.active.load(Ordering::SeqCst) == 0 {
                    break;
                }
                changed.await;
            }
        })
        .await
        .expect("全部 request future 都必须 Drop");
    }
}

struct ActiveFuture {
    state: Arc<Activity>,
    original: bool,
    retry: bool,
}
impl ActiveFuture {
    fn enter(state: Arc<Activity>, original: bool, retry: bool) -> Self {
        let count = state.active.fetch_add(1, Ordering::SeqCst) + 1;
        state.peak.fetch_max(count, Ordering::SeqCst);
        if original {
            state.original_active.fetch_add(1, Ordering::SeqCst);
        }
        Self {
            state,
            original,
            retry,
        }
    }
}
impl Drop for ActiveFuture {
    fn drop(&mut self) {
        if self.original {
            self.state.original_active.fetch_sub(1, Ordering::SeqCst);
            self.state.original_dropped.store(true, Ordering::SeqCst);
        }
        if self.retry {
            self.state.retry_dropped.store(true, Ordering::SeqCst);
        }
        self.state.active.fetch_sub(1, Ordering::SeqCst);
        self.state.changed.notify_waiters();
    }
}

struct Harness {
    server: Server,
    data: Arc<Vec<u8>>,
    state: Arc<Activity>,
    target: (u64, u64),
    mode: RetryMode,
}
impl Harness {
    fn new(mode: RetryMode, short_last: bool) -> Self {
        let data = payload(1_200_001);
        let chunk = (data.len() as u64 - RANGE_PROBE_BYTES).div_ceil(4);
        let start = RANGE_PROBE_BYTES + if short_last { chunk * 3 } else { 0 };
        let target = (start, (start + chunk - 1).min(data.len() as u64 - 1));
        let wire_attempts = Arc::new(AtomicUsize::new(0));
        let attempts = wire_attempts.clone();
        let body = data.clone();
        let server = Server::start(move |span, stream| {
            if span == Some(target) && attempts.fetch_add(1, Ordering::SeqCst) > 0 {
                match mode {
                    RetryMode::Http(status) => {
                        let _ = write!(stream, "HTTP/1.1 {status} fixture\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                        return;
                    }
                    RetryMode::WrongRange => {
                        let (start, end) = target;
                        let len = (end - start + 1) as usize;
                        let _ = write!(stream, "HTTP/1.1 206 fixture\r\nContent-Length: {len}\r\nContent-Range: bytes {}-{end}/{}\r\nConnection: close\r\n\r\n", start + 1, body.len());
                        let _ = stream.write_all(&body[start as usize..=end as usize]);
                        return;
                    }
                    _ => {}
                }
            }
            range_reply(stream, span, &body);
        });
        Self {
            server,
            data,
            state: Arc::new(Activity::new()),
            target,
            mode,
        }
    }

    fn request(&self) -> impl Fn(Span) -> ResponseFuture + Clone + Send + Sync + 'static {
        let request = self.server.request();
        let state = self.state.clone();
        let target = self.target;
        let mode = self.mode;
        move |span| -> ResponseFuture {
            let request = request.clone();
            let state = state.clone();
            Box::pin(async move {
                state.calls.lock().unwrap().push(span);
                let target_attempt =
                    (span == Some(target)).then(|| state.attempts.fetch_add(1, Ordering::SeqCst));
                let original = target_attempt == Some(0);
                let retry = target_attempt.is_some_and(|attempt| attempt > 0);
                let _live = ActiveFuture::enter(state.clone(), original, retry);
                if original {
                    let response = request(span).await?;
                    state.original_entered.add_permits(1);
                    if !matches!(mode, RetryMode::NoStall) {
                        std::future::pending::<()>().await;
                    }
                    return Ok(response);
                }
                if retry {
                    assert!(
                        state.original_dropped.load(Ordering::SeqCst),
                        "救援前必须回收旧 future"
                    );
                    assert_eq!(
                        state.original_active.load(Ordering::SeqCst),
                        0,
                        "不得与原尾并发"
                    );
                    state.retry_entered.add_permits(1);
                    if matches!(mode, RetryMode::TransportFailure) {
                        return Err("fixture rescue transport failure".into());
                    }
                    if matches!(mode, RetryMode::Gated) {
                        state.retry_release.acquire().await.unwrap().forget();
                    }
                }
                request(span).await
            })
        }
    }
}

fn quick_policy() -> TailPolicy {
    TailPolicy {
        floor: Duration::from_millis(30),
        phase_budget: Duration::from_secs(2),
        ..TailPolicy::default()
    }
}

#[test]
fn tail_threshold_uses_only_normal_successes_and_does_not_scale_down_small_pending() {
    let policy = TailPolicy::default();
    let normal = 300_000;
    assert!(policy
        .threshold(normal, normal, &[(normal, Duration::from_secs(1))])
        .is_none());
    let samples = [
        (normal, Duration::from_secs(4)),
        (normal - 1, Duration::from_secs(6)),
        (RANGE_PROBE_BYTES, Duration::from_millis(1)),
        (10, Duration::from_millis(1)),
    ];
    // normal-1 不小于小 pending，两个正常块中位5s，阈值7.5s；probe/短片不污染。
    assert_eq!(
        policy.threshold(10, normal, &samples),
        Some(Duration::from_millis(7500))
    );
    assert_eq!(
        policy.threshold(RANGE_PROBE_BYTES, normal, &samples),
        Some(Duration::from_millis(7500))
    );
    assert_eq!(policy.threshold(normal, normal, &samples), None);
    assert_eq!(policy.phase_budget, RANGED_TIMEOUT);
}

#[tokio::test]
async fn sole_tail_is_replaced_once_after_old_future_drop_and_preserves_other_bytes() {
    for short_last in [false, true] {
        let harness = Harness::new(RetryMode::Success, short_last);
        let trace = Arc::new(DownloadTrace::new());
        let (_, bytes) =
            fetch_bytes_ranged_with_policy(harness.request(), 4, trace.clone(), quick_policy())
                .await
                .unwrap();
        assert_eq!(&bytes, harness.data.as_ref());
        harness.state.idle().await;
        let calls = harness.state.calls.lock().unwrap();
        assert_eq!(calls.len(), 6, "首探+4区间+一次同区间救援");
        assert_eq!(
            calls
                .iter()
                .filter(|span| **span == Some(harness.target))
                .count(),
            2
        );
        assert!(calls.iter().all(Option::is_some));
        assert_eq!(trace.requests.load(Ordering::Relaxed), 6);
        assert_eq!(trace.tail_rescue.load(Ordering::Relaxed), 1);
        assert_eq!(trace.fallback.load(Ordering::Relaxed), 0);
        assert_eq!(trace.recovery_kind(), "tail_range");
        assert!(harness.state.original_dropped.load(Ordering::SeqCst));
        assert!(harness.state.peak.load(Ordering::SeqCst) <= 4);
    }
}

#[tokio::test]
async fn failed_rescue_never_spends_another_full_fallback() {
    for mode in [
        RetryMode::TransportFailure,
        RetryMode::Http(403),
        RetryMode::Http(429),
        RetryMode::WrongRange,
    ] {
        let harness = Harness::new(mode, false);
        let trace = Arc::new(DownloadTrace::new());
        assert!(fetch_bytes_ranged_with_policy(
            harness.request(),
            4,
            trace.clone(),
            quick_policy()
        )
        .await
        .is_err());
        harness.state.idle().await;
        assert_eq!(harness.state.attempts.load(Ordering::SeqCst), 2);
        assert_eq!(trace.tail_rescue.load(Ordering::Relaxed), 1);
        assert_eq!(trace.fallback.load(Ordering::Relaxed), 0);
        let calls = harness.state.calls.lock().unwrap();
        assert_eq!(calls.len(), 6);
        assert!(calls.iter().all(Option::is_some));
    }
}

#[tokio::test]
async fn rescue_timeout_keeps_the_original_phase_deadline_and_drops_every_future() {
    let harness = Harness::new(RetryMode::Gated, false);
    let trace = Arc::new(DownloadTrace::new());
    let policy = TailPolicy {
        phase_budget: Duration::from_millis(160),
        ..quick_policy()
    };
    let started = Instant::now();
    let error = fetch_bytes_ranged_with_policy(harness.request(), 4, trace.clone(), policy)
        .await
        .unwrap_err();
    assert!(error.contains("超时"));
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "救援不得重开超时/整包窗口"
    );
    harness.state.idle().await;
    assert!(harness.state.original_dropped.load(Ordering::SeqCst));
    assert!(harness.state.retry_dropped.load(Ordering::SeqCst));
    assert_eq!(harness.state.attempts.load(Ordering::SeqCst), 2);
    assert_eq!(trace.fallback.load(Ordering::Relaxed), 0);
    assert!(harness
        .state
        .calls
        .lock()
        .unwrap()
        .iter()
        .all(Option::is_some));
}

#[tokio::test]
async fn parent_cancellation_drops_original_or_rescue_without_more_requests() {
    for during_rescue in [false, true] {
        let harness = Harness::new(RetryMode::Gated, false);
        let trace = Arc::new(DownloadTrace::new());
        let policy = if during_rescue {
            quick_policy()
        } else {
            TailPolicy::default()
        };
        let parent = tokio::spawn(fetch_bytes_ranged_with_policy(
            harness.request(),
            4,
            trace.clone(),
            policy,
        ));
        Activity::entered(&harness.state.original_entered).await;
        if during_rescue {
            Activity::entered(&harness.state.retry_entered).await;
        }
        parent.abort();
        assert!(parent.await.unwrap_err().is_cancelled());
        harness.state.idle().await;
        assert!(harness.state.original_dropped.load(Ordering::SeqCst));
        if during_rescue {
            assert!(harness.state.retry_dropped.load(Ordering::SeqCst));
        }
        assert_eq!(
            harness.state.attempts.load(Ordering::SeqCst),
            if during_rescue { 2 } else { 1 }
        );
        assert!(trace.requests.load(Ordering::Relaxed) <= 6);
        assert_eq!(trace.fallback.load(Ordering::Relaxed), 0);
        assert!(harness
            .state
            .calls
            .lock()
            .unwrap()
            .iter()
            .all(Option::is_some));
    }
}

#[tokio::test]
async fn ordinary_fast_ranges_keep_original_request_count_and_no_rescue() {
    let harness = Harness::new(RetryMode::NoStall, false);
    let trace = Arc::new(DownloadTrace::new());
    let (_, bytes) = fetch_bytes_ranged_with_trace(harness.request(), 4, trace.clone())
        .await
        .unwrap();
    assert_eq!(&bytes, harness.data.as_ref());
    assert_eq!(harness.state.calls.lock().unwrap().len(), 5);
    assert_eq!(trace.requests.load(Ordering::Relaxed), 5);
    assert_eq!(trace.tail_rescue.load(Ordering::Relaxed), 0);
    assert_eq!(trace.fallback.load(Ordering::Relaxed), 0);
    assert_eq!(trace.recovery_kind(), "none");
}

#[tokio::test]
async fn sole_probe_reloads_its_full_validated_range_after_dropping_old_prefix() {
    let data = payload(1_200_001);
    let body = data.clone();
    let probes = Arc::new(AtomicUsize::new(0));
    let count = probes.clone();
    let closed = Arc::new(AtomicBool::new(false));
    let old_closed = closed.clone();
    let server = Server::start(move |span, stream| {
        if span.is_some_and(|(start, _)| start == 0) && count.fetch_add(1, Ordering::SeqCst) == 0 {
            let _ = write!(stream, "HTTP/1.1 206 fixture\r\nContent-Length: {RANGE_PROBE_BYTES}\r\nContent-Range: bytes 0-{}/{}\r\nConnection: close\r\n\r\n", RANGE_PROBE_BYTES - 1, body.len());
            let _ = stream.write_all(&body[..16 * 1024]);
            let _ = stream.flush();
            // 成功前缀允许扇出；剩余正文保持 pending，直至客户端 Drop 关闭该响应。
            let mut byte = [0u8];
            if stream.read(&mut byte).unwrap_or(1) == 0 {
                old_closed.store(true, Ordering::SeqCst);
            }
        } else {
            range_reply(stream, span, &body);
        }
    });
    let request = server.request();
    let trace = Arc::new(DownloadTrace::new());
    let observed = trace.clone();
    let probe_requests = Arc::new(AtomicUsize::new(0));
    let probe_seen = probe_requests.clone();
    let wrapped = move |span: Span| -> ResponseFuture {
        let request = request.clone();
        let trace = observed.clone();
        let count = probe_seen.clone();
        Box::pin(async move {
            if span.is_some_and(|(start, _)| start == 0) && count.fetch_add(1, Ordering::SeqCst) > 0
            {
                assert_eq!(trace.tail_rescue.load(Ordering::Relaxed), 1);
                assert!(
                    trace.probe_read_ms.load(Ordering::Relaxed) > 0,
                    "旧 probe task 的Drop计时器必须先完成"
                );
            }
            request(span).await
        })
    };
    let (_, bytes) = fetch_bytes_ranged_with_policy(wrapped, 4, trace.clone(), quick_policy())
        .await
        .unwrap();
    assert_eq!(&bytes, data.as_ref());
    assert_eq!(trace.requests.load(Ordering::Relaxed), 6);
    assert_eq!(trace.tail_rescue.load(Ordering::Relaxed), 1);
    assert_eq!(trace.fallback.load(Ordering::Relaxed), 0);
    assert_eq!(probes.load(Ordering::SeqCst), 2);
    let seen = server.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 6);
    assert_eq!(
        seen.iter()
            .filter(|span| **span == Some((0, RANGE_PROBE_BYTES - 1)))
            .count(),
        2
    );
    drop(server); // 回收server工作线程后观察旧响应关闭，不用任意sleep轮询。
    assert!(closed.load(Ordering::SeqCst));
}
