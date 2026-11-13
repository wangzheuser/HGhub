//! 单片下载的被动阶段统计；不记录 URL，也不发起额外测速。
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub(super) struct DownloadTrace {
    pub started: Instant,
    pub requests: AtomicUsize,
    pub concurrency: AtomicUsize,
    pub probe_headers_ms: AtomicU64,
    pub probe_read_ms: AtomicU64,
    pub slowest_range_ms: AtomicU64,
    pub fallback: AtomicUsize,
    pub tail_rescue: AtomicUsize,
    pub tail_threshold_ms: AtomicU64,
    pub tail_wait_ms: AtomicU64,
    pub tail_read_ms: AtomicU64,
}

impl DownloadTrace {
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            requests: AtomicUsize::new(0),
            concurrency: AtomicUsize::new(0),
            probe_headers_ms: AtomicU64::new(0),
            probe_read_ms: AtomicU64::new(0),
            slowest_range_ms: AtomicU64::new(0),
            fallback: AtomicUsize::new(0),
            tail_rescue: AtomicUsize::new(0),
            tail_threshold_ms: AtomicU64::new(0),
            tail_wait_ms: AtomicU64::new(0),
            tail_read_ms: AtomicU64::new(0),
        }
    }

    pub fn range_done(&self, started: Instant) {
        self.slowest_range_ms
            .fetch_max(started.elapsed().as_millis() as u64, Ordering::Relaxed);
    }

    pub fn recovery_kind(&self) -> &'static str {
        if self.tail_rescue.load(Ordering::Relaxed) != 0 {
            "tail_range"
        } else if self.fallback.load(Ordering::Relaxed) != 0 {
            "full_get"
        } else {
            "none"
        }
    }
}

/// 每个父下载只写一次摘要；Drop 覆盖父 future 取消，不在逐 chunk 路径写日志。
/// 这里只保存资源散列，不保存 URL；取消时 bytes 为空，不冒充已读正文或线缆字节。
pub(super) struct DownloadSummaryGuard {
    resource: String,
    requested_parts: usize,
    trace: Arc<DownloadTrace>,
    finished: bool,
}

impl DownloadSummaryGuard {
    pub fn new(resource: String, requested_parts: usize, trace: Arc<DownloadTrace>) -> Self {
        Self {
            resource,
            requested_parts,
            trace,
            finished: false,
        }
    }

    fn summary(
        &self,
        bytes: Option<usize>,
        error_kind: Option<&str>,
        cancelled: bool,
    ) -> serde_json::Value {
        let trace = &self.trace;
        serde_json::json!({
            "resource": self.resource,
            "requested_parts": self.requested_parts,
            "concurrency": trace.concurrency.load(Ordering::Relaxed),
            "request_count": trace.requests.load(Ordering::Relaxed),
            "probe_headers_ms": trace.probe_headers_ms.load(Ordering::Relaxed),
            "probe_read_ms": trace.probe_read_ms.load(Ordering::Relaxed),
            "slowest_range_ms": trace.slowest_range_ms.load(Ordering::Relaxed),
            "fallback_count": trace.fallback.load(Ordering::Relaxed),
            "recovery_kind": trace.recovery_kind(),
            "tail_rescue": trace.tail_rescue.load(Ordering::Relaxed) != 0,
            "tail_threshold_ms": trace.tail_threshold_ms.load(Ordering::Relaxed),
            "tail_wait_ms": trace.tail_wait_ms.load(Ordering::Relaxed),
            "tail_read_ms": trace.tail_read_ms.load(Ordering::Relaxed),
            "elapsed_ms": trace.started.elapsed().as_millis() as u64,
            "bytes": bytes,
            "error_kind": error_kind,
            "cancelled": cancelled,
        })
    }

    fn emit(&self, bytes: Option<usize>, error_kind: Option<&str>, cancelled: bool) {
        if crate::playback_log::enabled() {
            crate::playback_log::record(
                "network.download",
                self.summary(bytes, error_kind, cancelled),
            );
        }
    }

    pub fn finish(&mut self, result: &Result<super::MediaBytes, String>) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.emit(
            result.as_ref().ok().map(|(_, bytes)| bytes.len()),
            result
                .as_ref()
                .err()
                .map(|error| crate::playback_log::error_kind(error)),
            false,
        );
    }
}

impl Drop for DownloadSummaryGuard {
    fn drop(&mut self) {
        if !self.finished {
            // 取消摘要是父边界快照；子任务Drop稍后完成的阶段计时不保证已计入。
            self.emit(None, Some("cancelled"), true);
        }
    }
}

/// 父 future 超时/取消或响应失败时也记录已经消耗的阶段时长。
pub(super) struct StageTimer<'a> {
    sink: &'a AtomicU64,
    started: Instant,
}
impl<'a> StageTimer<'a> {
    pub fn since(sink: &'a AtomicU64, started: Instant) -> Self {
        Self { sink, started }
    }
}
impl Drop for StageTimer<'_> {
    fn drop(&mut self) {
        self.sink
            .store(self.started.elapsed().as_millis() as u64, Ordering::Relaxed);
    }
}

/// 只对唯一剩余区间救援一次。测试可降低最小等待，不改变生产 20s 总阶段截止。
#[derive(Clone, Copy)]
pub(super) struct TailPolicy {
    pub floor: Duration,
    pub minimum_samples: usize,
    pub phase_budget: Duration,
}
impl Default for TailPolicy {
    fn default() -> Self {
        Self {
            floor: Duration::from_secs(2),
            minimum_samples: 2,
            phase_budget: super::RANGED_TIMEOUT,
        }
    }
}
impl TailPolicy {
    pub fn threshold(
        &self,
        pending_len: u64,
        normal_chunk: u64,
        samples: &[(u64, Duration)],
    ) -> Option<Duration> {
        let mut completed: Vec<_> = samples
            .iter()
            // probe 不进入 samples；短尾也不污染正常块中位数。小尾/probe 使用大块耗时
            // 作保守上界，不按长度缩小阈值；仅容忍 ceil 划分引入的 <=8 字节差。
            .filter(|(len, _)| len.abs_diff(normal_chunk) <= 8 && *len >= pending_len)
            .map(|(_, elapsed)| *elapsed)
            .collect();
        if completed.len() < self.minimum_samples.max(2) {
            return None;
        }
        completed.sort_unstable();
        let middle = completed.len() / 2;
        let median = if completed.len() % 2 == 0 {
            (completed[middle - 1] + completed[middle]) / 2
        } else {
            completed[middle]
        };
        Some(self.floor.max(median + median / 2))
    }
}

/// 显式探索只能选已支持的有界档位；证据不足时保留调用方的既有默认值。
/// 不自动轮询测速，不因一次慢请求贸然扩大并发或改变全局网络配置。
pub(super) fn parse_parts(value: Option<&str>, fallback: usize) -> usize {
    match value.map(str::trim) {
        Some("1") => 1,
        Some("2") => 2,
        Some("4") => 4,
        Some("8") => 8,
        _ => fallback.clamp(2, 8),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_summary_keeps_counters_without_claiming_body_bytes() {
        let trace = Arc::new(DownloadTrace::new());
        trace.requests.store(5, Ordering::Relaxed);
        trace.concurrency.store(4, Ordering::Relaxed);
        trace.fallback.store(1, Ordering::Relaxed);
        // 只测纯payload，不触碰全局诊断开关或文件日志。
        let summary = DownloadSummaryGuard {
            resource: "r123".into(),
            requested_parts: 4,
            trace,
            finished: true,
        };
        let cancelled = summary.summary(None, Some("cancelled"), true);
        assert_eq!(cancelled["resource"], "r123");
        assert_eq!(cancelled["request_count"], 5);
        assert_eq!(cancelled["concurrency"], 4);
        assert_eq!(cancelled["recovery_kind"], "full_get");
        assert_eq!(cancelled["cancelled"], true);
        assert_eq!(cancelled["error_kind"], "cancelled");
        assert!(cancelled["bytes"].is_null());
        let completed = summary.summary(Some(64), None, false);
        assert_eq!(completed["bytes"], 64);
        assert_eq!(completed["cancelled"], false);
        assert!(completed["error_kind"].is_null());
    }

    #[test]
    fn supported_profiles_are_explicit_and_bounded() {
        for parts in [1, 2, 4, 8] {
            assert_eq!(parse_parts(Some(&parts.to_string()), 8), parts);
        }
        for value in [
            None,
            Some("0"),
            Some("12"),
            Some("16"),
            Some("auto"),
            Some(""),
        ] {
            assert_eq!(parse_parts(value, 8), 8);
        }
        assert_eq!(parse_parts(Some(" 4 "), 8), 4);
        assert_eq!(parse_parts(None, 2), 2);
    }
}
