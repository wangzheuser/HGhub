import type { MediaWarmupStatus } from "./api";
import type { BufferingLease, BufferingLeaseRequest } from "./bufferingLease";

export type WarmCandidate = { key: string; id: number; ep: number };
/** 墙钟剩余45s进入片尾窗口；方向/滑动模式只影响非片尾的推测。 */
export const WARM_NEAR_END_SECONDS = 45;
type WarmPosition = { time?: number; duration?: number; rate?: number; nearEnd?: boolean };
export function isWarmNearEnd(s: WarmPosition): boolean {
  if (s.nearEnd !== undefined) return s.nearEnd;
  const { time, duration } = s;
  return typeof time === "number" && typeof duration === "number" && Number.isFinite(time) && Number.isFinite(duration)
    && time >= 0 && duration > time && (duration - time) / Math.max(.25, s.rate || 1) <= WARM_NEAR_END_SECONDS;
}
export type WarmSnapshot = {
  active: boolean; currentUrl: string; candidate: WarmCandidate | null;
  policy: string; time: number; duration: number; buffered: number; rate: number;
  waiting: boolean; seeking: boolean; foreground: boolean;
  nearEnd?: boolean;
  /** 同一候选内单调递增的真实 pointer/focus 意图序号；取消传 undefined，timeupdate 不递增。 */
  intentVersion?: number;
};
type DestinationInput = {
  id: number; ep: number; episodes: number[]; videoIds: number[]; index: number;
  epMap: Record<number, number>;
};
/** ended 用 false（剧终停止），Next 用 true（剧终去下一部）；均不受滑动方向影响。 */
export function nextDestination(input: DestinationInput & { allowNextVideo?: boolean }): WarmCandidate | null {
  const { id, ep, episodes, videoIds, index, epMap, allowNextVideo = false } = input;
  if (episodes.includes(ep + 1)) return { key: `${id}:${ep + 1}`, id, ep: ep + 1 };
  const next = allowNextVideo ? videoIds[index + 1] : undefined;
  return next ? { key: `${next}:${epMap[next] || 1}`, id: next, ep: epMap[next] || 1 } : null;
}
export function chooseWarmCandidate(input: DestinationInput & WarmPosition & {
  swipeEp: boolean; direction: number; intentEp?: number;
}): WarmCandidate | null {
  const { id, ep, episodes, videoIds, index, epMap, swipeEp, direction, intentEp } = input;
  const dir = direction < 0 ? -1 : 1;
  if (intentEp && intentEp !== ep && episodes.includes(intentEp)) return { key: `${id}:${intentEp}`, id, ep: intentEp };
  if (isWarmNearEnd(input)) {
    const successor = nextDestination({ ...input, allowNextVideo: false });
    if (successor) return successor;
  }
  if (swipeEp && episodes.includes(ep + dir)) return { key: `${id}:${ep + dir}`, id, ep: ep + dir };
  const next = videoIds[index + dir];
  if (next) return { key: `${next}:${epMap[next] || 1}`, id: next, ep: epMap[next] || 1 };
  return null;
}
/** 8/4 秒墙钟余量迟滞；尾部全部缓冲视为足够，不把 ended 当健康。 */
export function mediaBudget(previous: boolean, s: Pick<WarmSnapshot, "time" | "duration" | "buffered" | "rate" | "waiting" | "seeking">): boolean {
  if (s.waiting || s.seeking || s.duration <= s.time) return false;
  const rate = Math.max(.25, s.rate || 1);
  return (s.buffered - s.time) / rate >= Math.min(previous ? 4 : 8, (s.duration - s.time) / rate) - .05;
}

export function bufferedWallSeconds(s: Pick<WarmSnapshot, "time" | "buffered" | "rate">): number {
  return Math.max(0, (s.buffered - s.time) / Math.max(.25, s.rate || 1));
}
/** 仅足够前向余量时让槽；20s进入，8s留给当前播放，单次硬上限12s。 */
export function bufferingLeaseMs(s: WarmSnapshot, ready = false): number {
  if (!s.active || !s.currentUrl || !s.candidate || s.waiting || s.seeking || s.foreground || ready || s.duration <= s.time) return 0;
  const ahead = bufferedWallSeconds(s);
  return Number.isFinite(ahead) && ahead >= 20 ? Math.min(12_000, (ahead - 8) * 1000) : 0;
}

/** 可回传调用方缓存的真实绝对到期时刻；仅返回 string 时最多复用60s。 */
export type WarmResolution = string | { url: string; expiresAt: number };
type Dependencies = {
  resolve: (candidate: WarmCandidate) => Promise<WarmResolution>;
  prepare?: (url: string) => Promise<boolean>;
  warm: (url: string) => Promise<boolean>;
  status: (url: string) => Promise<MediaWarmupStatus>;
  cancel: (url: string) => Promise<unknown>;
  trace?: (event: string, data: Record<string, unknown>) => void;
  isForeground?: () => boolean;
  acquireBufferingLease?: (request: BufferingLeaseRequest) => BufferingLease | null;
  now?: () => number;
  schedule?: (fn: () => void, ms: number) => ReturnType<typeof setTimeout>;
  clear?: (timer: ReturnType<typeof setTimeout>) => void;
};
/** 单槽：轻地址阶段跨缓冲波动存活；busy 让路不计失败，媒体就绪只认真实状态。 */
export function createWarmupScheduler(deps: Dependencies) {
  const now = deps.now ?? Date.now, schedule = deps.schedule ?? setTimeout, clear = deps.clear ?? clearTimeout;
  let snapshot: WarmSnapshot;
  let generation = 0, identity = "", url = "", heavy = false, queued = false, ready = false;
  let running = false, halted = false, failures = 0, lightFailures = 0, prepared = false;
  let checks = 0, polls = 0, readyUntil = 0, resolvedUntil = 0, waitBegan: number | undefined, jobBegan: number | undefined;
  let failedGeneration: number | undefined;
  let playlistRefreshes = 0;
  let windowVersion = 0, windowOpen = true, nearEndUsed = false, unsupported = false;
  let windowReason: "initial" | "near_end" | "intent" = "initial";
  let windowHadReady = false, needsRecovery = false, lastIntentVersion: number | undefined;
  let mediaEpoch = 0, leaseAttempts = 0, leaseCooldownUntil = 0, mediaBlockedUntil = 0;
  type LeaseSlot = { handle: BufferingLease | null; generation: number; ended: boolean };
  let leaseSlot: LeaseSlot | undefined;
  let cancellation: Promise<unknown> = Promise.resolve();
  let timer: ReturnType<typeof setTimeout> | undefined;
  const foreground = () => deps.isForeground?.() ?? snapshot.foreground;
  const trace = (event: string, data: Record<string, unknown> = {}) => deps.trace?.(event, { generation, candidate: snapshot?.candidate?.key, ...data });
  const clearTimer = () => { if (timer !== undefined) clear(timer); timer = undefined; };
  const cancelUrl = (previous: string) => {
    if (!previous || previous === snapshot.currentUrl) return;
    const request = deps.cancel(previous);
    cancellation = Promise.all([cancellation.catch(() => {}), request]).then(() => {});
    void cancellation.catch(() => {});
    trace("warm.cancel");
  };
  const cancel = () => {
    mediaEpoch++;
    cancelUrl(url);
    queued = false; jobBegan = undefined; polls = 0;
  };
  const halt = (reason: string) => {
    halted = true;
    clearTimer(); cancel();
    if (leaseSlot) endLease(leaseSlot, "exhausted");
    trace("warm.exhausted", { reason });
  };
  const abortLeaseReasons = new Set(["timeout", "low_buffer", "waiting", "seek", "seek_preview", "external_pause", "external_resume"]);
  const endLease = (slot: LeaseSlot, reason: string) => {
    if (slot.ended) return;
    slot.ended = true;
    if (leaseSlot === slot) leaseSlot = undefined;
    if (slot.handle && slot.generation === generation) {
      leaseCooldownUntil = Math.max(leaseCooldownUntil, now() + 30_000);
      if (!ready && abortLeaseReasons.has(reason)) {
        // 精确取消先入队，再由租约恢复当前填充；不等待网络ACK延长12s租约。
        mediaBlockedUntil = leaseCooldownUntil;
        cancel(); clearTimer();
        if (snapshot.active && !halted) later();
      }
      trace("warm.lease.release", { reason, ahead_s: bufferedWallSeconds(snapshot) });
    }
    slot.handle?.release(reason);
  };
  const maybeLease = () => {
    if (!deps.acquireBufferingLease || leaseSlot || ready || halted || leaseAttempts >= 2 || now() < leaseCooldownUntil) return;
    const maxMs = bufferingLeaseMs({ ...snapshot, foreground: foreground() }, ready);
    if (!maxMs) return;
    const slot: LeaseSlot = { handle: null, generation, ended: false };
    leaseSlot = slot;
    let handle: BufferingLease | null;
    try { handle = deps.acquireBufferingLease({ maxMs, onEnd: reason => endLease(slot, reason) }); }
    catch { if (leaseSlot === slot) leaseSlot = undefined; return; }
    if (!handle || slot.ended) { if (leaseSlot === slot) leaseSlot = undefined; return; }
    slot.handle = handle;
    leaseAttempts++;
    trace("warm.lease.acquire", { max_ms: maxMs, ahead_s: bufferedWallSeconds(snapshot), attempt: leaseAttempts });
  };
  const later = (ms = 1500) => { clearTimer(); timer = schedule(() => { timer = undefined; void run(); }, ms); };
  const dormant = () => { windowOpen = false; clearTimer(); trace("warm.dormant"); };
  const budgetSpent = () => {
    if (checks >= 80 || (waitBegan !== undefined && now() - waitBegan >= 120_000)) {
      if (windowHadReady && !needsRecovery) dormant(); else halt("admission_budget");
      return true;
    }
    if (queued && (polls >= 30 || (jobBegan !== undefined && now() - jobBegan >= 45_000))) {
      halt("media_budget"); return true;
    }
    return false;
  };
  const openWindow = (reason: "near_end" | "intent") => {
    clearTimer();
    // 新意图不延长旧下载的45s预算；先精确取消，在新 warm 前等 ACK。
    if (running || queued) cancel(); else mediaEpoch++;
    if (leaseSlot) endLease(leaseSlot, "window");
    windowVersion++; windowReason = reason; windowOpen = true; ready = false; halted = false;
    checks = 0; polls = 0; failures = 0; lightFailures = 0; playlistRefreshes = 0;
    waitBegan = undefined; jobBegan = undefined; failedGeneration = undefined;
    windowHadReady = false; needsRecovery = false;
    // 不重置 leaseAttempts/cooldown：同候选仍然最多两次租约。
    trace("warm.window", { reason });
  };
  const run = async () => {
    if (running || !snapshot?.active || !snapshot.candidate || ready || halted || !windowOpen) return;
    if (foreground()) { later(); return; }
    const gen = generation, version = windowVersion;
    let epoch = mediaEpoch;
    const lightCurrent = () => gen === generation && version === windowVersion && snapshot.active;
    running = true;
    try {
      if (budgetSpent()) return;
      if (!url || now() >= resolvedUntil) {
        const resolved = await deps.resolve(snapshot.candidate);
        if (!lightCurrent()) return;
        const nextUrl = typeof resolved === "string" ? resolved : resolved.url;
        const expiresAt = typeof resolved === "string" ? now() + 60_000 : resolved.expiresAt;
        if (!nextUrl || !Number.isFinite(expiresAt) || expiresAt <= now()) {
          if (++lightFailures >= 4) halt("resolve_failed"); else later();
          return;
        }
        if (url && nextUrl !== url) {
          cancel(); epoch = mediaEpoch;
          if (leaseSlot) endLease(leaseSlot, "address");
        }
        url = nextUrl; resolvedUntil = Math.min(expiresAt, now() + 60_000); prepared = false;
        // 即使 canonical key 相同，也先 prepare 精确新 URL，旧 raw 清单不算新地址就绪。
        trace("warm.resolved", { resolved: true });
      }
      if (!prepared) {
        // 只准备清单/plan，既不占媒体槽也不受缓冲健康波动影响。
        const result = deps.prepare ? await deps.prepare(url) : true;
        if (!lightCurrent()) return;
        prepared = result;
        trace("warm.prepared", { prepared });
        if (!prepared) { if (++lightFailures >= 4) halt("prepare_failed"); else later(); return; }
        lightFailures = 0;
      }
      if (!heavy || foreground()) { if (heavy) later(); return; }
      waitBegan ??= now();
      // 每个真实意图/片尾窗口仍只有80 checks/120s；成功维护后到期则休眠，不计失败。
      if (budgetSpent()) return;
      checks++;
      if (queued) polls++;
      const status = await deps.status(url);
      if (!lightCurrent() || epoch !== mediaEpoch || !heavy) return;
      trace("warm.status", { state: status.state, backend_generation: status.generation, reason: status.reason });
      ready = status.state === "ready" && status.playlist_ready && status.segment_ready && status.dependencies_ready;
      if (ready) {
        readyUntil = Math.min(now() + 30_000, resolvedUntil); windowHadReady = true; needsRecovery = false;
        queued = false; jobBegan = undefined; polls = 0;
        if (leaseSlot) endLease(leaseSlot, "ready");
        trace("warm.ready"); return;
      }
      needsRecovery = true;
      if (status.state === "failed" && status.reason?.startsWith("unsupported_")) {
        queued = false; halted = true; unsupported = true;
        if (leaseSlot) endLease(leaseSlot, "unsupported"); trace("warm.unsupported", { reason: status.reason }); return;
      }
      if (!status.playlist_ready) {
        // 成功TTL刷新不是失败；异常 prepare 仍最多4次，总检查/墙钟预算也不重置。
        // 同URL刷新保留 queued 首片，新签名URL则已在上方精确取消旧地址并重新 prepare。
        playlistRefreshes++;
        let refreshed = false;
        try { refreshed = deps.prepare ? await deps.prepare(url) : true; } catch { /* 轻刷新按独立有界失败计数 */ }
        if (!lightCurrent() || epoch !== mediaEpoch) return;
        trace("warm.prepared", { prepared: refreshed, refresh: true, attempt: playlistRefreshes });
        if (!refreshed) {
          if (++lightFailures >= 4) { halt("playlist_refresh_failed"); return; }
        } else { lightFailures = 0; prepared = true; }
        later();
        return;
      }
      if (now() < mediaBlockedUntil) { later(); return; }
      maybeLease();
      if (status.state === "busy") { queued = false; jobBegan = undefined; polls = 0; trace("warm.busy"); later(); return; }
      if (status.state === "queued" || (status.state === "ready" && !ready)) {
        queued = true; jobBegan ??= now(); later(); return;
      }
      if (status.state === "failed" && status.generation !== failedGeneration) {
        failedGeneration = status.generation;
        if (++failures >= 4) { halt("failure_budget"); return; }
      }
      // cancelled/idle 是前台抢占与恢复，不消耗失败次数。
      queued = false; jobBegan = undefined; polls = 0;
      await cancellation;
      if (gen !== generation || epoch !== mediaEpoch || !heavy || !snapshot.active) return;
      if (foreground()) { later(); return; }
      const requestedUrl = url;
      queued = true; // 等待 warm RPC ACK 期间也精确归属此槽。
      const accepted = await deps.warm(requestedUrl);
      if (gen !== generation || epoch !== mediaEpoch) {
        if (accepted) cancelUrl(requestedUrl);
        return;
      }
      queued = accepted;
      if (accepted) jobBegan = now();
      if (!heavy || !snapshot.active) { cancel(); return; }
      trace("warm.accepted", { accepted, failures });
      // false 通常是忙，下一轮先只读 status；绝不把 accepted 当 ready。
      later();
    } catch {
      if (gen === generation && epoch === mediaEpoch) {
        trace("warm.retry");
        if (!prepared) { if (++lightFailures >= 4) halt("prepare_error"); else later(); }
        else if (++failures >= 4) halt("rpc_error"); else later();
      }
    } finally {
      running = false;
      // 旧代异步结果不写回新槽；旧请求结束后才派发当前代轻任务。
      if ((gen !== generation || version !== windowVersion || epoch !== mediaEpoch) && snapshot?.active && windowOpen && !halted && !ready && !timer) later(gen !== generation ? 0 : 1500);
    }
  };
  return {
    update(next: WarmSnapshot) {
      const sourceChanged = snapshot?.currentUrl !== next.currentUrl;
      snapshot = next;
      const nextIdentity = next.active && next.candidate ? `${next.currentUrl}|${next.candidate.key}|${next.policy}` : "";
      const nearEnd = isWarmNearEnd(next);
      const intent = Number.isFinite(next.intentVersion) ? next.intentVersion : undefined;
      if (nextIdentity !== identity) {
        clearTimer(); cancel();
        if (leaseSlot) endLease(leaseSlot, next.active ? "candidate" : "inactive");
        generation++; identity = nextIdentity; windowVersion++;
        url = ""; heavy = false; ready = false; halted = false; prepared = false; unsupported = false;
        failures = 0; lightFailures = 0; polls = 0; checks = 0; readyUntil = 0; resolvedUntil = 0;
        waitBegan = undefined; jobBegan = undefined; failedGeneration = undefined; playlistRefreshes = 0;
        windowOpen = true; windowHadReady = false; needsRecovery = false; nearEndUsed = nearEnd; lastIntentVersion = intent;
        windowReason = intent !== undefined ? "intent" : nearEnd ? "near_end" : "initial";
        leaseAttempts = 0; if (sourceChanged) leaseCooldownUntil = 0; mediaBlockedUntil = 0;
      } else {
        const newNearEnd = nearEnd && !nearEndUsed;
        const newIntent = intent !== undefined && (lastIntentVersion === undefined || intent > lastIntentVersion);
        nearEndUsed ||= nearEnd;
        if (newIntent) lastIntentVersion = intent;
        // 片尾阈值往返/取消后重传相同序号均不续命，unsupported 只随真实身份改变解除。
        if (!unsupported && (newIntent || newNearEnd)) openWindow(newIntent ? "intent" : "near_end");
      }
      if (!nextIdentity || halted) return;
      if (ready && now() >= readyUntil) {
        ready = false;
        // 初次命中远离实际切换点时休眠，避免整集每30s轮询并保活上游清单。
        if (windowReason === "initial") dormant();
      }
      const leaseAbort = leaseSlot && (next.waiting || next.seeking || bufferedWallSeconds(next) <= 8);
      if (leaseAbort && leaseSlot) endLease(leaseSlot, next.waiting ? "waiting" : next.seeking ? "seek" : "low_buffer");
      const budget = mediaBudget(heavy, next);
      if (heavy && !budget) { if (!leaseAbort && !ready && windowOpen) cancel(); clearTimer(); }
      heavy = budget;
      if (windowOpen && !timer && !running && !ready) later(300);
    },
    stop() { snapshot = { ...snapshot, active: false }; clearTimer(); cancel(); if (leaseSlot) endLease(leaseSlot, "stop"); generation++; identity = ""; },
  };
}
