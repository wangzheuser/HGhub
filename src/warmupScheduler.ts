import type { MediaWarmupStatus } from "./api";
import type { BufferingLease, BufferingLeaseRequest } from "./bufferingLease";

export type WarmCandidate = { key: string; id: number; ep: number };
export type WarmSnapshot = {
  active: boolean; currentUrl: string; candidate: WarmCandidate | null;
  policy: string; time: number; duration: number; buffered: number; rate: number;
  waiting: boolean; seeking: boolean; foreground: boolean;
};
export function chooseWarmCandidate(input: {
  id: number; ep: number; episodes: number[]; videoIds: number[]; index: number;
  epMap: Record<number, number>; swipeEp: boolean; direction: number; intentEp?: number;
}): WarmCandidate | null {
  const { id, ep, episodes, videoIds, index, epMap, swipeEp, direction, intentEp } = input;
  const dir = direction < 0 ? -1 : 1;
  if (intentEp && intentEp !== ep && episodes.includes(intentEp)) return { key: `${id}:${intentEp}`, id, ep: intentEp };
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

type Dependencies = {
  resolve: (candidate: WarmCandidate) => Promise<string>;
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
  let checks = 0, polls = 0, readyUntil = 0, waitBegan: number | undefined, jobBegan: number | undefined;
  let failedGeneration: number | undefined;
  let playlistRefreshes = 0;
  let mediaEpoch = 0, leaseAttempts = 0, leaseCooldownUntil = 0, mediaBlockedUntil = 0;
  type LeaseSlot = { handle: BufferingLease | null; generation: number; ended: boolean };
  let leaseSlot: LeaseSlot | undefined;
  let cancellation: Promise<unknown> = Promise.resolve();
  let timer: ReturnType<typeof setTimeout> | undefined;
  const foreground = () => deps.isForeground?.() ?? snapshot.foreground;
  const trace = (event: string, data: Record<string, unknown> = {}) => deps.trace?.(event, { generation, candidate: snapshot?.candidate?.key, ...data });
  const clearTimer = () => { if (timer !== undefined) clear(timer); timer = undefined; };
  const cancel = () => {
    mediaEpoch++;
    if (url && url !== snapshot.currentUrl) {
      const previous = url;
      const request = deps.cancel(previous);
      cancellation = Promise.all([cancellation.catch(() => {}), request]).then(() => {});
      void cancellation.catch(() => {});
      trace("warm.cancel");
    }
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
  const run = async () => {
    if (running || !snapshot?.active || !snapshot.candidate || ready || halted) return;
    if (foreground()) { later(); return; }
    const gen = generation, epoch = mediaEpoch;
    running = true;
    try {
      if (!url) {
        const resolved = await deps.resolve(snapshot.candidate);
        if (gen !== generation) return;
        url = resolved;
        trace("warm.resolved", { resolved: !!url });
      }
      if (!url) { if (++lightFailures >= 4) halt("resolve_failed"); else later(); return; }
      if (!prepared) {
        // 只准备清单/plan，既不占媒体槽也不受缓冲健康波动影响。
        const result = deps.prepare ? await deps.prepare(url) : true;
        if (gen !== generation) return;
        prepared = result;
        trace("warm.prepared", { prepared });
        if (!prepared) { if (++lightFailures >= 4) halt("prepare_failed"); else later(); return; }
        lightFailures = 0;
      }
      if (!heavy || foreground()) { if (heavy) later(); return; }
      waitBegan ??= now();
      // 忙等可跨越一次真实冷片下载；只有真正 admitted 后才计 45s 下载/依赖预算。
      if (checks >= 80 || now() - waitBegan >= 120_000) { halt("admission_budget"); return; }
      if (queued && (polls >= 30 || (jobBegan !== undefined && now() - jobBegan >= 45_000))) { halt("media_budget"); return; }
      checks++;
      if (queued) polls++;
      const status = await deps.status(url);
      if (gen !== generation || epoch !== mediaEpoch || !heavy || !snapshot.active) return;
      trace("warm.status", { state: status.state, backend_generation: status.generation, reason: status.reason });
      ready = status.state === "ready" && status.playlist_ready && status.segment_ready && status.dependencies_ready;
      if (ready) { readyUntil = now() + 30_000; if (leaseSlot) endLease(leaseSlot, "ready"); trace("warm.ready"); return; }
      if (status.state === "failed" && status.reason?.startsWith("unsupported_")) {
        queued = false; halted = true; if (leaseSlot) endLease(leaseSlot, "unsupported"); trace("warm.unsupported", { reason: status.reason }); return;
      }
      if (!status.playlist_ready) {
        // 原文TTL可能短于120s等候预算；只刷新同URL轻清单，不撤销已queued首片。
        if (playlistRefreshes >= 4) { halt("playlist_refresh_budget"); return; }
        playlistRefreshes++;
        let refreshed = false;
        try { refreshed = deps.prepare ? await deps.prepare(url) : true; } catch { /* 轻刷新按独立有界失败计数 */ }
        if (gen !== generation || epoch !== mediaEpoch || !snapshot.active) return;
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
        if (accepted && requestedUrl !== snapshot.currentUrl) void deps.cancel(requestedUrl);
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
      if ((gen !== generation || epoch !== mediaEpoch) && snapshot?.active && !halted && !timer) later(gen !== generation ? 0 : 1500);
    }
  };
  return {
    update(next: WarmSnapshot) {
      const sourceChanged = snapshot?.currentUrl !== next.currentUrl;
      snapshot = next;
      const nextIdentity = next.active && next.candidate ? `${next.currentUrl}|${next.candidate.key}|${next.policy}` : "";
      if (nextIdentity !== identity) {
        clearTimer(); cancel();
        if (leaseSlot) endLease(leaseSlot, next.active ? "candidate" : "inactive");
        generation++; identity = nextIdentity;
        url = ""; heavy = false; ready = false; halted = false; prepared = false;
        failures = 0; lightFailures = 0; polls = 0; checks = 0; readyUntil = 0;
        waitBegan = undefined; jobBegan = undefined; failedGeneration = undefined; playlistRefreshes = 0;
        leaseAttempts = 0; if (sourceChanged) leaseCooldownUntil = 0; mediaBlockedUntil = 0;
      }
      if (!nextIdentity || halted) return;
      if (ready && now() >= readyUntil) { ready = false; polls = 0; checks = 0; waitBegan = undefined; jobBegan = undefined; }
      const leaseAbort = leaseSlot && (next.waiting || next.seeking || bufferedWallSeconds(next) <= 8);
      if (leaseAbort && leaseSlot) endLease(leaseSlot, next.waiting ? "waiting" : next.seeking ? "seek" : "low_buffer");
      const budget = mediaBudget(heavy, next);
      if (heavy && !budget) { if (!leaseAbort) cancel(); clearTimer(); }
      heavy = budget;
      if (!timer && !running && !ready) later(300);
    },
    stop() { snapshot = { ...snapshot, active: false }; clearTimer(); cancel(); if (leaseSlot) endLease(leaseSlot, "stop"); generation++; identity = ""; },
  };
}
