import { useCallback, useLayoutEffect, useRef, type RefObject } from "react";
import type Hls from "hls.js";
import { createPlaybackIntent } from "./playbackIntent";

/** 会话ACK只控制何时播放，不从旧源paused状态推导新内容的意图。 */
export function usePlaybackActivity(
  videoRef: RefObject<HTMLVideoElement | null>, hlsRef: RefObject<Hls | null>,
  { active, autoPlay, src, trace }: { active: boolean; autoPlay: boolean; src: string; trace: (event: string, data?: Record<string, unknown>) => void },
) {
  const latest = useRef({ active, trace, src }); latest.current = { active, trace, src };
  const intentRef = useRef<ReturnType<typeof createPlaybackIntent> | null>(null);
  if (!intentRef.current) intentRef.current = createPlaybackIntent(src, autoPlay);
  const intent = intentRef.current;
  const mounted = useRef(true);
  const previous = useRef({ active: false, src });

  const play = useCallback((source: string, automatic = false) => {
    const v = videoRef.current;
    if (!v) return;
    if (!mounted.current || !latest.current.active || (automatic && !intent.wanted())) {
      latest.current.trace("play.blocked", { source, inactive: !latest.current.active, play_wanted: intent.wanted() });
      return;
    }
    const identity = latest.current.src;
    intent.set(true);
    const hls = hlsRef.current;
    if (hls?.levels.length && !hls.loadingEnabled) hls.startLoad(v.currentTime);
    latest.current.trace("play.request", { source });
    v.play().then(() => {
      // 旧源的晚到Promise不暂停新源；同源的晚到play必须尊重最新主动pause。
      if (identity === latest.current.src && (!mounted.current || !latest.current.active || !intent.wanted())) v.pause();
    }).catch(e => latest.current.trace("play.rejected", { source, error_name: e?.name }));
  }, [intent]);
  const pause = useCallback((source = "handle") => {
    intent.set(false);
    latest.current.trace("pause.request", { source });
    videoRef.current?.pause();
  }, [intent]);

  useLayoutEffect(() => {
    const v = videoRef.current;
    if (!v) return;
    mounted.current = true;
    const changed = intent.transition(src, autoPlay);
    const before = previous.current;
    const hls = hlsRef.current;
    if (!active) {
      intent.suspend({ identity: before.src, wasActive: before.active, ready: v.readyState >= 2, paused: v.paused, ended: v.ended });
      v.pause();
      if (hls?.levels.length) hls.stopLoad();
      latest.current.trace("page.suspended", { play_wanted: intent.wanted() });
    } else if (!before.active || changed) {
      if (hls?.levels.length && !hls.loadingEnabled) hls.startLoad(v.currentTime);
      if (intent.wanted() && (hls?.levels.length || v.readyState > 0)) play("page_active", true);
    }
    previous.current = { active, src };
  }, [active, autoPlay, src, play, intent]);

  useLayoutEffect(() => {
    const v = videoRef.current;
    if (!v) return;
    const guard = () => {
      if (latest.current.active) return;
      latest.current.trace("play.blocked", { source: "media_event", inactive: true });
      v.pause();
      if (hlsRef.current?.levels.length) hlsRef.current.stopLoad();
    };
    v.addEventListener("play", guard);
    v.addEventListener("playing", guard);
    return () => { mounted.current = false; v.removeEventListener("play", guard); v.removeEventListener("playing", guard); v.pause(); };
  }, []);
  return {
    play, pause,
    isActive: () => mounted.current && latest.current.active,
    playWanted: () => intent.wanted(),
    restoreWanted: (value: boolean) => intent.set(value),
    observePause: () => {
      const v = videoRef.current;
      if (latest.current.active && v && !v.ended) intent.set(false);
    },
    observePlay: () => { if (latest.current.active) intent.set(true); },
  };
}
