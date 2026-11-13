export type WatchProgress = { id: number; ep: number; t: number; dur: number; at: number };

/** 显式时间优先；跨集缺省位置/时长归零，重复边界flush不制造新写入。 */
export function updateWatchHistory(history: WatchProgress[], id: number, ep?: number, time?: number, duration?: number, now = Date.now()): WatchProgress[] {
  if (!Number.isFinite(id) || id <= 0) return history;
  const previous = history.find(item => item.id === id);
  const nextEp = ep !== undefined && Number.isFinite(ep) && ep > 0 ? ep : previous?.ep ?? 1;
  const changed = !!previous && previous.ep !== nextEp;
  const t = time !== undefined && Number.isFinite(time) ? Math.max(0, time) : changed ? 0 : previous?.t ?? 0;
  const dur = duration !== undefined && Number.isFinite(duration) ? Math.max(0, duration) : changed ? 0 : previous?.dur ?? 0;
  if (history[0]?.id === id && previous?.ep === nextEp && previous.t === t && previous.dur === dur) return history;
  return [{ id, ep: nextEp, t, dur, at: now }, ...history.filter(item => item.id !== id)].slice(0, 80);
}

/** 高频历史写入独立订阅，不更新偏好/导航Context；持久化在边界同步完成。 */
export function createWatchHistoryStore(initial: WatchProgress[], persist: (history: WatchProgress[]) => void = () => {}) {
  let value = initial;
  const listeners = new Set<() => void>();
  const publish = (next: WatchProgress[]) => {
    if (next === value) return;
    value = next;
    try { persist(next); } catch { /* 存储配额/隐私模式不阻断播放。 */ }
    listeners.forEach(listener => listener());
  };
  return {
    getSnapshot: () => value,
    subscribe: (listener: () => void) => { listeners.add(listener); return () => { listeners.delete(listener); }; },
    add: (id: number, ep?: number, time?: number, duration?: number) => publish(updateWatchHistory(value, id, ep, time, duration)),
    clear: () => { if (value.length) publish([]); },
  };
}
