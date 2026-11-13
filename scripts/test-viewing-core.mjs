import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { createSeekPrefetch } from '../src/seekPrefetch.ts';
import { createMediaSessionConfirmation, waitForLatestMediaSession } from '../src/mediaSessionConfirmation.ts';
import { mediaElementSource, matchesMediaElementSource } from '../src/mediaElementSource.ts';
import { createPrefetchTransport } from '../src/prefetchTransport.ts';
import { createPlaybackIntent } from '../src/playbackIntent.ts';
import { createWatchHistoryStore, updateWatchHistory } from '../src/watchHistory.ts';
import { createWatchProgressSession } from '../src/watchProgressSession.ts';
import { diagnosticCause, playbackCause } from '../src/playbackCause.ts';

const completed = [];
const test = async (name, fn) => { await fn(); completed.push(name); };
const settle = async () => { for (let i = 0; i < 8; i++) await Promise.resolve(); };
const hls = { currentLevel: 0, levels: [{ details: { fragments: [
  { start: 0, duration: 10, url: 'http://hg.localhost/seg?u=https%3A%2F%2Ffixture.invalid%2Fa.ts' },
  { start: 10, duration: 10, url: 'http://hg.localhost/seg?u=https%3A%2F%2Ffixture.invalid%2Fb.ts' },
] } }] };
function seekFixture(overrides = {}) {
  let now = 0, seq = 0, buffered = false;
  const timers = new Map(), calls = [];
  const deps = { now: () => now, schedule: (fn, ms) => { const id = ++seq; timers.set(id, { fn, at: now + ms }); return id; }, clear: id => timers.delete(id),
    isBuffered: () => buffered,
    prefetch: async url => { calls.push(['prefetch', url]); return true; }, cancel: async url => { calls.push(['cancel', url]); }, ...overrides };
  const controller = createSeekPrefetch(() => hls, deps);
  return { controller, calls, buffered: value => { buffered = value; }, timers,
    async tick(ms) { const end = now + ms; let count = 0; for (;;) { const next = [...timers].sort((a, b) => a[1].at - b[1].at)[0]; if (!next || next[1].at > end) break; assert.ok(++count < 100); now = next[1].at; timers.delete(next[0]); next[1].fn(); await settle(); } now = end; await settle(); } };
}
await test('buffered preview and commit do not dispatch or cancel backend work', async () => {
  const f = seekFixture(); f.buffered(true); f.controller.at(3); f.controller.commit(3); await f.tick(1000);
  assert.deepEqual(f.calls, []); assert.equal(f.timers.size, 0);
});
await test('debounce rechecks buffer after target becomes available', async () => {
  const f = seekFixture(); f.controller.at(3); f.buffered(true); await f.tick(120); assert.deepEqual(f.calls, []);
});
await test('same cold fragment hands off without duplicate RPC; precise source cancel', async () => {
  const f = seekFixture(); f.controller.at(2); await f.tick(120); f.controller.commit(3); await settle();
  assert.equal(f.calls.length, 1); f.controller.commit(12); await settle(); assert.equal(f.calls.length, 2);
  f.controller.cancel(); assert.deepEqual(f.calls.at(-1), ['cancel', 'https://fixture.invalid/b.ts']);
});
await test('buffered target preserves already dispatched current consumer', async () => {
  const f = seekFixture(); f.controller.commit(3); await settle(); f.buffered(true); f.controller.at(4); f.controller.commit(4); await f.tick(1000);
  assert.equal(f.calls.length, 1); // 不因快路径额外发取消RPC。
});
await test('failed same-target preview retries once with cooldown, never loops automatically', async () => {
  let attempts = 0; const f = seekFixture({ prefetch: async () => { attempts++; return false; } });
  f.controller.commit(3); await settle(); f.controller.commit(3); await f.tick(299); assert.equal(attempts, 1);
  await f.tick(1); assert.equal(attempts, 2); f.controller.commit(3); await f.tick(10000); assert.equal(attempts, 2); assert.equal(f.timers.size, 0);
});
await test('cancelled debounce is inert and late accepted ACK cannot own next target', async () => {
  let release; const calls = [];
  const f = seekFixture({ prefetch: url => { calls.push(['prefetch', url]); return new Promise(resolve => { release = resolve; }); }, cancel: async url => { calls.push(['cancel', url]); } });
  f.controller.at(3); f.controller.cancel(); await f.tick(120); assert.equal(calls.length, 0);
  f.controller.commit(3); f.controller.cancel(); release(true); await settle(); assert.ok(calls.slice(1).every(row => row[0] === 'cancel' && row[1].endsWith('/a.ts')));
});
await test('intent transition ignores stale paused ended source while awaiting session ACK', () => {
  const intent = createPlaybackIntent('E1', true); intent.transition('E2', true);
  intent.suspend({ identity: 'E1', wasActive: true, ready: true, paused: true, ended: true }); assert.equal(intent.wanted(), true);
});
await test('same-identity codec/retry preserves explicit pause; new episode uses declared autoplay', () => {
  const intent = createPlaybackIntent('E1', true); intent.set(false); intent.transition('E1', true); assert.equal(intent.wanted(), false);
  assert.equal(intent.mayResume(true), false); intent.transition('E2', true); assert.equal(intent.mayResume(true), true); assert.equal(intent.mayResume(false), false);
});
await test('native pause snapshot only comes from actual previously active non-ended same source', () => {
  const intent = createPlaybackIntent('E1', true);
  intent.suspend({ identity: 'E1', wasActive: false, ready: true, paused: true, ended: false }); assert.equal(intent.wanted(), true);
  intent.suspend({ identity: 'E1', wasActive: true, ready: true, paused: true, ended: false }); assert.equal(intent.wanted(), false);
});
await test('history explicit episode time wins; omitted cross-episode duration resets', () => {
  const initial = [{ id: 1, ep: 1, t: 55, dur: 300, at: 1 }];
  const next = updateWatchHistory(initial, 1, 2, 37, undefined, 2); assert.equal(next[0].t, 37); assert.equal(next[0].dur, 0);
  assert.equal(updateWatchHistory(next, 1, 2, 37, 0, 3), next);
  assert.equal(updateWatchHistory(next, 1, 3, undefined, undefined, 4)[0].t, 0);
});
await test('history subscriptions only notify actual changes and persistence is synchronous', () => {
  const written = [], seen = []; const store = createWatchHistoryStore([], history => written.push(history));
  const stop = store.subscribe(() => seen.push(store.getSnapshot())); store.add(1, 2, 37, 300);
  assert.equal(written.length, 1); assert.equal(seen.length, 1); store.add(1, 2, 37, 300); assert.equal(seen.length, 1);
  stop(); store.add(1, 2, 38, 300); assert.equal(seen.length, 1); assert.equal(written.length, 2);
});
await test('source handoff flushes outgoing before incoming and cold exit cannot restore old episode', () => {
  let now = 0; const store = createWatchHistoryStore([]); const tracker = createWatchProgressSession(store.add, () => now);
  tracker.transition(1, 1); tracker.duration('1:1', 300); tracker.time('1:1', 55);
  tracker.transition(1, 2); assert.equal(store.getSnapshot()[0].ep, 2); assert.equal(store.getSnapshot()[0].t, 0);
  tracker.time('1:1', 99); tracker.duration('1:1', 999); tracker.flush(); assert.equal(store.getSnapshot()[0].ep, 2); assert.equal(store.getSnapshot()[0].dur, 0);
});
await test('resume initial time survives identity establishment; pause/pagehide flush deduplicates', () => {
  let now = 0; const written = []; const tracker = createWatchProgressSession((...args) => written.push(args), () => now);
  tracker.transition(1, 2, 37); assert.equal(written.at(-1)[2], 37);
  tracker.duration('1:2', 300); tracker.time('1:2', 37.4); tracker.flush(); const count = written.length;
  tracker.flush(); tracker.flush(); assert.equal(written.length, count);
  now = 5001; tracker.time('1:2', 42); assert.equal(written.at(-1)[2], 42);
});
await test('diagnostic fixed cause map matches Rust whitelist and strips arbitrary text', async () => {
  const source = await readFile(new URL('../src/playbackCause.ts', import.meta.url), 'utf8');
  const codes = [...source.matchAll(/\b[a-z_]+:\s*(\d+)/g)].map(match => Number(match[1]));
  assert.equal(new Set(codes).size, 47); assert.deepEqual(codes, [...Array.from({ length: 23 }, (_, i) => i + 1), ...Array.from({ length: 9 }, (_, i) => i + 101), ...Array.from({ length: 15 }, (_, i) => i + 201)]);
  assert.deepEqual(diagnosticCause({ reason: 'foreground', state: 'busy' }), { state: 'busy', cause_code: 7 });
  assert.deepEqual(diagnosticCause({ reason: 'https://secret.invalid/token', x: 1 }), { x: 1 }); assert.equal(playbackCause('constructor'), undefined);
});
await test('same raw target reentry waits for old cancel ACK before dispatch', async () => {
  let releaseCancel; const calls = [];
  const f = seekFixture({ prefetch: async url => { calls.push(['prefetch', url]); return true; }, cancel: url => { calls.push(['cancel', url]); return new Promise(resolve => { releaseCancel = resolve; }); } });
  f.controller.commit(3); await settle(); f.controller.cancel(); f.controller.commit(3); await settle();
  assert.equal(calls.filter(row => row[0] === 'prefetch').length, 1);
  releaseCancel(); await settle(); assert.equal(calls.filter(row => row[0] === 'prefetch').length, 2);
});
await test('shared transport preserves prefetch cancel prefetch ordering across remounts and signatures', async () => {
  let release; const calls = [];
  const transport = createPrefetchTransport(async url => { calls.push(['prefetch', url]); return true; }, async url => { calls.push(['cancel', url]); await new Promise(resolve => { release = resolve; }); }, url => url.split('?')[0]);
  await transport.prefetch('https://fixture.invalid/a.ts?old'); const cancel = transport.cancel('https://fixture.invalid/a.ts?old');
  const next = transport.prefetch('https://fixture.invalid/a.ts?new'); await settle(); assert.equal(calls.length, 2);
  release(); await cancel; await next; assert.deepEqual(calls.map(row => row[0]), ['prefetch', 'cancel', 'prefetch']);
});
await test('rapid hide/show and same-source retry require newest real session ACK', async () => {
  const old = createMediaSessionConfirmation(); let current = old, finished = false;
  const waiting = waitForLatestMediaSession(() => current, () => false).then(value => { finished = value; });
  const next = createMediaSessionConfirmation(); current = next; old.acknowledge(); await settle(); assert.equal(finished, false);
  next.acknowledge(); await waiting; assert.equal(finished, true);
});
await test('stale session error is ignored but current error stays terminal; disposed ACK never binds', async () => {
  const old = createMediaSessionConfirmation(); let current = old;
  const waiting = waitForLatestMediaSession(() => current, () => false); const next = createMediaSessionConfirmation(); current = next;
  old.fail(new Error('old')); await settle(); next.fail(new Error('current')); await assert.rejects(waiting, /current/);
  const ticket = createMediaSessionConfirmation(); let disposed = false; const late = waitForLatestMediaSession(() => ticket, () => disposed);
  disposed = true; ticket.acknowledge(); assert.equal(await late, false);
});
await test('MSE video.src and MMS source-child binding both reject stale currentSrc', () => {
  const video = { src: 'blob:new', currentSrc: 'blob:old', getAttribute: () => 'blob:new', querySelector: () => null };
  assert.equal(matchesMediaElementSource(video, 'blob:new'), false); video.currentSrc = 'blob:new'; assert.equal(matchesMediaElementSource(video, 'blob:new'), true);
  const mms = { src: '', currentSrc: 'blob:mms', getAttribute: () => null, querySelector: () => ({ src: 'blob:mms' }) };
  assert.equal(mediaElementSource(mms), 'blob:mms'); assert.equal(matchesMediaElementSource(mms, 'blob:mms'), true);
});
console.log(JSON.stringify({ suite: 'viewing-core-pure', cases: completed.length, completed, network_requests: 0, real_waits: 0 }));
