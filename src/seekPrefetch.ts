import type Hls from "hls.js";
import { api } from "./api";

export type SeekPrefetch = {
  at: (t: number) => void;
  /** 松手立即交接最终目标；保留在途同片下载供前台复用。 */
  commit: (t: number) => void;
  /** 取消计时器及已发出的、URL 匹配的服务端推测。 */
  cancel: () => void;
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

/** 单一目标、120ms 去抖；提交时保留同片，取消/换源时精确撤销旧任务。 */
export function createSeekPrefetch(getHls: () => Hls | null): SeekPrefetch {
  let pendingUrl = "";
  let firedUrl = "";
  let timer: number | null = null;
  const clearTimer = () => {
    if (timer !== null) window.clearTimeout(timer);
    timer = null;
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
  const fire = () => {
    clearTimer();
    if (pendingUrl && pendingUrl !== firedUrl) {
      firedUrl = pendingUrl;
      void api.mediaPrefetch(firedUrl);
    }
  };
  return {
    at(t) {
      const url = targetUrl(t);
      if (!url || url === pendingUrl) return;
      pendingUrl = url;
      clearTimer();
      timer = window.setTimeout(fire, 120);
    },
    commit(t) {
      const url = targetUrl(t);
      if (url) pendingUrl = url;
      fire();
    },
    cancel() {
      clearTimer();
      if (firedUrl) void api.mediaPrefetchCancel(firedUrl);
      pendingUrl = "";
      firedUrl = "";
    },
  };
}
