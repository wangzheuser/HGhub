import { rawCall } from "./api";
import { useEffect, useRef, useSyncExternalStore, type RefObject } from "react";
import Hls from "hls.js";
import type { FragBufferedData, FragLoadedData, FragLoadingData } from "hls.js";

type Data = Record<string, unknown>;
type Status = { enabled: boolean; path: string | null; error: string | null };
type Entry = { event: string; data: Data };
type Snapshot = Status & { ready: boolean; pending: boolean };

// Separate transport also lets browser regressions verify diagnostics without Tauri or upstream.
// rawCall 在浏览器模式走本机桥（桥白名单含 playback_log_*），桌面模式走 IPC，行为一致。
export const playbackLogApi = {
  status: () => rawCall<Status>("playback_log_status"),
  set: (enabled: boolean) => rawCall<Status>("playback_log_set", { enabled }),
  write: (entries: Entry[]) => rawCall<void>("playback_log_write", { entries }),
  read: () => rawCall<string>("playback_log_read"),
};
let state: Snapshot = { enabled: false, path: null, error: null, ready: false, pending: false };
const listeners = new Set<() => void>();
const session = globalThis.crypto?.randomUUID?.() ?? `web-${Date.now()}-${Math.random().toString(16).slice(2)}`;
let init: Promise<void> | undefined;
let queue: Entry[] = [];
let timer: ReturnType<typeof setTimeout> | undefined;
let writing: Promise<void> = Promise.resolve();
let nextPlayer = 0;
function publish(next: Partial<Snapshot>) {
  state = { ...state, ...next };
  listeners.forEach(fn => fn());
}
const subscribe = (fn: () => void) => { listeners.add(fn); return () => { listeners.delete(fn); }; };
export const usePlaybackLog = () => useSyncExternalStore(subscribe, () => state);
export const playbackLogEnabled = () => state.enabled && !state.pending && !state.error;

export function tracePlayback(event: string, data: Data = {}) {
  if (!playbackLogEnabled()) return;
  // Bounded batches; sustained dragging must not create unlimited IPC or disk work.
  if (queue.length >= 512) {
    queue.splice(0, queue.length - 510);
    queue.unshift({ event: "queue.dropped", data: { session } });
  }
  queue.push({ event, data: { ...data, session, at: new Date().toISOString(), mono_ms: Math.round(performance.now()) } });
  if (!timer) timer = setTimeout(() => { void flushPlaybackLog(); }, 250);
}

export async function flushPlaybackLog() {
  clearTimeout(timer); timer = undefined;
  const entries = queue; queue = [];
  writing = writing.catch(() => {}).then(async () => {
    for (let i = 0; i < entries.length; i += 128) await playbackLogApi.write(entries.slice(i, i + 128));
  });
  try { await writing; }
  catch { publish({ error: "诊断日志写入失败，请检查可用空间后重新开启" }); }
}

export function initPlaybackLog() {
  return init ??= playbackLogApi.status().then(saved => {
    publish({ ...saved, ready: true });
    tracePlayback("app.start", { version: __APP_VERSION__, platform: navigator.platform, user_agent: navigator.userAgent, hls_version: Hls.version, media_source: !!window.MediaSource, managed_media_source: "ManagedMediaSource" in window });
  }).catch(() => { init = undefined; publish({ error: "无法读取诊断日志设置" }); });
}

export async function setPlaybackLog(on: boolean) {
  if (!state.ready || state.pending) throw new Error("日志设置尚未就绪");
  publish({ pending: true });
  try {
    await flushPlaybackLog();
    const saved = await playbackLogApi.set(on);
    publish({ ...saved, pending: false });
    tracePlayback("app.snapshot", { version: __APP_VERSION__, platform: navigator.platform, user_agent: navigator.userAgent, hls_version: Hls.version, media_source: !!window.MediaSource, managed_media_source: "ManagedMediaSource" in window });
  } finally { if (state.pending) publish({ pending: false }); }
}

export async function copyPlaybackLog() {
  await flushPlaybackLog();
  const text = await playbackLogApi.read();
  if (!text) throw new Error("尚无诊断日志，请开启后复现问题");
  if (!navigator.clipboard) throw new Error("当前环境无法使用剪贴板，请从日志目录取出 playback.log");
  await navigator.clipboard.writeText(text);
}

export function mediaResource(raw: string) {
  try {
    const url = new URL(raw);
    if (url.protocol === "hg:" || url.hostname === "hg.localhost") raw = url.searchParams.get("u") || raw;
  } catch { /* still fingerprint invalid targets without logging their contents */ }
  let hash = 2166136261;
  for (const b of new TextEncoder().encode(raw)) hash = Math.imul(hash ^ b, 16777619) >>> 0;
  return `r${hash.toString(16).padStart(8, "0")}`;
}

/**
 * 只取 host，用来判断「是哪条 CDN / 哪个接口域名」。
 * 不含 path 与 query，所以能落盘：出问题时一眼看出是源站、还是某个分片域名挂了。
 */
export function mediaHost(raw: string): string {
  try {
    const url = new URL(raw);
    if (url.protocol === "hg:" || url.hostname === "hg.localhost") {
      const inner = url.searchParams.get("u");
      if (inner) return new URL(inner).host;
    }
    return url.host;
  } catch {
    return "?";
  }
}

function ranges(value: TimeRanges) {
  return Array.from({ length: Math.min(value.length, 12) }, (_, i) => [round(value.start(i)), round(value.end(i))]);
}
const round = (n: number) => Number.isFinite(n) ? Math.round(n * 1000) / 1000 : null;
const engines = new WeakMap<Hls, string>();
export function mediaSnapshot(v: HTMLVideoElement, hls?: Hls | null) {
  return { time: round(v.currentTime), duration: round(v.duration), paused: v.paused, seeking: v.seeking, ready: v.readyState, network: v.networkState,
    buffered: ranges(v.buffered), seekable: ranges(v.seekable), muted: v.muted, volume: v.volume, rate: v.playbackRate,
    buffering_enabled: hls?.bufferingEnabled, loading_enabled: hls?.loadingEnabled, media_error: v.error?.code ?? null,
    engine: hls ? engines.get(hls) : "native", prefer_managed: hls?.config.preferManagedMediaSource,
    visible: getComputedStyle(v).visibility !== "hidden" && v.getClientRects().length > 0 };
}

export function usePlayerDiagnostics(videoRef: RefObject<HTMLVideoElement | null>, hlsRef: RefObject<Hls | null>, context: Data) {
  const id = useRef<string>("");
  if (!id.current) id.current = `player-${++nextPlayer}`;
  const current = useRef(context); current.current = context;
  const snapshot = () => {
    const v = videoRef.current;
    return { player: id.current, ...current.current, ...(v ? mediaSnapshot(v, hlsRef.current) : {}) };
  };
  const trace = (event: string, data: Data = {}) => {
    if (!playbackLogEnabled()) return;
    try { tracePlayback(event, { ...snapshot(), seek: pending.current?.id, ...data }); } catch { /* diagnostics must never interrupt playback */ }
  };
  const pending = useRef<{ id: number; target: number; start: number; seeks: number; seeked: boolean } | null>(null);
  const seekSeq = useRef(0);
  const requestSeek = (target: number, source: string) => {
    if (!playbackLogEnabled()) return;
    const previous = pending.current;
    if (previous) trace("seek.superseded", { seek: previous.id, next_target: round(target), elapsed_ms: Math.round(performance.now() - previous.start) });
    pending.current = { id: ++seekSeq.current, target, start: performance.now(), seeks: 0, seeked: false };
    trace("seek.request", { seek: seekSeq.current, target: round(target), source });
    if (videoRef.current && !videoRef.current.seeking && Math.abs(videoRef.current.currentTime - target) < .001) {
      trace("seek.noop", { seek: seekSeq.current });
      pending.current = null;
    }
  };

  useEffect(() => {
    const v = videoRef.current;
    if (!v) return;
    let frame: number | undefined;
    let lastHeartbeat = 0;
    let timeoutReported = 0;
    const emit = (event: string, data: Data = {}) => {
      if (playbackLogEnabled()) {
        try { tracePlayback(event, { player: id.current, ...current.current, ...mediaSnapshot(v, hlsRef.current), ...data }); } catch { /* detached or changing ranges */ }
      }
    };
    const finish = (method: string, frameTime?: number) => {
      const p = pending.current;
      if (!p || !p.seeked) return;
      emit(method === "paused_ready" ? "seek.ready" : "seek.resumed", { seek: p.id, target: round(p.target), elapsed_ms: Math.round(performance.now() - p.start), method, frame_time: frameTime == null ? null : round(frameTime) });
      pending.current = null;
    };
    const onEvent = (e: Event) => {
      if (!playbackLogEnabled()) return;
      const p = pending.current;
      if (e.type === "seeking" && p) p.seeks++;
      if (e.type === "seeked" && p) p.seeked = true;
      emit(`media.${e.type}`, p ? { seek: p.id, target: round(p.target), elapsed_ms: Math.round(performance.now() - p.start), seeking_events: p.seeks } : {});
      if (e.type === "seeked") {
        if (frame != null) v.cancelVideoFrameCallback?.(frame);
        if (v.paused && v.readyState >= 2) finish("paused_ready");
        else if (v.requestVideoFrameCallback) {
          const expected = p?.id;
          frame = v.requestVideoFrameCallback((_now, metadata) => {
            if (pending.current?.id === expected) finish("video_frame", metadata.mediaTime);
          });
        }
      }
      if (e.type === "playing" && !v.requestVideoFrameCallback) finish("playing");
    };
    const events = ["loadstart", "loadedmetadata", "loadeddata", "play", "pause", "playing", "waiting", "stalled", "seeking", "seeked", "ended", "error", "emptied", "abort", "ratechange", "volumechange", "canplay"];
    events.forEach(event => v.addEventListener(event, onEvent));
    const stateChange = () => { if (playbackLogEnabled()) emit("player.snapshot"); else pending.current = null; };
    const unsubscribe = subscribe(stateChange);
    const heartbeat = setInterval(() => {
      if (!playbackLogEnabled()) return;
      const p = pending.current;
      if (p && performance.now() - p.start > 8000 && timeoutReported !== p.id) {
        timeoutReported = p.id;
        emit("seek.timeout", { seek: p.id, target: round(p.target), elapsed_ms: Math.round(performance.now() - p.start) });
      }
      if (performance.now() - lastHeartbeat >= 5000) { lastHeartbeat = performance.now(); emit("player.heartbeat"); }
    }, 1000);
    emit("player.mount");
    return () => {
      emit("player.unmount");
      events.forEach(event => v.removeEventListener(event, onEvent));
      clearInterval(heartbeat); unsubscribe();
      if (frame != null) v.cancelVideoFrameCallback?.(frame);
    };
  }, []);
  useEffect(() => { trace("player.context"); }, [context.active, context.view, context.video_id, context.episode, context.resource]);
  return { trace, requestSeek };
}

export function observeHls(hls: Hls, trace: (event: string, data?: Data) => void) {
  hls.on(Hls.Events.MEDIA_ATTACHED, (_event, data) => {
    const source = data.mediaSource;
    engines.set(hls, source?.constructor.name || "MediaSource");
    trace("hls.attached", { engine: engines.get(hls), prefer_managed: hls.config.preferManagedMediaSource });
    if (source) {
      const start = () => trace("mms.startstreaming");
      const end = () => trace("mms.endstreaming");
      source.addEventListener("startstreaming", start);
      source.addEventListener("endstreaming", end);
      hls.once(Hls.Events.DESTROYING, () => { source.removeEventListener("startstreaming", start); source.removeEventListener("endstreaming", end); });
    }
  });
  hls.on(Hls.Events.MANIFEST_PARSED, () => trace("hls.manifest", { levels: hls.levels.length }));
  for (const event of [Hls.Events.FRAG_LOADING, Hls.Events.FRAG_LOADED, Hls.Events.FRAG_BUFFERED, Hls.Events.FRAG_LOAD_EMERGENCY_ABORTED] as const) {
    hls.on(event, (_event: string, data: FragLoadingData | FragLoadedData | FragBufferedData) => {
      if (!playbackLogEnabled()) return;
      const { frag } = data;
      const loading = frag.stats.loading;
      trace(event, { resource: mediaResource(frag.url), host: mediaHost(frag.url), fragment: frag.sn, start: round(frag.start), duration: round(frag.duration), bytes: frag.stats.loaded,
        load_ms: loading.end > 0 && loading.end >= loading.start ? round(loading.end - loading.start) : null,
        first_byte_ms: loading.first > 0 && loading.first >= loading.start ? round(loading.first - loading.start) : null });
    });
  }
  hls.on(Hls.Events.ERROR, (_event, data) => trace("hls.error", { type: data.type, detail: data.details, fatal: data.fatal, status: data.response?.code,
    fragment: data.frag?.sn, resource: data.frag ? mediaResource(data.frag.url) : undefined, host: data.frag ? mediaHost(data.frag.url) : undefined }));
}

export function installPlaybackPageEvents() {
  document.addEventListener("visibilitychange", () => tracePlayback("page.visibility", { visibility: document.visibilityState }));
  window.addEventListener("pagehide", () => { tracePlayback("page.hide"); void flushPlaybackLog(); });
  document.addEventListener("securitypolicyviolation", e => tracePlayback("security.csp_violation", { directive: e.effectiveDirective, disposition: e.disposition, resource: mediaResource(e.blockedURI) }));
}
