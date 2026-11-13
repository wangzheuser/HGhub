type Position = { key: string; id: number; ep: number; t: number; dur: number };

/** 单个观看页的身份交接：旧源晚到更新、重复cleanup都不覆盖当前记录。 */
export function createWatchProgressSession(save: (id: number, ep: number, time: number, duration: number) => void, now = Date.now) {
  let current: Position = { key: "", id: 0, ep: 1, t: 0, dur: 0 };
  let dirty = false, lastFlush = 0;
  const flush = () => {
    if (!dirty || !current.id) return;
    save(current.id, current.ep, current.t, current.dur);
    dirty = false; lastFlush = now();
  };
  return {
    snapshot: () => current,
    transition(id: number, ep: number, initialTime = 0) {
      const key = `${id}:${ep}`;
      if (key === current.key) return;
      flush();
      current = { key, id, ep, t: Math.max(0, Number.isFinite(initialTime) ? initialTime : 0), dur: 0 };
      dirty = true; flush();
    },
    time(key: string, time: number) {
      if (key !== current.key || !Number.isFinite(time) || time < 0) return;
      if (current.t !== time) { current = { ...current, t: time }; dirty = true; }
      if (current.dur > 0 && now() - lastFlush >= 5000) flush();
    },
    duration(key: string, duration: number) {
      if (key !== current.key || !Number.isFinite(duration) || duration < 0 || current.dur === duration) return;
      current = { ...current, dur: duration }; dirty = true;
    },
    flush,
  };
}
