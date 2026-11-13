/** 不依赖DOM的播放意图：跨内容采用显式策略，同内容换编码/暂时失活保留意图。 */
export function createPlaybackIntent(initialIdentity: string, initialWanted: boolean) {
  let identity = initialIdentity, wanted = initialWanted;
  return {
    identity: () => identity,
    wanted: () => wanted,
    set: (next: boolean) => { wanted = next; },
    transition(nextIdentity: string, autoPlay: boolean) {
      if (nextIdentity === identity) return false;
      identity = nextIdentity; wanted = autoPlay;
      return true;
    },
    suspend(snapshot: { identity: string; wasActive: boolean; ready: boolean; paused: boolean; ended: boolean }) {
      // 新源的会话ACK等待期不可读取旧元素paused，ended也不代表主动暂停。
      if (snapshot.identity === identity && snapshot.wasActive && snapshot.ready && !snapshot.ended) wanted = !snapshot.paused;
    },
    mayResume: (active: boolean) => active && wanted,
  };
}
