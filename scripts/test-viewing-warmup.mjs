// 纯模块回归：只使用注入状态/假时钟，无应用启动、网络、React、磁盘输出或真实等待。
// 由父任务串行运行：node --experimental-strip-types --max-old-space-size=128 scripts/test-viewing-warmup.mjs
import assert from 'node:assert/strict';
import {
  WARM_NEAR_END_SECONDS, isWarmNearEnd, nextDestination, chooseWarmCandidate,
  createWarmupScheduler, bufferingLeaseMs, mediaBudget,
} from '../src/warmupScheduler.ts';
import { createBufferingLease } from '../src/bufferingLease.ts';

const completed = [];
const flush = async () => { for (let i = 0; i < 32; i++) await Promise.resolve(); };
function clock() {
  let ms = 0, sequence = 0, maxTimers = 0;
  const jobs = new Map();
  return {
    now: () => ms, jobs, maxTimers: () => maxTimers,
    schedule(fn, delay) {
      const id = ++sequence;
      jobs.set(id, { at: ms + delay, fn });
      maxTimers = Math.max(maxTimers, jobs.size);
      return id;
    },
    clear: id => jobs.delete(id),
    async advance(delta) {
      const end = ms + delta;
      let iterations = 0;
      for (;;) {
        let next;
        for (const entry of jobs) if (entry[1].at <= end && (!next || entry[1].at < next[1].at)) next = entry;
        if (!next) break;
        assert.ok(++iterations < 10_000, 'fake timer execution is bounded');
        ms = next[1].at; jobs.delete(next[0]); next[1].fn(); await flush();
      }
      ms = end; await flush();
    },
  };
}
const full = (overrides = {}) => ({ state: 'ready', playlist_ready: true, segment_ready: true, dependencies_ready: true, generation: 1, ...overrides });
const base = {
  active: true, currentUrl: 'current.m3u8', candidate: { key: '1:3', id: 1, ep: 3 }, policy: 'avc',
  time: 10, duration: 600, buffered: 90, rate: 1, waiting: false, seeking: false, foreground: false,
};
function harness(overrides = {}, initial = {}) {
  const timer = clock(), events = [], calls = { resolve: [], prepare: [], status: [], warm: [], cancel: [] };
  let snapshot = { ...base, ...initial };
  const invoke = (name, value, fallback) => {
    calls[name].push({ value, at: timer.now() });
    return overrides[name] ? overrides[name](value, timer) : fallback;
  };
  const scheduler = createWarmupScheduler({
    ...timer,
    resolve: async candidate => invoke('resolve', candidate, 'target.m3u8'),
    prepare: async url => invoke('prepare', url, true),
    status: async url => invoke('status', url, full()),
    warm: async url => invoke('warm', url, true),
    cancel: async url => invoke('cancel', url, undefined),
    acquireBufferingLease: overrides.acquireBufferingLease,
    trace: (event, data) => events.push({ event, data, at: timer.now() }),
  });
  return {
    clock: timer, scheduler, calls, events,
    update(patch = {}) { snapshot = { ...snapshot, ...patch }; scheduler.update(snapshot); },
    count: name => calls[name].length,
    exhausted: () => events.filter(e => e.event === 'warm.exhausted'),
    windows: () => events.filter(e => e.event === 'warm.window'),
    async stop() { scheduler.stop(); await flush(); assert.equal(timer.jobs.size, 0, 'cleanup leaves no scheduler/lease timer'); },
  };
}
async function test(name, run) { await run(); completed.push(name); }
async function watch(h, seconds, position = second => ({ time: second, buffered: second + 90 })) {
  for (let second = 0; second < seconds; second++) { h.update(position(second)); await h.clock.advance(1000); }
}
function deferred() {
  let resolve;
  const promise = new Promise(done => { resolve = done; });
  return { promise, resolve };
}

await test('destination: ended stops at series end, Next may advance video', () => {
  const input = { id: 1, ep: 2, episodes: [1, 2, 3], videoIds: [9, 1, 8], index: 1, epMap: { 8: 4 }, swipeEp: false, direction: -1 };
  assert.equal(nextDestination(input)?.key, '1:3');
  assert.equal(nextDestination({ ...input, ep: 3 }), null);
  assert.equal(nextDestination({ ...input, ep: 3, allowNextVideo: true })?.key, '8:4');
  assert.equal(nextDestination({ ...input, ep: 3, index: 2, allowNextVideo: true }), null);
  assert.equal(nextDestination({ ...input, episodes: [1, 2, 4] }), null, 'do not invent episode 3 across a gap');
  assert.equal(chooseWarmCandidate(input)?.key, '9:1', 'early backward cross-video intent remains intact');
  assert.equal(chooseWarmCandidate({ ...input, direction: 1 })?.key, '8:4');
  assert.equal(chooseWarmCandidate({ ...input, swipeEp: true })?.key, '1:1');
  assert.equal(chooseWarmCandidate({ ...input, swipeEp: true, direction: 1 })?.key, '1:3');
  for (const swipeEp of [true, false]) for (const direction of [-1, 1]) {
    assert.equal(chooseWarmCandidate({ ...input, swipeEp, direction, nearEnd: true })?.key, '1:3');
    assert.equal(chooseWarmCandidate({ ...input, swipeEp, direction, nearEnd: true, intentEp: 1 })?.key, '1:1');
  }
  assert.equal(chooseWarmCandidate({ ...input, ep: 3, nearEnd: true })?.key, '9:1', 'series-end fallback keeps ordinary direction, not new autoplay');
  assert.equal(chooseWarmCandidate({ ...input, nearEnd: true, intentEp: 99 })?.key, '1:3', 'invalid panel intent is ignored');
  assert.equal(chooseWarmCandidate({ ...input, nearEnd: true, intentEp: 2 })?.key, '1:3', 'current-episode intent is not a different destination');
});

await test('near-end is a shared 45-second wall-clock boundary', () => {
  assert.equal(WARM_NEAR_END_SECONDS, 45);
  assert.equal(isWarmNearEnd({ time: 555, duration: 600, rate: 1 }), true);
  assert.equal(isWarmNearEnd({ time: 554.99, duration: 600, rate: 1 }), false);
  assert.equal(isWarmNearEnd({ time: 510, duration: 600, rate: 2 }), true);
  assert.equal(isWarmNearEnd({ time: 510, duration: 600, rate: 1 }), false);
  assert.equal(isWarmNearEnd({}), false);
  assert.equal(isWarmNearEnd({ time: 600, duration: 600 }), false);
  assert.equal(isWarmNearEnd({ time: NaN, duration: 600 }), false);
  assert.equal(isWarmNearEnd({ time: 599, duration: 600, nearEnd: false }), false);
  assert.equal(isWarmNearEnd({ nearEnd: true }), true);
});

await test('20-minute normal VOD: early ready sleeps and wakes near the actual ending', async () => {
  let expires = 0;
  const h = harness({
    prepare: (_url, timer) => { expires = timer.now() + 60_000; return true; },
    status: (_url, timer) => full({ state: timer.now() < expires ? 'ready' : 'idle', playlist_ready: timer.now() < expires }),
  }, { time: 0, duration: 1200 });
  await watch(h, 1100);
  assert.equal(h.count('resolve'), 1);
  assert.equal(h.count('prepare'), 1);
  assert.equal(h.count('status'), 1, 'no 30-second whole-episode keepalive');
  assert.equal(h.clock.jobs.size, 0);
  await watch(h, 100, second => ({ time: 1100 + second, buffered: Math.min(1200, 1190 + second) }));
  assert.equal(h.windows().length, 1);
  assert.equal(h.windows()[0].data.reason, 'near_end');
  assert.equal(h.count('resolve'), 2, 'refresh the expired address near the actual destination deadline');
  assert.equal(h.count('prepare'), 2);
  assert.ok(h.count('status') >= 2 && h.count('status') <= 4);
  assert.equal(h.exhausted().length, 0);
  assert.equal(h.count('warm'), 0, 'valid completed head/dependencies need no new heavy request');
  assert.equal(h.clock.maxTimers(), 1);
  await h.stop();
});

await test('seventh-minute near-end window refreshes expired raw and restores an evicted head once', async () => {
  let expires = 0, head = true;
  const h = harness({
    prepare: (_url, timer) => { expires = timer.now() + 60_000; return true; },
    status: (_url, timer) => full({ state: head && timer.now() < expires ? 'ready' : 'idle', playlist_ready: timer.now() < expires, segment_ready: head }),
    warm: () => { head = true; return true; },
  }, { time: 0, duration: 465 });
  await watch(h, 420);
  assert.equal(h.count('status'), 1);
  head = false;
  h.update({ time: 420, buffered: 465 }); await h.clock.advance(1800);
  assert.equal(h.count('resolve'), 2); assert.equal(h.count('prepare'), 2);
  assert.equal(h.count('warm'), 1, 'only the actually missing head needs a heavy admission');
  assert.equal(h.events.filter(e => e.event === 'warm.ready').length, 2);
  assert.equal(h.exhausted().length, 0);
  await h.stop();
});

await test('successful short-TTL maintenance exceeds four refreshes but stops its bounded window', async () => {
  let expires = 0;
  const h = harness({
    resolve: (_candidate, timer) => ({ url: 'short-ttl.m3u8', expiresAt: timer.now() + 15_000 }),
    prepare: (_url, timer) => { expires = timer.now() + 15_000; return true; },
    status: (_url, timer) => full({ state: timer.now() < expires ? 'ready' : 'idle', playlist_ready: timer.now() < expires }),
  }, { nearEnd: true });
  await watch(h, 1200, second => ({ time: second, duration: 1800, buffered: second + 90 }));
  assert.ok(h.count('prepare') > 5, 'normal TTL successes are not the old cumulative four-refresh failure budget');
  assert.ok(h.count('prepare') <= 9 && h.count('status') <= 9, 'one window is still at most 120 seconds, not infinite maintenance');
  assert.equal(h.exhausted().length, 0);
  assert.equal(h.clock.jobs.size, 0);
  assert.equal(h.clock.maxTimers(), 1);
  await h.stop();
});

await test('real prepare failure is bounded; near-end and each new intent reopen only once', async () => {
  const h = harness({ prepare: () => false }, { nearEnd: false });
  h.update(); await h.clock.advance(10_000);
  assert.equal(h.count('prepare'), 4);
  assert.equal(h.exhausted().length, 1);
  await watch(h, 600, second => ({ time: second, duration: 1800, buffered: second + 90 }));
  assert.equal(h.count('prepare'), 4, 'ordinary timeupdates do not replenish failed preparation');
  h.update({ nearEnd: true }); await h.clock.advance(10_000);
  assert.equal(h.count('prepare'), 8);
  assert.equal(h.windows().length, 1);
  for (let i = 0; i < 10; i++) {
    h.update({ nearEnd: false }); h.update({ nearEnd: true }); await h.clock.advance(1000);
  }
  assert.equal(h.count('prepare'), 8, 'threshold recrossing is not a new automatic window');
  h.update({ intentVersion: 7 }); await h.clock.advance(10_000);
  assert.equal(h.count('prepare'), 12);
  for (const intentVersion of [7, undefined, 7, 6, undefined, 7]) { h.update({ intentVersion }); await h.clock.advance(1000); }
  assert.equal(h.count('prepare'), 12, 'same/older sequence and cancellation do not rearm it');
  h.update({ intentVersion: 8 }); await h.clock.advance(10_000);
  assert.equal(h.count('prepare'), 16);
  assert.equal(h.windows().length, 3);
  assert.equal(h.exhausted().length, 4);
  assert.equal(h.clock.jobs.size, 0);
  await h.stop();
});

await test('four actual failed raw refreshes stay terminal across ordinary updates', async () => {
  let prepares = 0;
  const h = harness({
    prepare: () => ++prepares === 1,
    status: () => full({ state: 'busy', playlist_ready: false, segment_ready: false, dependencies_ready: false }),
  }, { nearEnd: false });
  h.update(); await h.clock.advance(10_000);
  assert.equal(h.count('resolve'), 1);
  assert.equal(h.count('prepare'), 5, 'initial success plus four failed raw refreshes');
  assert.equal(h.exhausted()[0]?.data.reason, 'playlist_refresh_failed');
  await watch(h, 600, second => ({ time: second, duration: 1800, buffered: second + 90 }));
  assert.equal(h.count('prepare'), 5);
  assert.equal(h.count('warm'), 0);
  await h.stop();
});

await test('successful prepare with permanently stale raw state still uses only 80 checks', async () => {
  const h = harness({ status: () => full({ state: 'busy', playlist_ready: false }) }, { nearEnd: false });
  h.update(); await h.clock.advance(125_000);
  assert.equal(h.count('status'), 80);
  assert.ok(h.count('prepare') <= 82);
  assert.equal(h.exhausted()[0]?.data.reason, 'admission_budget');
  assert.equal(h.clock.jobs.size, 0);
  await h.stop();
});

await test('distinct media failure generations have a four-failure budget', async () => {
  let generation = 0;
  const h = harness({ status: () => full({ state: 'failed', generation: ++generation }), warm: () => false }, { nearEnd: false });
  h.update(); await h.clock.advance(10_000);
  assert.equal(h.count('status'), 4);
  assert.equal(h.count('warm'), 3);
  assert.equal(h.exhausted()[0]?.data.reason, 'failure_budget');
  await h.stop();
});

await test('unsupported stays terminal even with near-end and new explicit windows', async () => {
  const h = harness({ status: () => full({ state: 'failed', reason: 'unsupported_audio_rendition' }) }, { nearEnd: false });
  h.update(); await h.clock.advance(300);
  for (let intentVersion = 1; intentVersion <= 6; intentVersion++) { h.update({ nearEnd: true, intentVersion }); await h.clock.advance(2000); }
  assert.equal(h.count('status'), 1);
  assert.equal(h.count('warm'), 0);
  assert.equal(h.windows().length, 0);
  assert.equal(h.clock.jobs.size, 0);
  await h.stop();
});

await test('busy admission keeps 80 checks/120 seconds without timeupdate reset', async () => {
  const h = harness({ status: () => full({ state: 'busy', segment_ready: false }) }, { nearEnd: false });
  await watch(h, 180, second => ({ time: second, duration: 1800, buffered: second + 90 }));
  assert.equal(h.count('status'), 80);
  assert.equal(h.count('warm'), 0);
  assert.equal(h.exhausted()[0]?.data.reason, 'admission_budget');
  assert.equal(h.clock.jobs.size, 0);
  assert.equal(h.clock.maxTimers(), 1);
  await h.stop();
});

await test('admitted media keeps 30-poll/45-second work budget', async () => {
  let admitted = false;
  const h = harness({
    status: () => full({ state: admitted ? 'queued' : 'idle', segment_ready: false, dependencies_ready: false }),
    warm: () => { admitted = true; return true; },
  }, { nearEnd: false });
  h.update(); await h.clock.advance(50_000);
  assert.equal(h.count('warm'), 1);
  assert.ok(h.count('status') <= 31);
  assert.equal(h.exhausted()[0]?.data.reason, 'media_budget');
  assert.ok(h.exhausted()[0].at <= 45_300);
  await h.stop();
});

await test('queued same-URL raw TTL refresh preserves in-flight media and no duplicate admission', async () => {
  let valid = false, state = 'busy', prepares = 0;
  const h = harness({
    prepare: () => { if (++prepares > 1) valid = true; return true; },
    status: () => full({ state, playlist_ready: valid, segment_ready: state === 'ready', dependencies_ready: state === 'ready' }),
  }, { nearEnd: false });
  h.update(); await h.clock.advance(300);
  assert.equal(h.count('prepare'), 2); assert.equal(h.count('warm'), 0);
  await h.clock.advance(1500);
  state = 'idle'; await h.clock.advance(1500);
  assert.equal(h.count('warm'), 1);
  state = 'queued'; valid = false; await h.clock.advance(1500);
  assert.equal(h.count('prepare'), 3);
  assert.equal(h.count('resolve'), 1);
  assert.equal(h.count('warm'), 1); assert.equal(h.count('cancel'), 0);
  state = 'ready'; await h.clock.advance(1500);
  assert.equal(h.events.filter(e => e.event === 'warm.ready').length, 1);
  await h.stop();
});

await test('exact resolution TTL is not extended and changed signed URL prepares before old raw can look ready', async () => {
  const order = [];
  let resolutions = 0;
  const h = harness({
    resolve: (_candidate, timer) => ({ url: `target.m3u8?signature=${++resolutions}`, expiresAt: timer.now() + 5000 }),
    prepare: url => { order.push(`prepare:${url}`); return true; },
    status: url => { order.push(`status:${url}`); return full(); },
    cancel: url => { order.push(`cancel:${url}`); },
  }, { nearEnd: false });
  h.update(); await h.clock.advance(300);
  await h.clock.advance(5100); h.update(); await h.clock.advance(300);
  assert.equal(h.count('status'), 1, 'far from end, expiration sleeps rather than checking old raw');
  h.update({ intentVersion: 1 }); await h.clock.advance(300);
  assert.equal(h.count('resolve'), 2, '5-second supplied expiry must beat default 60-second maximum');
  const next = 'target.m3u8?signature=2';
  assert.ok(order.indexOf(`cancel:target.m3u8?signature=1`) < order.indexOf(`prepare:${next}`));
  assert.ok(order.indexOf(`prepare:${next}`) < order.indexOf(`status:${next}`));
  assert.equal(h.calls.status.at(-1).value, next);
  await h.stop();
});

await test('already-expired resolution results fail within four attempts without using old raw', async () => {
  const h = harness({ resolve: (_candidate, timer) => ({ url: 'stale.m3u8', expiresAt: timer.now() }) }, { nearEnd: false });
  h.update(); await h.clock.advance(10_000);
  assert.equal(h.count('resolve'), 4);
  assert.equal(h.count('prepare'), 0); assert.equal(h.count('status'), 0);
  assert.equal(h.exhausted()[0]?.data.reason, 'resolve_failed');
  await h.stop();
});

for (const stage of ['resolve', 'prepare', 'status']) {
  await test(`late ${stage} result from inactive identity cannot restore readiness/work`, async () => {
    const gate = deferred();
    const h = harness({ [stage]: () => gate.promise }, { nearEnd: false });
    h.update(); await h.clock.advance(300);
    assert.equal(h.count(stage), 1);
    h.update({ active: false });
    gate.resolve(stage === 'resolve' ? 'late.m3u8' : stage === 'prepare' ? true : full());
    await flush(); await h.clock.advance(5000);
    assert.equal(h.count('warm'), 0);
    assert.equal(h.events.filter(e => e.event === 'warm.ready').length, 0);
    if (stage === 'resolve') assert.equal(h.count('prepare'), 0);
    if (stage === 'prepare') assert.equal(h.count('status'), 0);
    assert.equal(h.clock.jobs.size, 0);
    await h.stop();
  });
}

await test('late prepare from previous explicit window is discarded before next window preparation', async () => {
  const gate = deferred(); let prepares = 0;
  const h = harness({ prepare: () => ++prepares === 1 ? gate.promise : true }, { nearEnd: false });
  h.update(); await h.clock.advance(300);
  h.update({ intentVersion: 1 }); gate.resolve(true); await flush();
  assert.equal(h.events.filter(e => e.event === 'warm.prepared').length, 0);
  await h.clock.advance(1500);
  assert.equal(h.count('prepare'), 2);
  assert.equal(h.events.filter(e => e.event === 'warm.ready').length, 1);
  await h.stop();
});

await test('late warm ACK is precisely cancelled and new heavy request awaits every cancellation ACK', async () => {
  const warmGate = deferred(), cancelGate = deferred();
  const h = harness({
    resolve: candidate => candidate.key,
    status: () => full({ state: 'idle', segment_ready: false }),
    warm: url => url === '1:3' ? warmGate.promise : false,
    cancel: url => url === '1:3' ? cancelGate.promise : undefined,
  }, { nearEnd: false });
  h.update(); await h.clock.advance(300);
  assert.equal(h.count('warm'), 1);
  h.update({ candidate: { key: '2:1', id: 2, ep: 1 } });
  warmGate.resolve(true); await flush(); await h.clock.advance(0);
  assert.equal(h.count('cancel'), 2, 'identity cancellation plus late accepted RPC cancellation both tracked');
  assert.equal(h.count('warm'), 1, 'new heavy work is gated on cancellation ACK, not merely enqueued cancel');
  cancelGate.resolve(); await flush();
  assert.equal(h.count('warm'), 2);
  assert.equal(h.calls.warm.at(-1).value, '2:1');
  assert.equal(h.clock.maxTimers(), 1);
  await h.stop();
});

function leaseEngine(events) {
  return {
    loadingEnabled: true, bufferingEnabled: true, pauses: 0, resumes: 0,
    pauseBuffering() { this.bufferingEnabled = false; this.pauses++; events.push('pause'); },
    resumeBuffering() { this.bufferingEnabled = true; this.resumes++; events.push('resume'); },
  };
}
await test('lease entry/hysteresis boundaries remain 20s/8s/12s and ready/wait/seek block entry', () => {
  const s = { ...base, time: 10, buffered: 30 };
  assert.equal(bufferingLeaseMs(s), 12_000);
  assert.equal(bufferingLeaseMs({ ...s, buffered: 29.99 }), 0);
  assert.equal(bufferingLeaseMs({ ...s, rate: 2 }), 0);
  assert.equal(bufferingLeaseMs({ ...s, buffered: 50, rate: 2 }), 12_000);
  for (const patch of [{ active: false }, { waiting: true }, { seeking: true }, { foreground: true }]) assert.equal(bufferingLeaseMs({ ...s, ...patch }), 0);
  assert.equal(bufferingLeaseMs(s, true), 0);
  assert.equal(mediaBudget(false, { ...s, buffered: 18 }), true);
  assert.equal(mediaBudget(true, { ...s, buffered: 14.1 }), true);
  assert.equal(mediaBudget(true, { ...s, buffered: 13.8 }), false);
});

await test('lease remains at most two attempts/30-second cooldown, cancel before resume, even after reopening', async () => {
  const order = [], engine = leaseEngine(order), requests = [];
  let h;
  h = harness({
    status: () => full({ state: 'busy', segment_ready: false }),
    cancel: () => { order.push('cancel'); },
    acquireBufferingLease: request => {
      requests.push({ at: h.clock.now(), maxMs: request.maxMs });
      return createBufferingLease(engine, { ...request, ...h.clock, isCurrent: () => true });
    },
  }, { time: 10, buffered: 30, nearEnd: false });
  h.update(); await h.clock.advance(300);
  assert.equal(requests.length, 1); assert.equal(engine.bufferingEnabled, false);
  h.update({ buffered: 18 });
  assert.ok(order.indexOf('cancel') < order.indexOf('resume'));
  assert.equal(engine.bufferingEnabled, true);
  h.update({ buffered: 30 }); await h.clock.advance(29_000);
  assert.equal(requests.length, 1);
  await h.clock.advance(1500); assert.equal(requests.length, 2);
  assert.ok(requests[1].at - 300 >= 30_000);
  assert.ok(requests.every(r => r.maxMs <= 12_000));
  await h.clock.advance(12_000);
  assert.equal(engine.resumes, 2);
  await h.clock.advance(90_000);
  assert.equal(h.exhausted()[0]?.data.reason, 'admission_budget');
  h.update({ intentVersion: 1 }); await h.clock.advance(3000);
  assert.equal(requests.length, 2, 'new validation budget never grants a third lease for the same candidate');
  assert.ok(h.clock.maxTimers() <= 2, 'at most the scheduler timer plus the owned lease timer');
  await h.stop();
});

await test('lease timeout remains exactly 12 seconds and release is idempotent', async () => {
  const timer = clock(), engine = leaseEngine([]), reasons = [];
  const lease = createBufferingLease(engine, { maxMs: 99_000, onEnd: reason => reasons.push(reason), ...timer, isCurrent: () => true });
  await timer.advance(11_999); assert.equal(engine.resumes, 0);
  await timer.advance(1); assert.equal(engine.resumes, 1); assert.deepEqual(reasons, ['timeout']);
  lease.release(); await timer.advance(30_000);
  assert.equal(engine.resumes, 1); assert.equal(timer.jobs.size, 0);
});

await test('ready lease releases without cancelling cached target; stale lease never resumes a replaced engine', async () => {
  const order = [], engine = leaseEngine(order); let ready = false, h;
  h = harness({
    status: () => full({ state: ready ? 'ready' : 'busy', segment_ready: ready }),
    acquireBufferingLease: request => createBufferingLease(engine, { ...request, ...h.clock, isCurrent: () => true }),
  }, { time: 10, buffered: 30, nearEnd: false });
  h.update(); await h.clock.advance(300); ready = true; await h.clock.advance(1500);
  assert.equal(engine.resumes, 1); assert.equal(h.count('cancel'), 0);
  await h.stop();
  const timer = clock(), old = leaseEngine([]), next = leaseEngine([]);
  let current = old;
  const lease = createBufferingLease(old, { maxMs: 50_000, onEnd() {}, ...timer, isCurrent: () => current === old });
  current = next; lease.release('source_change'); await timer.advance(12_000);
  assert.equal(old.resumes, 0); assert.equal(next.resumes, 0); assert.equal(timer.jobs.size, 0);
});

console.log(JSON.stringify({ suite: 'viewing-warmup-pure', cases: completed.length, completed, network_requests: 0, real_waits: 0 }));
