/** 同 tick 合并意图，但以真正 ACK 的 active root 为基准处理旧组件 cleanup。 */
export function createMediaSessionQueue(send: (url: string, active: boolean) => Promise<void>) {
  type Update = { url: string; active: boolean };
  type Waiter = { resolve: () => void; reject: (error: unknown) => void };
  let updates: Update[] = [], waiters: Waiter[] = [];
  let armed = false;
  // undefined 是尚未观察到 backend 状态；null 是已确认不活动。
  let acknowledged: string | null | undefined;
  let tail: Promise<void> = Promise.resolve();
  const flush = () => {
    armed = false;
    const batch = updates, callers = waiters;
    updates = []; waiters = [];
    if (!batch.length) return;
    const request = tail.then(async () => {
      // 排到真正发送时才 reduce：前一个批次可能还未 ACK。
      const before = acknowledged;
      let desired = before;
      let lastTrue: string | undefined;
      const falseUrls: string[] = [];
      for (const update of batch) {
        if (update.active) { desired = update.url; lastTrue = update.url; }
        else {
          falseUrls.push(update.url);
          if (desired === update.url) desired = null;
        }
      }
      const dispatch = async (url: string, active: boolean) => {
        await send(url, active);
        if (active) acknowledged = url;
        else if (acknowledged === url) acknowledged = null;
      };
      if (typeof desired === "string") {
        // true(B) 后面的 false(A) 是过期 cleanup，不得盖掉 B。
        await dispatch(desired, true);
      } else if (typeof before === "string") {
        // ACK active=A，batch true(B)→false(B)：真正该关闭的是后端仍在播的 A。
        await dispatch(before, false);
      } else if (desired === null && before === undefined && lastTrue) {
        // 初始未知且批次最终 inactive：先确立一个已知 root，再精确关闭它。
        await dispatch(lastTrue, true);
        await dispatch(lastTrue, false);
      } else {
        // 初始未知时，false(A)、false(B) 各自可能有效，保留其真实 ACK。
        const targets = before === undefined ? [...new Set(falseUrls)] : falseUrls.slice(-1);
        for (const url of targets) await dispatch(url, false);
      }
    });
    tail = request.catch(() => {});
    void request.then(() => callers.forEach(w => w.resolve()), error => callers.forEach(w => w.reject(error)));
  };
  return (url: string, active: boolean): Promise<void> => new Promise((resolve, reject) => {
    updates.push({ url, active }); waiters.push({ resolve, reject });
    if (!armed) { armed = true; queueMicrotask(flush); }
  });
}
