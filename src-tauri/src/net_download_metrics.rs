//! 单片下载的被动阶段统计；不记录 URL，也不发起额外测速。
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
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
