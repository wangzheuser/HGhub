// 运行实际hook的最小生命周期模型；没有React DOM、浏览器、服务或媒体解码。
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import ts from 'typescript';
import { createPlaybackIntent } from '../src/playbackIntent.ts';

function fixture() {
  let cursor = 0, pending = [], slots = [], latestResult;
  const equal = (a, b) => a && b && a.length === b.length && a.every((value, i) => Object.is(value, b[i]));
  const hooks = {
    useRef(value) { const index = cursor++; return slots[index] ??= { current: value }; },
    useCallback(fn, deps) { const index = cursor++; const slot = slots[index]; if (slot && equal(deps, slot.deps)) return slot.fn; slots[index] = { fn, deps }; return fn; },
    useLayoutEffect(fn, deps) { const index = cursor++; const previous = slots[index]; if (previous && equal(deps, previous.deps)) return; pending.push(() => { previous?.cleanup?.(); slots[index] = { deps, cleanup: fn() }; }); },
  };
  const source = fixture.source;
  const compiled = ts.transpileModule(source, { compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2021 } }).outputText;
  const module = { exports: {} };
  new Function('require', 'module', 'exports', compiled)(name => {
    if (name === 'react') return hooks;
    if (name === './playbackIntent') return { createPlaybackIntent };
    throw Error('Unexpected runtime import: ' + name);
  }, module, module.exports);
  const events = new EventTarget(), playPromises = [], traces = [];
  const video = { readyState: 4, paused: false, ended: false, currentTime: 10,
    addEventListener: (...args) => events.addEventListener(...args), removeEventListener: (...args) => events.removeEventListener(...args),
    play() { this.paused = false; return new Promise(resolve => playPromises.push(resolve)); },
    pause() { this.paused = true; },
  };
  const hls = { levels: [1], loadingEnabled: true, startLoad() { this.loadingEnabled = true; }, stopLoad() { this.loadingEnabled = false; } };
  return { video, hls, traces, playPromises,
    render(props) { cursor = 0; pending = []; latestResult = module.exports.usePlaybackActivity({ current: video }, { current: hls }, { active: true, autoPlay: true, src: 'E1', trace: (...args) => traces.push(args), ...props }); for (const effect of pending) effect(); return latestResult; },
  };
}
fixture.source = await readFile(new URL('../src/usePlaybackActivity.ts', import.meta.url), 'utf8');
const completed = [], test = async (name, fn) => { await fn(); completed.push(name); };
await test('new episode ACK wait does not inherit ended old element pause', () => {
  const f = fixture(); f.render({}); f.video.paused = true; f.video.ended = true;
  const pending = f.render({ src: 'E2', active: false }); assert.equal(pending.playWanted(), true);
  const ready = f.render({ src: 'E2', active: true }); assert.equal(ready.playWanted(), true); assert.ok(f.traces.some(([event]) => event === 'play.request'));
});
await test('codec and page suspend keep manual pause and never force playback', () => {
  const f = fixture(); const first = f.render({}); first.pause(); f.render({ active: false }); const next = f.render({ active: true });
  assert.equal(next.playWanted(), false); assert.equal(f.video.paused, true);
});
await test('late same-source play completion respects a later explicit pause', async () => {
  const f = fixture(); const activity = f.render({}); activity.play('test'); activity.pause(); f.playPromises.at(-1)(); await Promise.resolve();
  assert.equal(f.video.paused, true); assert.equal(activity.playWanted(), false);
});
await test('old-source play completion never pauses newly playing identity', async () => {
  const f = fixture(); const activity = f.render({}); activity.play('old'); const resolve = f.playPromises.at(-1);
  f.render({ src: 'E2', active: false }); const next = f.render({ src: 'E2', active: true }); next.play('new');
  resolve(); await Promise.resolve(); assert.equal(f.video.paused, false);
});
await test('same-identity session ACK pending blocks explicit and automatic playback', () => {
  const f = fixture(); f.render({}); const pending = f.render({ active: false }); const before = f.playPromises.length;
  pending.play('retry', true); pending.play('handle'); assert.equal(f.playPromises.length, before); assert.equal(f.hls.loadingEnabled, false);
});
console.log(JSON.stringify({ suite: 'playback-activity-hook-model', cases: completed.length, completed, network_requests: 0, browser_processes: 0, limits: 'hook model only, not DOM/React scheduler/media-event integration' }));
