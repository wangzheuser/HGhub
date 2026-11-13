import React, { useEffect, useMemo, useRef, useState } from "react";
import Hls from "hls.js";
import { api, fmtTime, platform, playlist } from "../api";
import { mediaErrorMessage } from "../mediaErrors";
import { mediaHost, mediaResource, observeHls, usePlayerDiagnostics } from "../playbackDiagnostics";
import { usePlaybackActivity } from "../usePlaybackActivity";
import { installHlsRecovery } from "../playbackRecovery";
import { useSeekInput } from "../useSeekInput";
import { createSeekPrefetch, isTimeBuffered, originalMediaUrl } from "../seekPrefetch";
import PlaybackLock from "./PlaybackLock";
import { ICompress, IExpand, ILockF, IPauseF, IPlayF, IVol, IVolMute } from "../icons";

type Props = {
  src: string;              // 原始 m3u8 地址
  poster?: string;          // 已处理好的封面地址（本地协议）
  autoPlay?: boolean;
  active?: boolean;
  muted?: boolean;
  loop?: boolean;
  onEnded?: () => void;
  onPlaying?: () => void;
  onTogglePlay?: (playing: boolean) => void;
  onToggleMute?: () => void;
  controls?: boolean;
  className?: string;
  diagnosticContext?: Record<string, unknown>;
};

export default function Player({
  src, poster, autoPlay, active = true, muted = false, loop = false, onEnded, onPlaying,
  onTogglePlay, onToggleMute, controls = true, className = "", diagnosticContext = {},
}: Props) {
  const videoRef = useRef<HTMLVideoElement>(null);
  const wrapRef = useRef<HTMLDivElement>(null);
  const hlsRef = useRef<Hls | null>(null);
  const [retryEpoch, setRetryEpoch] = useState(0);
  const terminalRef = useRef(false);
  const retryPosition = useRef<{ src: string; time: number; rate: number; wanted: boolean } | null>(null);
  const recoveryControl = useRef<ReturnType<typeof installHlsRecovery> | null>(null);
  const seekPrefetch = useMemo(() => createSeekPrefetch(() => hlsRef.current), []);
  useEffect(() => { if (active) recoveryControl.current?.sync(); else recoveryControl.current?.suspend(); }, [active]);
  const { trace, requestSeek } = usePlayerDiagnostics(videoRef, hlsRef, { view: "post_detail", ...diagnosticContext, active, resource: mediaResource(src) });
  const [session, setSession] = useState({ src: "", active: false });
  const sessionReady = useRef<Promise<void>>(Promise.resolve());
  const activity = usePlaybackActivity(videoRef, hlsRef, { active: active && session.src === src && session.active, autoPlay: !!autoPlay, src, trace });
  useEffect(() => {
    if (!src) return;
    let alive = true;
    const url = originalMediaUrl(src);
    sessionReady.current = api.mediaSession(url, active).then(() => { if (alive) setSession({ src, active }); });
    void sessionReady.current.catch(() => { if (alive) setErrMsg("播放会话确认失败，请重试"); });
    if (!active) seekPrefetch.cancel();
    return () => { alive = false; seekPrefetch.cancel(); void api.mediaSession(url, false).catch(() => {}); };
  }, [src, active, seekPrefetch, retryEpoch]);
  const [playing, setPlaying] = useState(false);
  const [waiting, setWaiting] = useState(false);
  const [time, setTime] = useState(0);
  const [duration, setDuration] = useState(0);
  const [full, setFull] = useState(false);
  const [rate, setRate] = useState(1);
  const [errMsg, setErrMsg] = useState<string>("");
  const [controlsLocked, setControlsLocked] = useState(false);
  const seek = useSeekInput({ time, duration, enabled: active && !controlsLocked, resetKey: src,
    onInteraction: (phase, input, target, pointerTarget) => {
      trace(`seek.input.${phase}`, { input, target, pointer_target: pointerTarget });
      // 拖动/点击预览阶段就推测预取目标分片：松手后大概率已命中缓存或已在下载
      if (phase === "change" && activity.isActive()) seekPrefetch.at(target);
      if (phase === "cancel") seekPrefetch.cancel();
    },
    onSeek: (target, input) => {
      const video = videoRef.current;
      if (!video || !activity.isActive()) return;
      requestSeek(target, `legacy_${input}`);
      seekPrefetch.commit(target);
      const buffered = isTimeBuffered(video, target);
      setWaiting(!buffered);
      const hls = hlsRef.current;
      if (hls && (!buffered || !hls.loadingEnabled || !hls.bufferingEnabled)) {
        try { hls.startLoad(target); } catch { /* ignore */ }
      }
      video.currentTime = target;
      trace("seek.assigned", { target });
      setTime(video.currentTime);
    },
  });

  useEffect(() => {
    const video = videoRef.current;
    if (!video || !src) return;
    setErrMsg("");
    setWaiting(false);
    let disposed = false;
    const saved = retryPosition.current?.src === src ? retryPosition.current : null;
    retryPosition.current = null;
    if (saved) activity.restoreWanted(saved.wanted);
    video.playbackRate = saved?.rate ?? rate;
    const restorePosition = () => {
      if (!saved) return;
      video.playbackRate = saved.rate;
      if (saved.time > 0) video.currentTime = Number.isFinite(video.duration) ? Math.min(saved.time, Math.max(0, video.duration - .15)) : saved.time;
      trace("source.restored", { target: saved.time, rate: saved.rate, play_wanted: saved.wanted });
    };
    video.addEventListener("loadedmetadata", restorePosition);
    const sourceStarted = performance.now();
    let firstFrame: number | undefined;
    let rendered = false;
    const reportFirstFrame = (method: string, mediaTime: number) => {
      if (disposed || rendered || !activity.isActive()) return;
      rendered = true;
      stopRecovery?.sync();
      trace("source.firstframe", { method, frame_time: mediaTime, elapsed_ms: Math.round(performance.now() - sourceStarted) });
    };
    if (video.requestVideoFrameCallback) {
      const frame: VideoFrameRequestCallback = (_now, metadata) => {
        reportFirstFrame("video_frame", metadata.mediaTime);
        if (!disposed && !rendered) firstFrame = video.requestVideoFrameCallback(frame);
      };
      firstFrame = video.requestVideoFrameCallback(frame);
    }
    const fallbackFrame = () => { if (!video.requestVideoFrameCallback) reportFirstFrame("playing_fallback", video.currentTime); };
    video.addEventListener("playing", fallbackFrame);
    terminalRef.current = false;
    let stopRecovery: ReturnType<typeof installHlsRecovery> | undefined;
    const url = playlist(src);
    trace("source.load", { engine: Hls.isSupported() ? "hls.js" : "native", auto_play: autoPlay, resource: mediaResource(src), host: mediaHost(src) });

    if (hlsRef.current) { hlsRef.current.destroy(); hlsRef.current = null; }

    if (Hls.isSupported()) {
      const hls = new Hls({
        // 禁用从插播/内容引导 JSON 派生直连 URL 的可选功能。
        interstitialsController: undefined,
        contentSteeringController: undefined,
        enableInterstitialPlayback: false,
        // WKWebView 里 hls.js 的 worker 不稳，iOS 关掉
        enableWorker: platform !== "ios",
        // Keep the same macOS seek workaround as the shared player.
        preferManagedMediaSource: false,
        autoStartLoad: false,
        lowLatencyMode: false,
        // 与 VideoSurface 保持一致：桥侧单分片 3–5 秒，60 秒窗口才有足够余量
        maxBufferLength: 60,
        maxMaxBufferLength: 120,
        backBufferLength: 60,
        maxBufferHole: 0.5,
        nudgeOffset: 0.25,
        nudgeMaxRetry: 6,
        manifestLoadingTimeOut: 20000,
        manifestLoadingMaxRetry: 4,
        fragLoadingTimeOut: 45000,
        fragLoadingMaxRetry: 2,
        fragLoadingRetryDelay: 400,
        maxFragLookUpTolerance: 0.25,
      });
      hlsRef.current = hls;
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
      stopRecovery = installHlsRecovery(hls, { active: activity.isActive, time: () => video.currentTime, trace,
        bufferedStall: { media: video, wanted: activity.playWanted, rendered: () => rendered },
        beforeTerminate: () => { retryPosition.current = { src, time: video.currentTime, rate: video.playbackRate, wanted: activity.playWanted() }; },
        terminal: message => {
          terminalRef.current = true; seekPrefetch.cancel();
          if (hlsRef.current === hls) hlsRef.current = null;
          setWaiting(false); setPlaying(false); setErrMsg(message);
        },
      });
      recoveryControl.current = stopRecovery;
    } else {
      // Safari / 部分 WebView 原生支持 HLS
      void sessionReady.current.then(() => {
        if (disposed) return;
        video.src = url;
        if (autoPlay) activity.play("native_autoplay", true);
      }).catch(() => {});
    }

    return () => {
      disposed = true;
      video.removeEventListener("playing", fallbackFrame);
      video.removeEventListener("loadedmetadata", restorePosition);
      if (firstFrame !== undefined) video.cancelVideoFrameCallback?.(firstFrame);
      stopRecovery?.();
      if (recoveryControl.current === stopRecovery) recoveryControl.current = null;
      trace("source.cleanup");
      seekPrefetch.cancel();
      if (hlsRef.current) { hlsRef.current.destroy(); hlsRef.current = null; }
    };
  }, [src, autoPlay, retryEpoch]);

  useEffect(() => {
    const video = videoRef.current;
    if (!video) return;
    video.muted = muted;
  }, [muted]);

  // iOS 内联播放：部分 WKWebView 只认 HTML 属性，不认 React 的 playsInline
  useEffect(() => {
    const video = videoRef.current;
    if (!video) return;
    video.setAttribute("webkit-playsinline", "true");
  }, []);

  useEffect(() => {
    const onFs = () => setFull(!!document.fullscreenElement);
    document.addEventListener("fullscreenchange", onFs);
    return () => document.removeEventListener("fullscreenchange", onFs);
  }, []);

  const toggle = () => {
    const video = videoRef.current;
    if (!video) return;
    trace("toggle.request");
    if (terminalRef.current) return;
    if (video.paused) activity.play("toggle");
    else activity.pause("toggle");
  };

  const cycleRate = () => {
    const video = videoRef.current;
    if (!video) return;
    const next = rate === 1 ? 1.25 : rate === 1.25 ? 1.5 : rate === 1.5 ? 2 : 1;
    video.playbackRate = next;
    setRate(next);
  };

  const toggleFull = () => {
    const el = wrapRef.current;
    if (!el) return;
    if (document.fullscreenElement) document.exitFullscreen().catch(() => {});
    else el.requestFullscreen().catch(() => {});
  };

  const pct = duration > 0 ? (seek.time / duration) * 100 : 0;

  return (
    <div ref={wrapRef} className={`relative bg-black overflow-hidden group ${className}`}>
      <div className="absolute inset-0" inert={controlsLocked}>
      <video
        ref={videoRef}
        poster={poster}
        playsInline
        preload="auto"
        muted={muted}
        loop={loop}
        className="w-full h-full object-contain bg-black"
        onClick={toggle}
        onPlay={() => { if (activity.isActive()) { setPlaying(true); onTogglePlay?.(true); } }}
        onPause={() => { setPlaying(false); onTogglePlay?.(false); }}
        onWaiting={() => setWaiting(true)}
        onPlaying={() => { if (activity.isActive()) { setPlaying(true); setWaiting(false); setErrMsg(""); onPlaying?.(); } }}
        onSeeked={() => setWaiting(false)}
        onTimeUpdate={(e) => setTime((e.target as HTMLVideoElement).currentTime)}
        onLoadedMetadata={(e) => {
          setDuration((e.target as HTMLVideoElement).duration || 0);
          if (!hlsRef.current && autoPlay) activity.play("native_metadata", true);
        }}
        onEnded={() => { if (activity.isActive()) onEnded?.(); }}
        onError={() => {
          if (hlsRef.current || terminalRef.current) return;
          terminalRef.current = true;
          setWaiting(false);
          setPlaying(false);
          setErrMsg(mediaErrorMessage(videoRef.current?.error ?? null));
        }}
      />

      {errMsg && (
        <div className="absolute left-2 top-2 z-30 max-w-[80%] rounded bg-black/70 px-2 py-1 text-[10px] leading-tight text-red-300 break-all">
          {errMsg}
          <button type="button" className="ml-2 underline" onClick={() => { activity.restoreWanted(true); setRetryEpoch(epoch => epoch + 1); }}>重试</button>
        </div>
      )}

      {waiting && (
        <div className="absolute inset-0 flex items-center justify-center pointer-events-none">
          <div className="w-9 h-9 spinner" />
        </div>
      )}

      {!playing && !waiting && (
        <button onClick={toggle} aria-label="播放"
          className="absolute inset-0 flex items-center justify-center">
          <span className="w-16 h-16 rounded-full bg-black/45 backdrop-blur-sm flex items-center justify-center elev-2 pressable">
            <IPlayF size={26} className="text-white ml-0.5" />
          </span>
        </button>
      )}

      {controls && (
        <div className="absolute inset-x-0 bottom-0 px-3 pb-2 pt-8 bg-gradient-to-t from-black/80 to-transparent opacity-0 group-hover:opacity-100 transition-opacity">
          <input aria-label="播放进度" type="range" {...seek.inputProps}
            className="hg-range w-full" style={{ ...seek.inputProps.style, "--fill": `${pct}%` } as React.CSSProperties} />
          <div className="flex items-center gap-3 mt-1 text-white">
            <button onClick={toggle} aria-label={playing ? "暂停" : "播放"} className="pressable">
              {playing ? <IPauseF size={16} /> : <IPlayF size={16} />}
            </button>
            <button onClick={() => onToggleMute?.()} aria-label={muted ? "取消静音" : "静音"} className="pressable">
              {muted ? <IVolMute size={16} /> : <IVol size={16} />}
            </button>
            <span className="text-[11px] tabular-nums text-white/85">{fmtTime(seek.time)} / {fmtTime(duration)}</span>
            <div className="flex-1" />
            <button onClick={() => setControlsLocked(true)} aria-label="锁定播放控制" title="锁定播放控制" aria-pressed={false} className="pressable">
              <ILockF size={16} />
            </button>
            <button onClick={cycleRate} className="text-[11px] font-bold text-white/85 pressable tabular-nums">{rate}x</button>
            <button onClick={toggleFull} aria-label="全屏" className="pressable">
              {full ? <ICompress size={16} /> : <IExpand size={16} />}
            </button>
          </div>
        </div>
      )}
      </div>
      {controlsLocked && <PlaybackLock active={active} onUnlock={() => setControlsLocked(false)} />}
    </div>
  );
}
