import { useEffect, useImperativeHandle, useMemo, useRef, forwardRef, useState } from "react";
import Hls from "hls.js";
import { api, isLocalMedia, platform, playlist } from "../api";
import { mediaErrorMessage } from "../mediaErrors";
import { mediaHost, mediaResource, observeHls, usePlayerDiagnostics } from "../playbackDiagnostics";
import { usePlaybackActivity } from "../usePlaybackActivity";
import { installHlsRecovery } from "../playbackRecovery";
import { createBufferingLease, type BufferingLease, type BufferingLeaseRequest } from "../bufferingLease";
import { decodingCapability, type PlaybackQuality } from "../playbackQuality";
import { bufferedEnd, createSeekPrefetch, isTimeBuffered, originalMediaUrl } from "../seekPrefetch";

/** 与实际引擎一致地检查 HEVC：MSE 路径不借用浏览器原生播放能力。 */
export function canPlayHevc(): boolean {
  const codecs = ["hvc1.1.6.L93.B0", "hev1.1.6.L93.B0"];
  if (Hls.isSupported()) {
    const scope = window as typeof window & { ManagedMediaSource?: typeof MediaSource };
    const source = scope.MediaSource || scope.ManagedMediaSource;
    return codecs.some(codec => {
      try { return !!source?.isTypeSupported(`video/mp4; codecs="${codec}"`); } catch { return false; }
    });
  }
  const video = document.createElement("video");
  return codecs.some(codec => !!video.canPlayType(`video/mp4; codecs="${codec}"`));
}

export type VideoHandle = {
  el: () => HTMLVideoElement | null;
  play: () => void;
  pause: () => void;
  toggle: () => void;
  seek: (t: number, source?: string) => void;
  trace: (event: string, data?: Record<string, unknown>) => void;
  rate: (r: number) => void;
  mute: (m: boolean) => void;
  volume: (v: number) => void;
  /** 拖动/点击进度条预览时调用：把目标位置的分片交给 Rust 推测预取 */
  prefetchAt: (t: number) => void;
  cancelPrefetch: () => void;
  retry: () => void;
  acquireBufferingLease: (request: BufferingLeaseRequest) => BufferingLease | null;
};

type Props = {
  src: string;
  /** 同一集换编码保留位置；切集使用不同 identity。 */
  sourceIdentity?: string;
  poster?: string;
  autoPlay?: boolean;
  active?: boolean;
  diagnosticContext?: Record<string, unknown>;
  muted?: boolean;
  loop?: boolean;
  fit?: "contain" | "cover";
  /** 换源后重新套用，避免切集时音量/倍速被浏览器重置 */
  volume?: number;
  rate?: number;
  onTime?: (t: number) => void;
  onBuffer?: (end: number) => void;
  onDuration?: (d: number) => void;
  onSize?: (w: number, h: number) => void;
  onQuality?: (quality: PlaybackQuality) => void;
  onFirstFrame?: () => void;
  onSeeking?: (seeking: boolean) => void;
  onPlaying?: () => void;
  onPause?: () => void;
  onWaiting?: () => void;
  onEnded?: () => void;
  onError?: (msg: string) => void;
  onClick?: () => void;
  className?: string;
};

/** 只负责「播」：hls.js + video 元素，控制条由外层渲染。 */
const VideoSurface = forwardRef<VideoHandle, Props>(function VideoSurface(
  { src, sourceIdentity = src, poster, autoPlay = true, active = true, diagnosticContext = {}, muted = false, loop = false, fit = "contain", volume = 1, rate = 1,
    onTime, onBuffer, onDuration, onSize, onQuality, onFirstFrame, onSeeking, onPlaying, onPause, onWaiting, onEnded, onError, onClick, className = "" },
  ref,
) {
  const videoRef = useRef<HTMLVideoElement>(null);
  const hlsRef = useRef<Hls | null>(null);
  const bufferingLease = useRef<BufferingLease | null>(null);
  const standardMse = useRef<Hls | null>(null);
  const latestSource = useRef(src); latestSource.current = src;
  const [retryEpoch, setRetryEpoch] = useState(0);
  const terminalRef = useRef(false);
  const recoveryControl = useRef<ReturnType<typeof installHlsRecovery> | null>(null);
  const restoredRate = useRef<number | null>(null);
  const savedPosition = useRef<{ identity: string; time: number; rate: number; wanted: boolean } | null>(null);
  const callbacks = useRef({ onSize, onQuality, onFirstFrame, onSeeking, onError });
  callbacks.current = { onSize, onQuality, onFirstFrame, onSeeking, onError };
  const codecRef = useRef("");
  const capabilityKey = useRef("");
  const nearEndFiredRef = useRef(false);
  const renderedFrame = useRef(false);
  const seekPrefetch = useMemo(() => createSeekPrefetch(() => hlsRef.current), []);
  useEffect(() => { if (active) recoveryControl.current?.sync(); else recoveryControl.current?.suspend(); }, [active]);
  const [ready, setReady] = useState(false);
  const bufferCallback = useRef(onBuffer); bufferCallback.current = onBuffer;
  const lastBuffer = useRef(-1);
  const reportBuffer = () => {
    const v = videoRef.current;
    const end = v ? Math.round(bufferedEnd(v) * 10) / 10 : 0;
    if (v && bufferingLease.current?.active()) {
      const ahead = (bufferedEnd(v) - v.currentTime) / Math.max(.25, v.playbackRate || 1);
      if (v.seeking || ahead <= 8) bufferingLease.current.release(v.seeking ? "seek" : "low_buffer");
    }
    if (end !== lastBuffer.current) { lastBuffer.current = end; bufferCallback.current?.(end); }
  };
  const { trace, requestSeek } = usePlayerDiagnostics(videoRef, hlsRef, { ...diagnosticContext, active, resource: mediaResource(src) });
  const [session, setSession] = useState({ src: "", active: false });
  const sessionReady = useRef<Promise<void>>(Promise.resolve());
  const activity = usePlaybackActivity(videoRef, hlsRef, { active: active && session.src === src && session.active, autoPlay, src: sourceIdentity, trace });
  useEffect(() => {
    if (!src) return;
    let alive = true;
    const url = originalMediaUrl(src);
    sessionReady.current = api.mediaSession(url, active).then(() => { if (alive) setSession({ src, active }); });
    void sessionReady.current.catch(() => { if (alive) callbacks.current.onError?.("播放会话确认失败，请重试"); });
    if (!active) { seekPrefetch.cancel(); bufferingLease.current?.release("inactive", false); }
    return () => { alive = false; seekPrefetch.cancel(); void api.mediaSession(url, false).catch(() => {}); };
  }, [src, active, seekPrefetch, retryEpoch]);

  const reportSize = (v: HTMLVideoElement | null) => {
    if (!v) return;
    const w = v.videoWidth || 0;
    const h = v.videoHeight || 0;
    if (w > 0 && h > 0) callbacks.current.onSize?.(w, h);
    const quality = { width: w, height: h, codec: codecRef.current };
    callbacks.current.onQuality?.(quality);
    const key = `${w}:${h}:${quality.codec}`;
    if (w && h && quality.codec && key !== capabilityKey.current) {
      capabilityKey.current = key;
      trace("quality.observed", { video_width: w, video_height: h, codec: quality.codec });
      void decodingCapability(quality).then(info => {
        if (info && capabilityKey.current === key) trace("quality.capability", { supported: info.supported, smooth: info.smooth, power_efficient: info.powerEfficient });
      });
    }
  };

  useImperativeHandle(ref, () => ({
    el: () => videoRef.current,
    play: () => { if (!terminalRef.current) activity.play("handle"); },
    retry: () => { activity.restoreWanted(true); setRetryEpoch(epoch => epoch + 1); },
    acquireBufferingLease: request => {
      const hls = hlsRef.current, video = videoRef.current;
      if (bufferingLease.current?.active() || !hls || !video || standardMse.current !== hls || !renderedFrame.current || !activity.isActive() || terminalRef.current || video.seeking || video.readyState < 2) return null;
      const ahead = (bufferedEnd(video) - video.currentTime) / Math.max(.25, video.playbackRate || 1);
      if (ahead < 20) return null;
      const boundSource = latestSource.current;
      let lease: BufferingLease | null = null;
      lease = createBufferingLease(hls, { ...request, maxMs: Math.min(request.maxMs, (ahead - 8) * 1000),
        isCurrent: () => hlsRef.current === hls && latestSource.current === boundSource,
        canResume: () => activity.isActive() && !terminalRef.current,
        onEnd: reason => {
          if (bufferingLease.current === lease) bufferingLease.current = null;
          request.onEnd(reason);
        },
      });
      bufferingLease.current = lease;
      return lease;
    },
    pause: () => activity.pause(),
    toggle: () => {
      const v = videoRef.current;
      if (!v || terminalRef.current) return;
      trace("toggle.request");
      if (v.paused) activity.play("toggle"); else activity.pause("toggle");
    },
    seek: (t: number, source = "handle") => {
      const v = videoRef.current;
      if (!v || terminalRef.current || !Number.isFinite(t) || !activity.isActive()) return;
      // iOS 上直接赋到 duration 会卡在 ended 态，夹到结尾前 0.15s
      const dur = v.duration && Number.isFinite(v.duration) ? v.duration : 0;
      const target = dur > 0 ? Math.max(0, Math.min(dur - 0.15, t)) : Math.max(0, t);
      bufferingLease.current?.release("seek");
      requestSeek(target, source);
      seekPrefetch.commit(target);
      const hls = hlsRef.current;
      // 已缓冲的定位保持现有加载器；MMS 停缓冲/失活后的恢复仍需 startLoad。
      if (hls && (!isTimeBuffered(v, target) || !hls.loadingEnabled || !hls.bufferingEnabled)) {
        try { hls.startLoad(target); } catch { /* ignore */ }
      }
      const resume = !v.paused;
      try { v.currentTime = target; trace("seek.assigned", { target, source }); }
      catch (e) { trace("seek.rejected", { error_name: (e as Error).name }); throw e; }
      if (resume) { const p = v.play(); if (p && p.catch) p.catch(() => {}); }
    },
    trace,
    rate: (r: number) => { const v = videoRef.current; if (v) v.playbackRate = r; },
    mute: (m: boolean) => { const v = videoRef.current; if (v) v.muted = m; },
    volume: (vol: number) => { const v = videoRef.current; if (v) v.volume = vol; },
    prefetchAt: (t: number) => { if (activity.isActive()) seekPrefetch.at(t); },
    cancelPrefetch: () => seekPrefetch.cancel(),
  }), []);

  useEffect(() => {
    const video = videoRef.current;
    if (!video || !src) return;
    setReady(false);
    lastBuffer.current = -1;
    bufferCallback.current?.(0);
    let disposed = false;
    nearEndFiredRef.current = false;
    renderedFrame.current = false;
    terminalRef.current = false;
    standardMse.current = null;
    codecRef.current = ""; capabilityKey.current = "";
    callbacks.current.onQuality?.({ width: 0, height: 0, codec: "" });
    const saved = savedPosition.current?.identity === sourceIdentity ? savedPosition.current : null;
    savedPosition.current = null;
    restoredRate.current = saved?.rate ?? null;
    if (saved) activity.restoreWanted(saved.wanted);
    let stopRecovery: ReturnType<typeof installHlsRecovery> | undefined;
    let firstFrame: number | undefined;
    let rendered = false;
    const sourceStarted = performance.now();
    const frameReady = (method: string, mediaTime: number) => {
      if (disposed || rendered || !activity.isActive()) return;
      rendered = true; renderedFrame.current = true;
      stopRecovery?.sync();
      trace("source.firstframe", { method, frame_time: mediaTime, elapsed_ms: Math.round(performance.now() - sourceStarted) });
      callbacks.current.onFirstFrame?.();
    };
    if (video.requestVideoFrameCallback) {
      const onFrame: VideoFrameRequestCallback = (_now, metadata) => {
        frameReady("video_frame", metadata.mediaTime);
        if (!disposed && !rendered) firstFrame = video.requestVideoFrameCallback(onFrame);
      };
      firstFrame = video.requestVideoFrameCallback(onFrame);
    }
    const onRenderedFallback = () => { if (!video.requestVideoFrameCallback) frameReady("playing_fallback", video.currentTime); };
    const restorePosition = () => {
      if (!saved) return;
      video.playbackRate = saved.rate;
      if (Number.isFinite(saved.time) && saved.time > 0) {
        video.currentTime = Number.isFinite(video.duration) ? Math.min(saved.time, Math.max(0, video.duration - .15)) : saved.time;
      }
      trace("source.restored", { target: saved.time, play_wanted: saved.wanted, rate: saved.rate });
    };
    video.addEventListener("loadedmetadata", restorePosition);
    video.addEventListener("playing", onRenderedFallback);
    const onVideoResize = () => reportSize(video);
    video.addEventListener("resize", onVideoResize);
    if (hlsRef.current) { hlsRef.current.destroy(); hlsRef.current = null; }

    const proxied = isLocalMedia(src);
    const url = proxied ? src : playlist(src);
    const useHls = !url.includes("/seg?u=");
    trace("source.load", { engine: useHls && Hls.isSupported() ? "hls.js" : "native", auto_play: autoPlay, resource: mediaResource(src), host: mediaHost(src) });

    /* 换源时先把音量/倍速套回去：hls 重新 attach 后浏览器会把它们重置成默认值 */
    const applySettings = () => {
      const v = videoRef.current;
      if (!v) return;
      v.volume = volume;
      v.muted = muted;
      v.playbackRate = saved?.rate ?? rate;
    };
    applySettings();

    if (useHls && Hls.isSupported()) {
      const hls = new Hls({
        // 禁用从插播/内容引导 JSON 派生直连 URL 的可选功能。
        interstitialsController: undefined,
        contentSteeringController: undefined,
        enableInterstitialPlayback: false,
        // WKWebView 里 hls.js 的 worker 不稳，iOS 关掉
        enableWorker: platform !== "ios",
        // Prefer standard MSE on macOS; hls.js handles MMS-only fallback and seek recovery.
        preferManagedMediaSource: false,
        autoStartLoad: false,
        // 缓冲窗口调大：桥侧单分片下载 3–5 秒（受 RTT 限制），缓冲区太薄时
        // 一旦预取没跟上就会卡在「等待下一片」。60 秒窗口 = 约 10–15 片余量，
        // 足够覆盖一次远距离跳转的补片时间；backBuffer 同值让回看不必重下。
        maxBufferLength: 60,
        maxMaxBufferLength: 120,
        backBufferLength: 60,
        maxBufferHole: 0.5,
        nudgeOffset: 0.25,
        nudgeMaxRetry: 6,
        manifestLoadingTimeOut: 20000,
        manifestLoadingMaxRetry: 4,
        // 后端冷片可能经历完整首探+Range；外层不以 15s 过早重启同一下载。
        fragLoadingTimeOut: 45000,
        fragLoadingMaxRetry: 2,
        fragLoadingRetryDelay: 400,
        // 跳转后允许在目标附近多找一点：分片是 2–6 秒一片，容差太小会
        // 落到更远的分片上，反而多等一片的下载时间。
        maxFragLookUpTolerance: 0.25,
      });
      hlsRef.current = hls;
      hls.once(Hls.Events.DESTROYING, () => { if (hlsRef.current === hls) bufferingLease.current?.release("destroy", false); });
      hls.on(Hls.Events.MEDIA_ATTACHED, (_event, data) => {
        const source = data.mediaSource;
        if (hlsRef.current === hls && source && source.constructor.name !== "ManagedMediaSource" && typeof window.MediaSource === "function" && source instanceof window.MediaSource) standardMse.current = hls;
      });
      observeHls(hls, trace);
      void sessionReady.current.then(() => {
        if (disposed) return;
        hls.loadSource(url);
        hls.attachMedia(video);
      }).catch(() => {});
      hls.on(Hls.Events.MANIFEST_PARSED, () => {
        if (activity.isActive()) hls.startLoad(saved?.time ?? -1);
        if (autoPlay) activity.play("manifest_autoplay", true);
      });
      hls.on(Hls.Events.FRAG_BUFFERED, reportBuffer);
      const reportCodec = () => {
        const level = hls.levels[hls.currentLevel] ?? hls.levels[0];
        if (level?.videoCodec) codecRef.current = level.videoCodec;
        reportSize(video);
      };
      hls.on(Hls.Events.LEVEL_SWITCHED, reportCodec);
      hls.on(Hls.Events.BUFFER_CODECS, (_event, tracks) => {
        const track = tracks.video;
        if (track?.codec) codecRef.current = track.codec;
        reportSize(video);

      });
      stopRecovery = installHlsRecovery(hls, { active: activity.isActive, time: () => video.currentTime, trace,
        bufferedStall: { media: video, wanted: activity.playWanted, rendered: () => rendered, beforeNudge: () => bufferingLease.current?.release("seek") },
        beforeTerminate: () => { bufferingLease.current?.release("destroy", false); savedPosition.current = { identity: sourceIdentity, time: video.currentTime, rate: video.playbackRate, wanted: activity.playWanted() }; },
        terminal: message => {
          terminalRef.current = true; seekPrefetch.cancel();
          if (hlsRef.current === hls) hlsRef.current = null;
          callbacks.current.onError?.(message);
        },
      });
      recoveryControl.current = stopRecovery;
    } else {
      void sessionReady.current.then(() => {
        if (disposed) return;
        video.src = url;
        if (autoPlay) activity.play("native_autoplay", true);
      }).catch(() => {});
    }

    return () => {
      if (!terminalRef.current || !savedPosition.current) {
        savedPosition.current = { identity: sourceIdentity, time: video.currentTime, rate: video.playbackRate, wanted: activity.playWanted() };
      }
      disposed = true;
      bufferingLease.current?.release("source_change", false);
      standardMse.current = null;
      video.removeEventListener("resize", onVideoResize);
      video.removeEventListener("loadedmetadata", restorePosition);
      video.removeEventListener("playing", onRenderedFallback);
      if (firstFrame !== undefined) video.cancelVideoFrameCallback?.(firstFrame);
      stopRecovery?.();
      if (recoveryControl.current === stopRecovery) recoveryControl.current = null;
      seekPrefetch.cancel();
      trace("source.cleanup");
      if (hlsRef.current) { hlsRef.current.destroy(); hlsRef.current = null; }
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [src, retryEpoch]);

  // iOS 内联播放：部分 WKWebView 只认 HTML 属性
  useEffect(() => {
    const v = videoRef.current;
    if (v) v.setAttribute("webkit-playsinline", "true");
  }, []);

  useEffect(() => { if (videoRef.current) videoRef.current.muted = muted; }, [muted]);
  useEffect(() => { if (videoRef.current) videoRef.current.volume = volume; }, [volume]);
  useEffect(() => { restoredRate.current = null; if (videoRef.current) videoRef.current.playbackRate = rate; }, [rate]);

  const reapply = (v: HTMLVideoElement) => {
    v.volume = volume;
    v.muted = muted;
    v.playbackRate = restoredRate.current ?? rate;
  };

  const handleVideoError = () => {
    if (hlsRef.current || terminalRef.current) return;
    terminalRef.current = true;
    callbacks.current.onError?.(mediaErrorMessage(videoRef.current?.error ?? null));
  };

  return (
    <video
      ref={videoRef}
      poster={poster}
      playsInline
      preload="auto"
      muted={muted}
      loop={loop}
      onClick={onClick}
      className={`w-full h-full bg-black ${fit === "cover" ? "object-cover" : "object-contain"} ${className}`}
      style={{ opacity: ready ? 1 : 0, transition: "opacity .35s" }}
      onLoadedData={(e) => {
        const v = e.target as HTMLVideoElement;
        setReady(true);
        reapply(v);
        reportSize(v);
      }}
      onProgress={reportBuffer}
      onRateChange={reportBuffer}
      onSeeking={() => { bufferingLease.current?.release("seek"); callbacks.current.onSeeking?.(true); }}
      onSeeked={() => { reportBuffer(); callbacks.current.onSeeking?.(false); }}
      onEmptied={reportBuffer}
      onTimeUpdate={(e) => {
        const v = e.target as HTMLVideoElement;
        reportBuffer();
        reportSize(v);
        onTime?.(v.currentTime);
        // iOS 有时不触发 ended：靠近片尾自己补一次，保证自动连播
        const d = v.duration;
        if (d && Number.isFinite(d) && v.currentTime > d - 0.3 && !nearEndFiredRef.current) {
          nearEndFiredRef.current = true;
          onEnded?.();
        }
      }}
      onLoadedMetadata={(e) => {
        const v = e.target as HTMLVideoElement;
        reapply(v);
        onDuration?.(v.duration || 0);
        reportSize(v);
        if (!hlsRef.current && autoPlay) activity.play("native_metadata", true);
      }}
      onPause={() => onPause?.()}
      onWaiting={() => { bufferingLease.current?.release("waiting"); onWaiting?.(); }}
      onPlaying={(e) => {
        reportSize(e.target as HTMLVideoElement);
        if (activity.isActive()) onPlaying?.();
      }}
      onEnded={() => { if (activity.isActive()) onEnded?.(); }}
      onError={handleVideoError}
    />
  );
});

export default VideoSurface;
