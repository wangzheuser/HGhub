import type Hls from "hls.js";

export type SeekPrefetch = {
  at: (t: number) => void;
  /** 松手交接最终目标；同片在途不重复派发。 */
  commit: (t: number) => void;
  /** 仅撤销自己发出的原始 URL。 */
  cancel: () => void;
};
export type SeekPrefetchDependencies = {
  prefetch: (url: string) => Promise<boolean>;
  cancel: (url: string) => Promise<unknown>;
  isBuffered?: (time: number) => boolean;
  now?: () => number;
  schedule?: (fn: () => void, ms: number) => ReturnType<typeof setTimeout>;
  clear?: (timer: ReturnType<typeof setTimeout>) => void;
};

/** 本地代理媒体地址还原，原生/远端地址保持不变。 */
export function originalMediaUrl(url: string): string {
  try { return new URL(url).searchParams.get("u") || url; } catch { return url; }
}

/** 只计算覆盖当前时刻的连续缓冲，跨洞的远端缓冲不算健康。 */
export function bufferedEnd(video: HTMLVideoElement, time = video.currentTime): number {
  for (let i = 0; i < video.buffered.length; i++) {
    if (video.buffered.start(i) <= time + 0.05 && video.buffered.end(i) > time) return video.buffered.end(i);
  }
  return 0;
}
export function isTimeBuffered(video: HTMLVideoElement, time: number): boolean {
  for (let i = 0; i < video.buffered.length; i++) {
    if (video.buffered.start(i) <= time && video.buffered.end(i) > time + 0.15) return true;
  }
  return false;
}
export function healthyBuffer(time: number, duration: number, end: number): boolean {
  const remaining = duration - time;
  return remaining > 0 && end - time >= Math.min(8, remaining) - 0.05;
}

/** 单一目标、120ms去抖；缓冲命中不发RPC，同一连续目标失败最多显式重试一次。 */
export function createSeekPrefetch(getHls: () => Hls | null, deps: SeekPrefetchDependencies): SeekPrefetch {
  const now = deps.now ?? Date.now, schedule = deps.schedule ?? setTimeout, clear = deps.clear ?? clearTimeout;
  let pendingUrl = "", firedUrl = "", attemptUrl = "";
  let attempts = 0, lastAttempt = -Infinity, generation = 0, pendingTime = 0;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let cancellation: Promise<unknown> = Promise.resolve(), cancelPending = 0, cancelledGeneration = -1;
  const clearTimer = () => { if (timer !== undefined) clear(timer); timer = undefined; };
  const cancelUrl = (url: string) => {
    if (!url) return;
    cancelPending++;
    const request = deps.cancel(url).catch(() => {}).finally(() => { cancelPending--; });
    cancellation = Promise.all([cancellation.catch(() => {}), request]);
  };
  const targetUrl = (t: number) => {
    const hls = getHls();
    if (!hls || !Number.isFinite(t)) return "";
    const details = hls.levels[hls.currentLevel]?.details ?? hls.levels.find(l => l.details)?.details;
    const fragments = details?.fragments;
    if (!fragments?.length) return "";
    const last = fragments[fragments.length - 1];
    const frag = fragments.find(f => t >= f.start && t < f.start + (f.duration || 0.001))
      ?? (t >= last.start ? last : fragments[0]);
    if (!frag?.url || !frag.url.includes("?u=")) return "";
    return originalMediaUrl(frag.url);
  };
  const buffered = (t: number) => {
    if (!Number.isFinite(t) || !deps.isBuffered?.(t)) return false;
    clearTimer(); pendingUrl = "";
    // 已派发的同片可能正在被当前播放复用；缓冲快路径不额外发取消或换代RPC。
    return true;
  };
  const fire = () => {
    clearTimer();
    const url = pendingUrl;
    if (!url || buffered(pendingTime) || url === firedUrl) return;
    if (url !== attemptUrl) { attemptUrl = url; attempts = 0; lastAttempt = -Infinity; }
    if (attempts >= 2) return;
    if (now() - lastAttempt < 300) { timer = schedule(fire, 300 - (now() - lastAttempt)); return; }
    attempts++; lastAttempt = now(); firedUrl = url;
    const requestGeneration = ++generation;
    const dispatch = () => {
      if (requestGeneration !== generation || pendingUrl !== url || buffered(pendingTime)) return;
      let request: Promise<boolean>;
      try { request = deps.prefetch(url); } catch { request = Promise.resolve(false); }
      void request.catch(() => false).then(accepted => {
        if (requestGeneration !== generation) {
          // cancel已排在原prefetch之后；晚ACK不再追加cancel误伤重挂的新同片请求。
          if (accepted && requestGeneration > cancelledGeneration && firedUrl !== url) cancelUrl(url);
          return;
        }
        if (!accepted && firedUrl === url) firedUrl = "";
      });
    };
    if (cancelPending) void cancellation.then(dispatch); else dispatch();
  };
  return {
    at(t) {
      if (buffered(t)) return;
      const url = targetUrl(t);
      if (!url || (url === pendingUrl && (timer !== undefined || firedUrl === url))) return;
      pendingTime = t; pendingUrl = url; clearTimer(); timer = schedule(fire, 120);
    },
    commit(t) {
      if (buffered(t)) return;
      const url = targetUrl(t);
      if (!url) { clearTimer(); pendingUrl = ""; return; }
      pendingTime = t; pendingUrl = url; fire();
    },
    cancel() {
      clearTimer(); cancelledGeneration = generation; generation++;
      const previous = firedUrl;
      pendingUrl = ""; firedUrl = ""; attemptUrl = ""; attempts = 0; lastAttempt = -Infinity;
      cancelUrl(previous);
    },
  };
}
