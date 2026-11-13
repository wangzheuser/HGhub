export type BufferingLease = {
  release: (reason?: string, resume?: boolean) => void;
  active: () => boolean;
};
export type BufferingLeaseRequest = { maxMs: number; onEnd: (reason: string) => void };
export type BufferingEngine = {
  readonly loadingEnabled: boolean;
  readonly bufferingEnabled: boolean;
  pauseBuffering: () => void;
  resumeBuffering: () => void;
};

/** 仅包装一个具体实例；不接管原本关闭的缓冲，不修改全局 HLS prototype。 */
export function createBufferingLease(engine: BufferingEngine, options: BufferingLeaseRequest & {
  isCurrent: () => boolean;
  canResume?: () => boolean;
  schedule?: (fn: () => void, ms: number) => ReturnType<typeof setTimeout>;
  clear?: (timer: ReturnType<typeof setTimeout>) => void;
}): BufferingLease | null {
  const maxMs = Math.min(12_000, options.maxMs);
  if (!Number.isFinite(maxMs) || maxMs <= 0 || !options.isCurrent() || !engine.loadingEnabled || !engine.bufferingEnabled) return null;
  const schedule = options.schedule ?? setTimeout, clear = options.clear ?? clearTimeout;
  const pause = engine.pauseBuffering, resume = engine.resumeBuffering;
  const pauseDescriptor = Object.getOwnPropertyDescriptor(engine, "pauseBuffering");
  const resumeDescriptor = Object.getOwnPropertyDescriptor(engine, "resumeBuffering");
  let live = true, owned = true;
  let timer: ReturnType<typeof setTimeout> | undefined;
  const restore = () => {
    const intact = engine.pauseBuffering === externalPause && engine.resumeBuffering === externalResume;
    if (engine.pauseBuffering === externalPause) {
      if (pauseDescriptor) Object.defineProperty(engine, "pauseBuffering", pauseDescriptor);
      else Reflect.deleteProperty(engine, "pauseBuffering");
    }
    if (engine.resumeBuffering === externalResume) {
      if (resumeDescriptor) Object.defineProperty(engine, "resumeBuffering", resumeDescriptor);
      else Reflect.deleteProperty(engine, "resumeBuffering");
    }
    return intact;
  };
  const release = (reason = "released", shouldResume = true) => {
    if (!live) return;
    live = false;
    if (timer !== undefined) clear(timer);
    timer = undefined;
    const intact = restore();
    // 先让调度器投递精准取消，再恢复当前填充；取消 ACK 不延长本租约。
    try { options.onEnd(reason); } catch { /* 租约清理不能被观测/回调中断 */ }
    if (shouldResume && owned && intact && options.isCurrent() && (options.canResume?.() ?? true) && engine.loadingEnabled && !engine.bufferingEnabled) {
      resume.call(engine);
    }
  };
  function externalPause() {
    owned = false;
    try { pause.call(engine); } finally { release("external_pause", false); }
  }
  function externalResume() {
    owned = false;
    try { resume.call(engine); } finally { release("external_resume", false); }
  }
  try {
    engine.pauseBuffering = externalPause;
    engine.resumeBuffering = externalResume;
    pause.call(engine);
    if (engine.bufferingEnabled) { release("not_paused", false); return null; }
    timer = schedule(() => release("timeout"), maxMs);
  } catch {
    release("acquire_failed");
    return null;
  }
  return { release, active: () => live && owned };
}
