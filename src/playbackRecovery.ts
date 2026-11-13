import Hls from "hls.js";

export type BufferedStallSnapshot = {
  active: boolean; wanted: boolean; rendered: boolean; paused: boolean; seeking: boolean; ended: boolean;
  time: number; duration: number; rate: number; bufferEnd: number; ranges: number;
};
/** 只补内建GapController遗漏的单连续缓冲内停顿，不重启任何加载器。 */
export function createBufferedStallRecovery(options: {
  sample: () => BufferedStallSnapshot;
  nudge: (target: number) => void;
  terminal: (message: string) => void;
  trace?: (event: string, data: Record<string, unknown>) => void;
  now?: () => number;
  schedule?: (fn: () => void, ms: number) => ReturnType<typeof setTimeout>;
  clear?: (timer: ReturnType<typeof setTimeout>) => void;
}) {
  const now = options.now ?? (() => performance.now()), schedule = options.schedule ?? setTimeout, clear = options.clear ?? clearTimeout;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let disposed = false, attempts = 0, lastTime: number | undefined, progressAt = 0;
  let ownTarget: number | undefined, ownAt = 0, ownSeeking = false, episodeBegan: number | undefined;
  const nudges: number[] = [];
  const clearTimer = () => { if (timer !== undefined) clear(timer); timer = undefined; };
  const reset = () => { clearTimer(); attempts = 0; lastTime = undefined; ownTarget = undefined; ownSeeking = false; episodeBegan = undefined; };
  const arm = () => { if (timer === undefined && !disposed) timer = schedule(() => { timer = undefined; sync(); }, 1000); };
  const terminate = (reason = "attempt_budget") => {
    disposed = true; clearTimer();
    options.trace?.("recovery.buffered_stall.terminal", { attempts, reason });
    options.terminal("缓冲已就绪但播放仍停顿，请重试");
  };
  const sync = () => {
    if (disposed) return;
    const s = options.sample(), time = now();
    if (!s.active || !s.wanted || !s.rendered || s.paused || s.ended || !(s.rate > 0)) { reset(); return; }
    if (s.seeking) {
      // 自身微seek若永远不发seeked，仍用同一个timer监视，不让seeking绕开预算。
      if (ownTarget === undefined || Math.abs(s.time - ownTarget) > .025) { reset(); return; }
      ownSeeking = true;
      if (time - ownAt >= 8000 || (episodeBegan !== undefined && time - episodeBegan >= 45_000)) {
        terminate(time - ownAt >= 8000 ? "own_seek_timeout" : "episode_timeout"); return;
      }
      arm(); return;
    }
    if (ownSeeking) { ownSeeking = false; lastTime = s.time; progressAt = time; }
    const rate = Math.max(.25, s.rate || 1);
    const eligible = s.active && s.wanted && s.rendered && !s.paused && !s.ended && s.rate > 0 && s.ranges === 1 &&
      Number.isFinite(s.time) && s.bufferEnd - s.time >= Math.max(2, 2 * rate);
    if (!eligible) { reset(); return; }
    if (lastTime === undefined) { lastTime = s.time; progressAt = time; }
    const ownAssignment = ownTarget !== undefined && time - ownAt <= 2000 && Math.abs(s.time - ownTarget) <= .025;
    if (Math.abs(s.time - lastTime) > .025 && !ownAssignment) {
      // 真正推进解除本次停顿；45s滑窗计数不因playing或内部seek而重置。
      if (attempts) options.trace?.("recovery.buffered_stall.resolved", { attempts });
      attempts = 0; progressAt = time; ownTarget = undefined; episodeBegan = undefined;
    }
    lastTime = s.time;
    if (time - progressAt >= 3000) {
      while (nudges.length && time - nudges[0] >= 45_000) nudges.shift();
      if (attempts >= 2 || nudges.length >= 2 || time - progressAt >= 45_000) { terminate(); return; }
      const target = Math.min(s.time + .15, s.bufferEnd - .05, Number.isFinite(s.duration) ? s.duration - .15 : Infinity);
      if (!(target > s.time + .025)) { reset(); return; }
      episodeBegan ??= progressAt;
      attempts++; nudges.push(time); ownTarget = target; ownAt = time; progressAt = time; lastTime = target;
      options.trace?.("recovery.buffered_stall.nudge", { attempt: attempts, target, ahead_s: (s.bufferEnd - s.time) / rate, window_attempts: nudges.length });
      try { options.nudge(target); } catch { terminate(); return; }
    }
    arm();
  };
  return {
    sync,
    suspend: reset,
    seeking() {
      if (disposed) return;
      const s = options.sample();
      ownSeeking = ownTarget !== undefined && Math.abs(s.time - ownTarget) <= .025;
      clearTimer();
      if (!ownSeeking) { attempts = 0; lastTime = undefined; ownTarget = undefined; episodeBegan = undefined; }
      else sync();
    },
    seeked() {
      if (disposed) return;
      if (ownSeeking) { lastTime = options.sample().time; progressAt = now(); }
      ownSeeking = false; sync();
    },
    destroy() { disposed = true; reset(); },
  };
}

/** 每次源/显式重试一个预算：成功缓冲不重置次数，避免间歇错误无限续命。 */
export function createRecoveryBudget(maxAttempts = 3, totalMs = 45_000, now = Date.now) {
  let attempts = 0, began: number | undefined, terminated = false;
  return {
    next() {
      if (terminated) return null;
      began ??= now();
      if (attempts >= maxAttempts || now() - began >= totalMs) { terminated = true; return null; }
      attempts++;
      return attempts * 250;
    },
    available() { return !terminated && (began === undefined || now() - began < totalMs); },
    terminate() { terminated = true; },
  };
}

export function installHlsRecovery(hls: Hls, options: {
  active: () => boolean; time: () => number;
  terminal: (message: string) => void;
  beforeTerminate?: () => void;
  bufferedStall?: { media: HTMLVideoElement; wanted: () => boolean; rendered: () => boolean; beforeNudge?: () => void };
  trace: (event: string, data?: Record<string, unknown>) => void;
}) {
  const budget = createRecoveryBudget();
  let timer: ReturnType<typeof setTimeout> | undefined;
  let disposed = false;
  let buffered: ReturnType<typeof createBufferedStallRecovery> | undefined;
  let detachBuffered = () => {};
  const terminate = (message: string) => {
    if (disposed) return;
    disposed = true; budget.terminate(); clearTimeout(timer);
    buffered?.destroy(); detachBuffered();
    options.trace("recovery.terminal", { error: message });
    options.beforeTerminate?.();
    hls.stopLoad(); hls.destroy(); options.terminal(message);
  };
  hls.on(Hls.Events.ERROR, (_event, data) => {
    if (disposed) return;
    const message = `${data.type}/${data.details}`;
    if (data.details === Hls.ErrorDetails.BUFFER_ADD_CODEC_ERROR || data.details === Hls.ErrorDetails.MANIFEST_INCOMPATIBLE_CODECS_ERROR) {
      terminate(message); return;
    }
    // hls.js 的非 fatal 重试、stall nudge 自行负责；这里不重启加载器。
    if (!data.fatal || !options.active()) return;
    if (data.type !== Hls.ErrorTypes.NETWORK_ERROR && data.type !== Hls.ErrorTypes.MEDIA_ERROR) { terminate(message); return; }
    if (timer !== undefined) return;
    const delay = budget.next();
    if (delay === null) { terminate(message); return; }
    options.trace("recovery.scheduled", { error: message, delay_ms: delay });
    timer = setTimeout(() => {
      timer = undefined;
      if (disposed || !options.active()) return;
      if (!budget.available()) { terminate(message); return; }
      try {
        if (data.type === Hls.ErrorTypes.NETWORK_ERROR) hls.startLoad(options.time() || -1);
        else hls.recoverMediaError();
      } catch { terminate(message); }
    }, delay);
  });
  const dispose = () => { disposed = true; clearTimeout(timer); budget.terminate(); buffered?.destroy(); detachBuffered(); };
  const watched = options.bufferedStall;
  if (watched) {
    const media = watched.media;
    buffered = createBufferedStallRecovery({
      sample: () => {
        let end = 0, ranges = 0;
        try {
          ranges = media.buffered.length;
          for (let i = 0; i < ranges; i++) if (media.buffered.start(i) <= media.currentTime && media.buffered.end(i) > media.currentTime) { end = media.buffered.end(i); break; }
        } catch { /* 已卸载的SourceBuffer不构成可恢复的已缓冲停顿 */ }
        return { active: !disposed && options.active() && (typeof document !== "object" || document.visibilityState !== "hidden"), wanted: watched.wanted(), rendered: watched.rendered(),
          paused: media.paused, seeking: media.seeking, ended: media.ended, time: media.currentTime, duration: media.duration,
          rate: media.playbackRate, bufferEnd: end, ranges };
      },
      nudge: target => { watched.beforeNudge?.(); media.currentTime = target; },
      terminal: terminate, trace: options.trace,
    });
    const sync = () => buffered?.sync(), seeking = () => buffered?.seeking(), seeked = () => buffered?.seeked();
    const events = ["waiting", "playing", "timeupdate", "progress", "pause", "ended"];
    events.forEach(event => media.addEventListener(event, sync));
    media.addEventListener("seeking", seeking); media.addEventListener("seeked", seeked);
    if (typeof document === "object") document.addEventListener("visibilitychange", sync);
    hls.on(Hls.Events.FRAG_BUFFERED, sync);
    hls.on(Hls.Events.DESTROYING, dispose);
    detachBuffered = () => {
      events.forEach(event => media.removeEventListener(event, sync));
      media.removeEventListener("seeking", seeking); media.removeEventListener("seeked", seeked);
      if (typeof document === "object") document.removeEventListener("visibilitychange", sync);
      hls.off(Hls.Events.FRAG_BUFFERED, sync); hls.off(Hls.Events.DESTROYING, dispose);
    };
  }
  return Object.assign(dispose, { sync: () => buffered?.sync(), suspend: () => buffered?.suspend() });
}
