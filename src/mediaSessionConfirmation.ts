export function createMediaSessionConfirmation() {
  let acknowledge!: () => void, fail!: (error: unknown) => void;
  const ready = new Promise<void>((resolve, reject) => { acknowledge = resolve; fail = reject; });
  void ready.catch(() => {});
  return { ready, acknowledge, fail };
}
/** 等到最新一轮真实ACK；等待期间active/retry变化不复用旧确认，也不忙轮询。 */
export async function waitForLatestMediaSession<T extends { ready: Promise<void> }>(latest: () => T, disposed: () => boolean): Promise<boolean> {
  while (!disposed()) {
    const request = latest();
    try { await request.ready; } catch (error) { if (latest() === request) throw error; else continue; }
    if (latest() === request) return !disposed();
  }
  return false;
}
