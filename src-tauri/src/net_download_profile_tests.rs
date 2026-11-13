//! 仅本地 fixture 的下载档位、首探重叠与阶段指标回归。
use super::ranged_tests::{payload, range_reply, Server};
use super::*;
use std::io::Write;
use std::sync::Condvar;

#[tokio::test]
async fn single_get_profile_has_one_request_and_preserves_bytes() {
    let data = payload(1_200_000);
    let body = data.clone();
    let server = Server::start(move |span, stream| range_reply(stream, span, &body));
    let trace = Arc::new(DownloadTrace::new());
    let (_, bytes) = fetch_bytes_ranged_with_trace(server.request(), 1, trace.clone())
        .await
        .unwrap();
    assert_eq!(&bytes, data.as_ref());
    assert_eq!(*server.seen.lock().unwrap(), vec![None]);
    assert_eq!(trace.requests.load(Ordering::Relaxed), 1);
    assert_eq!(trace.concurrency.load(Ordering::Relaxed), 1);
    assert_eq!(trace.fallback.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn first_body_and_other_ranges_overlap_without_raising_the_budget() {
    let data = payload(1_200_000);
    let body = data.clone();
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let child_arrived = gate.clone();
    let stalled = Arc::new(AtomicBool::new(false));
    let timed_out = stalled.clone();
    let server = Server::start(move |span, stream| {
        if span.is_some_and(|(start, _)| start == 0) {
            let end = RANGE_PROBE_BYTES as usize;
            let header = format!("HTTP/1.1 206 fixture\r\nContent-Length: {end}\r\nContent-Range: bytes 0-{}/{}\r\nConnection: close\r\n\r\n", end - 1, body.len());
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(&body[..16 * 1024]);
            let _ = stream.flush();
            let (lock, signal) = &*child_arrived;
            let seen = lock.lock().unwrap();
            let (_seen, timeout) = signal
                .wait_timeout_while(seen, Duration::from_secs(1), |seen| !*seen)
                .unwrap();
            timed_out.store(timeout.timed_out(), Ordering::Relaxed);
            let _ = stream.write_all(&body[16 * 1024..end]);
        } else {
            let (lock, signal) = &*child_arrived;
            *lock.lock().unwrap() = true;
            signal.notify_all();
            range_reply(stream, span, &body);
        }
    });
    let trace = Arc::new(DownloadTrace::new());
    let (_, bytes) = fetch_bytes_ranged_with_trace(server.request(), 4, trace.clone())
        .await
        .unwrap();
    assert_eq!(&bytes, data.as_ref());
    assert!(
        !stalled.load(Ordering::Relaxed),
        "子区间必须在首块体收完前到达"
    );
    assert_eq!(trace.concurrency.load(Ordering::Relaxed), 4);
    assert_eq!(trace.requests.load(Ordering::Relaxed), 5);
    assert_eq!(trace.fallback.load(Ordering::Relaxed), 0);
    let mut spans: Vec<_> = server
        .seen
        .lock()
        .unwrap()
        .iter()
        .map(|span| span.unwrap())
        .collect();
    spans.sort_unstable();
    let mut expected = 0;
    for (start, end) in spans {
        assert_eq!(start, expected);
        expected = end + 1;
    }
    assert_eq!(expected, data.len() as u64, "每个字节恰好覆盖一次");
}

#[tokio::test]
async fn tiny_object_reports_actual_single_request_not_requested_eight() {
    let body = payload(16);
    let server = Server::start(move |span, stream| range_reply(stream, span, &body));
    let trace = Arc::new(DownloadTrace::new());
    let (_, bytes) = fetch_bytes_ranged_with_trace(server.request(), 8, trace.clone())
        .await
        .unwrap();
    assert_eq!(bytes.len(), 16);
    assert_eq!(trace.requests.load(Ordering::Relaxed), 1);
    assert_eq!(trace.concurrency.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn partially_read_probe_failure_cancels_children_before_one_full_fallback() {
    let body = payload(1_200_000);
    let server = Server::start(move |span, stream| {
        if span.is_some_and(|(start, _)| start == 0) {
            let header = format!("HTTP/1.1 206 fixture\r\nContent-Length: 65536\r\nContent-Range: bytes 0-65535/{}\r\nConnection: close\r\n\r\n", body.len());
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(&body[..32 * 1024]);
        } else {
            range_reply(stream, span, &body);
        }
    });
    let trace = Arc::new(DownloadTrace::new());
    let (_, bytes) = fetch_bytes_ranged_with_trace(server.request(), 8, trace.clone())
        .await
        .unwrap();
    assert_eq!(bytes.len(), 1_200_000);
    assert_eq!(trace.fallback.load(Ordering::Relaxed), 1);
    let requests = server.seen.lock().unwrap();
    assert_eq!(requests.iter().filter(|span| span.is_none()).count(), 1);
    assert!(requests.len() <= 10, "最多首探+8区间+一次整包回退");
}
