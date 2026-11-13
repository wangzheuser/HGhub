import Hls from "hls.js";
import { chooseWarmCandidate, createWarmupScheduler, mediaBudget, bufferingLeaseMs, bufferedWallSeconds, type WarmSnapshot } from "../src/warmupScheduler";
import { createBufferingLease } from "../src/bufferingLease";
import { createMediaSessionQueue } from "../src/mediaSessionQueue";
import { canonicalWarmupTarget } from "../src/mediaWarmupIdentity";
import { createRecoveryBudget, installHlsRecovery, createBufferedStallRecovery, type BufferedStallSnapshot } from "../src/playbackRecovery";
import { codecLabel, isCodecFailure, qualityLabel } from "../src/playbackQuality";
import { api, hasForegroundPlayRequest } from "../src/api";
import { bufferedEnd, createSeekPrefetch, healthyBuffer } from "../src/seekPrefetch";

const realPlay = api.play;
const realVideo = api.video;
const realMediaSession = api.mediaSession;
const realWarmupCancel = api.mediaWarmupCancel;
const check = (ok: unknown, message: string) => { if (!ok) throw new Error(message); };
const wait = (ms: number) => new Promise(resolve => setTimeout(resolve, ms));

/** 无网络控制器测试；UI/解码路径由 test-playback 的真实浏览器场景覆盖。 */
export async function optimizationUnitChecks() {
  await schedulerChecks();
  await bufferingLeaseChecks();
  await playlistTTLRefreshChecks();
  await sessionQueueChecks();
  recoveryQualityChecks();
  await bufferedStallChecks();
  await warmupCanonicalChecks();
  check(!healthyBuffer(10, 90, 17.8), "Under eight seconds must not warm next media");
  check(healthyBuffer(10, 90, 18), "Eight continuous seconds must allow warmup");
  check(healthyBuffer(87, 90, 90), "Near-end complete buffer must allow warmup");
  check(!healthyBuffer(90, 90, 90), "Ended media must not start a warmup");
  const video = { currentTime: 10, buffered: { length: 2, start: (i: number) => [0, 30][i], end: (i: number) => [12, 80][i] } } as HTMLVideoElement;
  check(bufferedEnd(video) === 12, "A later buffered island must not inflate current buffer health");
  const calls: { command: string; url?: string }[] = [];
  const prefetch = api.mediaPrefetch, cancel = api.mediaPrefetchCancel;
  api.mediaPrefetch = async url => { calls.push({ command: "prefetch", url }); return true; };
  api.mediaPrefetchCancel = async url => { calls.push({ command: "cancel", url }); };
  const hls = { currentLevel: 0, levels: [{ details: { fragments: [
    { start: 0, duration: 10, url: "http://hg.localhost/seg?u=https%3A%2F%2Ffixture.invalid%2Fa.ts" },
    { start: 10, duration: 10, url: "http://hg.localhost/seg?u=https%3A%2F%2Ffixture.invalid%2Fb.ts" },
  ] } }] } as unknown as Hls;
  try {
    const controller = createSeekPrefetch(() => hls);
    controller.at(2); controller.cancel(); await wait(150);
    check(calls.length === 0, "Cancelled debounce must not dispatch or cancel unrelated server tasks");
    controller.at(2); await wait(150);
    controller.commit(3);
    check(calls.length === 1, "Same-fragment release must hand off without duplicate prefetch or cancellation");
    controller.at(12); controller.commit(12);
    check(calls.length === 2 && calls[1].url?.endsWith("b.ts"), "Final target must flush immediately");
    controller.cancel();
    check(calls.length === 3 && calls[2].command === "cancel" && calls[2].url?.endsWith("b.ts"), "Cleanup must cancel exactly its last raw URL");
    await wait(150);
    check(calls.length === 3, "Source cleanup must clear pending timer");
  } finally { api.mediaPrefetch = prefetch; api.mediaPrefetchCancel = cancel; }

  const internals = (window as unknown as { __TAURI_INTERNALS__: { invoke: (command: string, args?: Record<string, unknown>) => Promise<unknown> } }).__TAURI_INTERNALS__;
  const invoke = internals.invoke;
  let requests = 0, details = 0;
  let release!: () => void;
  const gate = new Promise<void>(resolve => { release = resolve; });
  internals.invoke = async (command, args) => {
    if (command === "line_set") return "fixture-line";
    if (command === "api_video") { details++; await gate; return { id: 81991 }; }
    if (command !== "api_play") return invoke(command, args);
    requests += 1;
    await gate;
    return { id: 81991, ep_num: 1, video_url: "https://fixture.invalid/dedup.m3u8" };
  };
  try {
    const first = realPlay(81991, 1, true), second = realPlay(81991, 1);
    check(hasForegroundPlayRequest(), "Foreground consumer of a speculative Promise must hold priority");
    check(requests === 1, "Concurrent play URL requests must be deduplicated");
    const detailFirst = realVideo(81991), detailSecond = realVideo(81991);
    check(details === 1, "Concurrent detail requests must share one Promise without serializing play");
    release(); await Promise.all([first, second, detailFirst, detailSecond]);
    check(!hasForegroundPlayRequest(), "Settled main request must release foreground priority");
    await realPlay(81991, 1);
    check(requests === 1, "Settled response must survive component cleanup within TTL");
    const now = Date.now;
    Date.now = () => now() + 60_001;
    try { await realPlay(81991, 1); } finally { Date.now = now; }
    check(requests === 2, "Expired playback URL must be fetched again");
    await realVideo(81991);
    check(details === 1, "Detail responses must survive within their short TTL");
    await api.lineSet("https://fixture.invalid/line");
    await Promise.all([realPlay(81991, 1), realVideo(81991)]);
    check(requests === 3 && details === 2, "Line changes must invalidate both detail and playback URL caches consistently");
  } finally { release(); internals.invoke = invoke; }
  return { buffer_gate: true, seek_handoff: true, exact_cancel: true, play_promise_dedup_and_ttl: true,
    two_stage_hysteresis: true, warmup_status_and_handoff: true, session_coalesce_real_ack: true, recovery_budget: true, actual_quality: true };
}

async function warmupCanonicalChecks() {
  const oldRoot = "https://tp12.drleao.cn:8443/videos/main/list.m3u8?auth_key=old&v=3&via=hgdj&x=a%2Fb+z";
  const newRoot = "https://tp3.drleao.cn:8443/videos/main/list.m3u8?v=3&auth_key=new&via=hgdj&x=a%2Fb+z";
  const expected = "https://tp.drleao.cn:8443/videos/main/list.m3u8?v=3&via=hgdj&x=a%2Fb+z";
  const canonical = canonicalWarmupTarget;
  check(canonical(oldRoot) === expected && canonical(newRoot) === expected, "Rotating auth_key/tpN must identify the same warm resource while preserving port and raw query");
  check(canonical(expected) === expected, "Warm target canonicalization must be idempotent");
  const differentRendition = oldRoot.replace("/main/", "/hevc/");
  const differentHost = oldRoot.replace("tp12.drleao.cn", "tp12.example.com");
  check(canonical(oldRoot) !== canonical(differentRendition), "Different renditions/paths must remain distinct");
  check(canonical(oldRoot) !== canonical(differentHost) && canonical(differentHost).includes("tp12.example.com"), "Other hosts must never join the drleao pool");
  check(canonical(oldRoot) !== canonical(oldRoot.replace(":8443", ":8444")), "Nondefault ports must remain distinct");
  check(canonical(oldRoot) !== canonical(oldRoot.replace("v=3", "v=4")), "Non-auth query parameters must remain distinct");
  check(canonical("https://tpa.drleao.cn/list?auth_key=x") === "https://tpa.drleao.cn/list", "Only tp followed by ASCII digits is a rotating pool member");
  check(canonical("https://tp1.drleao.cn.evil.invalid/list?auth_key=x") === "https://tp1.drleao.cn.evil.invalid/list", "Host suffix lookalikes must remain exact hosts");
  check(canonical("https://tp1.drleao.cn/list?%61uth_key=x&Auth_key=y&v=a%20b") === "https://tp.drleao.cn/list?%61uth_key=x&Auth_key=y&v=a%20b", "Rust's literal query-name and raw encoding semantics must be preserved");
  check(canonical("not a url") === "not a url", "Invalid inputs must remain unchanged");

  const internals = (window as unknown as { __TAURI_INTERNALS__: { invoke: (command: string, args?: Record<string, unknown>) => Promise<unknown> } }).__TAURI_INTERNALS__;
  const invoke = internals.invoke;
  const calls: { command: string; url?: string; active?: boolean }[] = [];
  let release!: () => void, acknowledged = false;
  const gate = new Promise<void>(resolve => { release = resolve; });
  internals.invoke = async (command, args) => {
    if (command === "media_session" || command === "media_warmup_cancel") {
      calls.push({ command, url: args?.url as string, active: args?.active as boolean | undefined });
      if (command === "media_session" && args?.active === true) await gate;
      return;
    }
    return invoke(command, args);
  };
  const activation = realMediaSession(newRoot, true).then(() => { acknowledged = true; });
  try {
    await realWarmupCancel(oldRoot);
    await drain();
    check(!acknowledged && calls.every(call => call.command !== "media_warmup_cancel"), "A canonical same-resource cleanup must preserve the warm target before the new session ACK");
    await realWarmupCancel(differentRendition);
    await realWarmupCancel(differentHost);
    const cancellations = calls.filter(call => call.command === "media_warmup_cancel").map(call => call.url);
    check(cancellations.join() === [differentRendition, differentHost].join(), "Other targets must still send their complete original signed URLs to the exact cancel RPC");
    const staleCleanup = realMediaSession(oldRoot, false);
    await drain();
    check(calls.filter(call => call.command === "media_session").length === 1, "Old signed cleanup must still wait behind the actual new-session ACK");
    release(); await Promise.all([activation, staleCleanup]);
    const sessions = calls.filter(call => call.command === "media_session");
    check(sessions[sessions.length - 1].url === newRoot && sessions[sessions.length - 1].active === true, "Session stale-cleanup reduction must stay exact-URL: canonical old false must not deactivate the newer signed true");
  } finally {
    release(); await activation.catch(() => {});
    try { await realMediaSession(newRoot, false); } finally { internals.invoke = invoke; }
  }
}

const drain = async () => { for (let i = 0; i < 20; i++) await Promise.resolve(); };
async function sessionQueueChecks() {
  const sent: string[] = [];
  let acknowledge!: () => void;
  const send = createMediaSessionQueue(async (url, active) => {
    sent.push(`${url}:${active}`);
    await new Promise<void>(resolve => { acknowledge = resolve; });
  });
  let completed = 0;
  const oldFalse = send("old", false).then(() => completed++);
  const newTrue = send("new", true).then(() => completed++);
  await drain();
  check(sent.join() === "new:true" && completed === 0, "False/new true must coalesce and both promises must wait for the real ACK");
  const leave = send("new", false).then(() => completed++);
  await drain();
  check(sent.length === 1, "Already sent RPC must retain its ordering barrier");
  acknowledge(); await Promise.all([oldFalse, newTrue]); await drain();
  check(sent.join() === "new:true,new:false" && completed === 2, "A real leave must send false at the same tick boundary, after the earlier ACK");
  acknowledge(); await leave;
  const strictFalse = send("same", false), strictTrue = send("same", true);
  await drain(); check(sent[sent.length - 1] === "same:true", "StrictMode cleanup/reactivation must collapse to active");
  acknowledge(); await Promise.all([strictFalse, strictTrue]);
  const reduced: string[] = [];
  const reduce = createMediaSessionQueue(async (url, active) => { reduced.push(`${url}:${active}`); });
  await reduce("A", true);
  await Promise.all([reduce("B", true), reduce("A", false)]);
  check(reduced.join() === "A:true,B:true", "A stale false(A) after new true(B) must retain active B");
  await Promise.all([reduce("B", true), reduce("B", false)]);
  check(reduced[reduced.length - 1] === "B:false", "Same-root true/false must actually deactivate");
  await reduce("A", true);
  await Promise.all([reduce("B", true), reduce("B", false)]);
  check(reduced[reduced.length - 1] === "A:false", "If new B is collapsed away, the previously acknowledged active A must be closed instead");
  const unknown: string[] = [];
  const unknownQueue = createMediaSessionQueue(async (url, active) => { unknown.push(`${url}:${active}`); });
  await Promise.all([unknownQueue("A", false), unknownQueue("B", false)]);
  check(unknown.join() === "A:false,B:false", "Unknown initial backend state must retain every potentially relevant false");
  await Promise.all([unknownQueue("B", true), unknownQueue("B", false)]);
  check(unknown.slice(-2).join() === "B:true,B:false", "Unknown true/false batch must establish and close a known root with both ACKs");
  const fail = createMediaSessionQueue(async () => { throw new Error("ACK rejected"); });
  const failures = await Promise.allSettled([fail("a", false), fail("b", true)]);
  check(failures.every(result => result.status === "rejected"), "A failed ACK must reject all coalesced callers, never publish a fake active session");
}

async function schedulerChecks() {
  const input = { id: 1, ep: 2, episodes: [1, 2, 3], videoIds: [9, 1, 8], index: 1, epMap: { 8: 4 }, swipeEp: false, direction: 1 };
  check(chooseWarmCandidate(input)?.key === "8:4", "swipeEp=false must prefer the next video over the next episode");
  check(chooseWarmCandidate({ ...input, swipeEp: true })?.key === "1:3", "Episode swipe mode must respect next-episode intent");
  check(chooseWarmCandidate({ ...input, direction: -1 })?.key === "9:1", "Reverse scroll must prefer the previous video");
  check(chooseWarmCandidate({ ...input, intentEp: 1 })?.key === "1:1", "Explicit episode focus must override scroll intent");
  const base: WarmSnapshot = { active: true, currentUrl: "current", candidate: { key: "2:1", id: 2, ep: 1 }, policy: "avc",
    time: 10, duration: 90, buffered: 18, rate: 1, waiting: false, seeking: false, foreground: false };
  check(mediaBudget(false, base), "Eight seconds must enter media budget");
  check(mediaBudget(true, { ...base, buffered: 14.1 }), "Media budget must persist below eight until four seconds");
  check(!mediaBudget(true, { ...base, buffered: 13.8 }), "Below four seconds must yield media bandwidth");
  check(!mediaBudget(false, { ...base, rate: 2 }), "2x playback must require sixteen media seconds to enter");
  check(!mediaBudget(true, { ...base, waiting: true }) && !mediaBudget(true, { ...base, seeking: true }), "Waiting and seek must gate media budget");
  const jobs = new Map<number, () => void>();
  let token = 0, resolves = 0, prepares = 0, warmCalls = 0, stateCalls = 0;
  let dependencyReady = false;
  let state: "idle" | "queued" | "ready" | "cancelled" | "failed" = "idle";
  let reason: string | undefined;
  const cancelled: string[] = [], events: string[] = [];
  let releaseResolve!: (url: string) => void;
  const scheduler = createWarmupScheduler({
    resolve: async candidate => { resolves++; return resolves === 1 ? new Promise<string>(resolve => { releaseResolve = resolve; }) : candidate.key; },
    prepare: async () => { prepares++; return true; },
    warm: async () => { warmCalls++; state = "queued"; return true; },
    status: async () => { stateCalls++; return { state, playlist_ready: true, segment_ready: true, dependencies_ready: dependencyReady, generation: 12, reason }; },
    cancel: async url => { cancelled.push(url); },
    trace: event => { events.push(event); },
    schedule: fn => { const id = ++token; jobs.set(id, fn); return id as unknown as ReturnType<typeof setTimeout>; },
    clear: id => { jobs.delete(id as unknown as number); },
  });
  const advance = async () => { const next = jobs.entries().next().value; if (next) { jobs.delete(next[0]); next[1](); } await drain(); };
  scheduler.update(base); await advance();
  scheduler.update({ ...base, buffered: 12, waiting: true });
  releaseResolve("target"); await drain();
  check(resolves === 1 && prepares === 1 && warmCalls === 0, "Light URL/playlist preparation must survive timeupdate/buffer/waiting changes without starting heavy media");
  scheduler.update(base); await advance();
  check(warmCalls === 1 && !events.includes("warm.ready"), "Accepted is queued, not ready");
  await advance(); check(stateCalls >= 1 && !events.includes("warm.ready"), "Queued status must stay pending");
  state = "ready"; await advance();
  check(!events.includes("warm.ready"), "Segment cache without dependencies must not report ready");
  dependencyReady = true; await advance();
  check(events.includes("warm.ready"), "All three resource readiness flags must be confirmed");
  state = "idle";
  scheduler.update({ ...base, currentUrl: "target", candidate: { key: "3:1", id: 3, ep: 1 } });
  await advance();
  check(!cancelled.includes("target"), "Promoting the old warm target to current playback must preserve its in-flight first segment");
  state = "cancelled"; await advance(); await advance();
  check(warmCalls === 3, "A preempted target must be requeued with a bounded retry");
  state = "failed"; reason = "unsupported_audio_rendition"; await advance();
  const before = warmCalls;
  scheduler.update({ ...base, currentUrl: "target", candidate: { key: "3:1", id: 3, ep: 1 } });
  await advance(); check(warmCalls === before, "Unsupported rendition must stop media retries for this candidate");
  state = "idle"; reason = undefined;
  scheduler.update({ ...base, candidate: { key: "4:1", id: 4, ep: 1 } }); await advance();
  scheduler.update({ ...base, active: false, candidate: { key: "4:1", id: 4, ep: 1 } }); await drain();
  check(cancelled.includes("4:1"), "Inactivation must cancel exactly its warm target");
  scheduler.stop();
  await busyBudgetChecks(base);
  await pendingPrepareCancelChecks(base);
}

async function pendingPrepareCancelChecks(base: WarmSnapshot) {
  const jobs = new Map<number, () => void>();
  let token = 0, release!: (value: boolean) => void;
  const cancellations: string[] = [];
  const scheduler = createWarmupScheduler({
    resolve: async candidate => candidate.key,
    prepare: () => new Promise<boolean>(resolve => { release = resolve; }),
    warm: async () => { throw new Error("Heavy stage must not start while preparation is pending"); },
    status: async () => { throw new Error("Status must not poll while preparation is pending"); },
    cancel: async url => { cancellations.push(url); },
    schedule: fn => { const id = ++token; jobs.set(id, fn); return id as unknown as ReturnType<typeof setTimeout>; },
    clear: id => { jobs.delete(id as unknown as number); },
  });
  scheduler.update({ ...base, buffered: 10, waiting: true });
  const first = jobs.entries().next().value!; jobs.delete(first[0]); first[1](); await drain();
  scheduler.update({ ...base, candidate: { key: "next:1", id: 8, ep: 1 } }); await drain();
  check(cancellations.includes("2:1"), "Replacing a candidate must cancel its pending light playlist preparation even before heavy admission");
  release(true); await drain();
  const second = jobs.entries().next().value!; jobs.delete(second[0]); second[1](); await drain();
  scheduler.stop(); await drain();
  check(cancellations.includes("next:1"), "Stop must precisely cancel a pending light preparation");
  release(true); await drain();
}

async function busyBudgetChecks(base: WarmSnapshot) {
  const jobs = new Map<number, () => void>();
  let token = 0, now = 1, statusCalls = 0, warms = 0, exhaustions = 0;
  let busy = true, accepted = false;
  const scheduler = createWarmupScheduler({
    resolve: async () => "busy-target", prepare: async () => true,
    warm: async () => { warms++; accepted = true; return true; }, cancel: async () => {},
    status: async () => {
      statusCalls++;
      return { state: busy ? "busy" : accepted ? "ready" : "idle", playlist_ready: true, segment_ready: accepted,
        dependencies_ready: accepted, generation: 1 };
    },
    now: () => now,
    trace: event => { if (event === "warm.exhausted") exhaustions++; },
    schedule: fn => { const id = ++token; jobs.set(id, fn); return id as unknown as ReturnType<typeof setTimeout>; },
    clear: id => { jobs.delete(id as unknown as number); },
  });
  const advance = async () => {
    const next = jobs.entries().next().value;
    if (next) { jobs.delete(next[0]); next[1](); }
    await drain(); now += 1500;
  };
  scheduler.update(base);
  for (let i = 0; i < 7; i++) await advance();
  check(statusCalls === 7 && warms === 0 && exhaustions === 0, "Busy beyond six seconds must remain a cheap read-only wait, not exhaust network failures");
  busy = false; await advance(); await advance();
  check(warms === 1 && exhaustions === 0, "A candidate admitted after a long foreground download must still reach ready");
  scheduler.update({ ...base, candidate: { key: "5:1", id: 5, ep: 1 } });
  busy = true; accepted = false; await advance();
  now += 120_001; await advance();
  const requests = statusCalls;
  for (let i = 0; i < 20; i++) { scheduler.update({ ...base, candidate: { key: "5:1", id: 5, ep: 1 }, time: 10 + i / 10 }); await advance(); }
  check(exhaustions === 1 && jobs.size === 0 && statusCalls === requests, "An exhausted candidate must stay terminal across timeupdates, with no 300ms re-entry loop");
  scheduler.stop();
}

function leaseTestClock() {
  let now = 0, sequence = 0;
  const jobs = new Map<number, { at: number; fn: () => void }>();
  return {
    now: () => now,
    jobs,
    schedule: (fn: () => void, ms: number) => { const id = ++sequence; jobs.set(id, { at: now + ms, fn }); return id as unknown as ReturnType<typeof setTimeout>; },
    clear: (id: ReturnType<typeof setTimeout>) => { jobs.delete(id as unknown as number); },
    async advance(ms: number) {
      const until = now + ms;
      let count = 0;
      while (true) {
        const next = [...jobs.entries()].filter(([, job]) => job.at <= until).sort((a, b) => a[1].at - b[1].at)[0];
        if (!next) break;
        if (++count > 500) throw new Error("Lease test timer loop exceeded bounded work");
        now = next[1].at; jobs.delete(next[0]); next[1].fn(); await drain();
      }
      now = until; await drain();
    },
  };
}
function leaseTestEngine(events: string[] = []) {
  return {
    loadingEnabled: true, bufferingEnabled: true, pauses: 0, resumes: 0,
    pauseBuffering() { this.pauses++; this.bufferingEnabled = false; events.push("pause"); },
    resumeBuffering() { this.resumes++; this.bufferingEnabled = true; events.push("resume"); },
  };
}
async function bufferingLeaseChecks() {
  const base: WarmSnapshot = { active: true, currentUrl: "current", candidate: { key: "next:1", id: 2, ep: 1 }, policy: "avc",
    time: 10, duration: 200, buffered: 30, rate: 1, waiting: false, seeking: false, foreground: false };
  check(bufferingLeaseMs(base) === 12_000 && bufferingLeaseMs({ ...base, buffered: 90 }) === 12_000, "20s wall-clock buffer may yield at most twelve seconds, even with a large media buffer");
  check(bufferingLeaseMs({ ...base, buffered: 29.9 }) === 0, "Less than twenty wall-clock seconds must not pause forward buffering");
  check(bufferingLeaseMs({ ...base, rate: 2 }) === 0 && bufferingLeaseMs({ ...base, rate: 2, buffered: 50 }) === 12_000, "Lease entry must scale media seconds with playback rate");
  check(bufferedWallSeconds({ ...base, buffered: 26, rate: 2 }) === 8, "Lease reserve must be measured in wall-clock seconds");
  check(bufferingLeaseMs(base, true) === 0 && bufferingLeaseMs({ ...base, waiting: true }) === 0 && bufferingLeaseMs({ ...base, seeking: true }) === 0 && bufferingLeaseMs({ ...base, active: false }) === 0 && bufferingLeaseMs({ ...base, foreground: true }) === 0, "Ready, inactive, waiting, seek and foreground business work must block a lease");
  const clock = leaseTestClock(), engine = leaseTestEngine();
  const originalPause = engine.pauseBuffering, originalResume = engine.resumeBuffering;
  const reasons: string[] = [];
  engine.bufferingEnabled = false;
  const prior = createBufferingLease(engine, { ...clock, maxMs: 12_000, isCurrent: () => true, onEnd: reason => reasons.push(reason) });
  check(!prior && engine.pauses === 0 && engine.resumes === 0 && engine.pauseBuffering === originalPause, "Pre-existing user/MMS pause must remain untouched with no wrappers installed");
  engine.bufferingEnabled = true;
  const owned = createBufferingLease(engine, { ...clock, maxMs: 99_000, isCurrent: () => true, onEnd: reason => reasons.push(reason) })!;
  check(owned.active() && !engine.bufferingEnabled && engine.pauses === 1, "Owned lease must pause buffering exactly once without stopping the engine");
  await clock.advance(11_999); check(engine.resumes === 0, "Lease must remain held before its deadline");
  await clock.advance(1);
  check(!owned.active() && engine.resumes === 1 && reasons.join() === "timeout" && engine.pauseBuffering === originalPause && engine.resumeBuffering === originalResume && clock.jobs.size === 0, "Twelve-second timeout must resume once and restore both exact instance methods");
  owned.release(); await clock.advance(30_000);
  check(engine.resumes === 1 && reasons.length === 1, "Expired/released leases must be idempotent and never resurrect their timers");
  const external = createBufferingLease(engine, { ...clock, maxMs: 12_000, isCurrent: () => true, onEnd: reason => reasons.push(reason) })!;
  engine.pauseBuffering(); external.release(); await clock.advance(12_000);
  check(!engine.bufferingEnabled && engine.resumes === 1 && reasons[reasons.length - 1] === "external_pause" && engine.pauseBuffering === originalPause && engine.resumeBuffering === originalResume, "A later external pause must revoke lease ownership; cleanup must not undo user intent");
  engine.bufferingEnabled = true;
  const resumed = createBufferingLease(engine, { ...clock, maxMs: 12_000, isCurrent: () => true, onEnd: reason => reasons.push(reason) })!;
  engine.resumeBuffering(); resumed.release();
  check(engine.resumes === 2 && engine.bufferingEnabled && !resumed.active() && clock.jobs.size === 0, "External resume/startLoad must end the lease without a second resume");
  class PrototypeEngine {
    loadingEnabled = true; bufferingEnabled = true;
    pauseBuffering() { this.bufferingEnabled = false; }
    resumeBuffering() { this.bufferingEnabled = true; }
  }
  const inherited = new PrototypeEngine(), untouched = new PrototypeEngine();
  const prototypePause = PrototypeEngine.prototype.pauseBuffering, prototypeResume = PrototypeEngine.prototype.resumeBuffering;
  const inheritedLease = createBufferingLease(inherited, { ...clock, maxMs: 12_000, isCurrent: () => true, onEnd: () => {} })!;
  check(Object.prototype.hasOwnProperty.call(inherited, "pauseBuffering") && untouched.pauseBuffering === prototypePause && PrototypeEngine.prototype.pauseBuffering === prototypePause, "Only the leased instance may receive wrappers; another engine and its prototype must remain unchanged");
  inheritedLease.release();
  check(!Object.prototype.hasOwnProperty.call(inherited, "pauseBuffering") && !Object.prototype.hasOwnProperty.call(inherited, "resumeBuffering") && PrototypeEngine.prototype.resumeBuffering === prototypeResume, "Release must remove temporary instance properties and restore inherited methods exactly");
  const old = leaseTestEngine(), next = leaseTestEngine();
  let current = old;
  const oldPause = old.pauseBuffering, oldResume = old.resumeBuffering;
  const stale = createBufferingLease(old, { ...clock, maxMs: 12_000, isCurrent: () => current === old, onEnd: () => {} })!;
  current = next; stale.release("source_change"); await clock.advance(12_000);
  check(old.resumes === 0 && next.pauses === 0 && next.resumes === 0 && old.pauseBuffering === oldPause && old.resumeBuffering === oldResume, "Source replacement must restore only the old instance's methods and never resume or pause the new engine");
  const destroyed = createBufferingLease(next, { ...clock, maxMs: 12_000, isCurrent: () => true, onEnd: () => {} })!;
  const nextPause = next.pauseBuffering;
  destroyed.release("destroy", false);
  check(next.resumes === 0 && next.pauseBuffering !== nextPause && clock.jobs.size === 0, "Destruction must remove wrappers and its timer without reviving buffering");

  const harness = () => {
    const clock = leaseTestClock(), events: string[] = [], engine = leaseTestEngine(events);
    let snapshot = base, ready = false, leases = 0, cancels = 0, exhausted = 0;
    const scheduler = createWarmupScheduler({
      ...clock,
      resolve: async () => "target.m3u8", prepare: async () => true, warm: async () => true,
      status: async () => ({ state: ready ? "ready" : "busy", playlist_ready: true, segment_ready: ready, dependencies_ready: ready, generation: 1 }),
      cancel: async url => { if (url === "target.m3u8") { cancels++; events.push("cancel"); } },
      acquireBufferingLease: request => {
        const lease = createBufferingLease(engine, { ...request, ...clock, isCurrent: () => true, canResume: () => snapshot.active });
        if (lease) leases++;
        return lease;
      },
      trace: event => { if (event === "warm.exhausted") exhausted++; },
    });
    return { clock, engine, events, scheduler, originalPause: engine.pauseBuffering, originalResume: engine.resumeBuffering, ready: () => { ready = true; }, leases: () => leases, cancels: () => cancels, exhausted: () => exhausted,
      update: (next: WarmSnapshot) => { snapshot = next; scheduler.update(next); } };
  };
  const done = harness(); done.update(base); await done.clock.advance(300);
  check(done.leases() === 1 && !done.engine.bufferingEnabled, "Unready candidate with twenty seconds ahead must acquire one bounded lease");
  done.ready(); await done.clock.advance(1500);
  check(done.engine.bufferingEnabled && done.engine.resumes === 1 && done.cancels() === 0, "True ready must immediately release the lease without cancelling its completed target");
  done.scheduler.stop();
  const low = harness(); low.update(base); await low.clock.advance(300);
  low.update({ ...base, buffered: 18 });
  check(low.engine.bufferingEnabled && low.cancels() === 1 && low.events.indexOf("cancel") < low.events.indexOf("resume"), "At eight seconds, exact target cancellation must precede resuming current forward fill");
  await low.clock.advance(29_000); low.update(base); await low.clock.advance(500);
  check(low.leases() === 1, "Low-buffer release must enforce its thirty-second cooldown");
  await low.clock.advance(1500);
  check(low.leases() === 2, "A healthy buffer may make only one more bounded attempt after cooldown");
  await low.clock.advance(12_000);
  check(low.engine.bufferingEnabled && low.cancels() === 2 && low.engine.resumes === 2, "Lease timeout must cancel unfinished media and resume exactly once");
  for (let i = 0; i < 10; i++) low.update(base);
  await low.clock.advance(80_000);
  check(low.leases() === 2, "A candidate must never acquire a third lease after timeout/cooldown");
  await low.clock.advance(30_000);
  const exhausted = low.exhausted();
  for (let i = 0; i < 20; i++) { low.update(base); await low.clock.advance(300); }
  check(exhausted === 1 && low.exhausted() === 1 && low.clock.jobs.size === 0, "Existing 120s total budget must remain terminal across updates, with no lease timer revival");
  low.scheduler.stop();
  for (const reason of ["waiting", "seeking", "inactive", "candidate"] as const) {
    const h = harness(); h.update(base); await h.clock.advance(300);
    h.update(reason === "waiting" ? { ...base, waiting: true } : reason === "seeking" ? { ...base, seeking: true } : reason === "inactive" ? { ...base, active: false } : { ...base, candidate: { key: "other:1", id: 3, ep: 1 } });
    check(h.engine.pauseBuffering === h.originalPause && h.engine.resumeBuffering === h.originalResume, `${reason} must restore both instance methods on release`);
    check(h.cancels() >= 1, `${reason} must precisely cancel an unfinished candidate`);
    h.scheduler.stop(); await h.clock.advance(12_000);
    check(h.clock.jobs.size === 0, `${reason} cleanup must leave no expiring lease timers`);
  }
}

async function playlistTTLRefreshChecks() {
  const base: WarmSnapshot = { active: true, currentUrl: "current", candidate: { key: "ttl:1", id: 2, ep: 1 }, policy: "avc",
    time: 10, duration: 200, buffered: 40, rate: 1, waiting: false, seeking: false, foreground: false };
  const clock = leaseTestClock();
  let resolves = 0, prepares = 0, warms = 0, leases = 0, cancels = 0, valid = false;
  let state: "idle" | "busy" | "queued" | "ready" = "busy";
  const generations: unknown[] = [];
  const scheduler = createWarmupScheduler({
    ...clock,
    resolve: async () => { resolves++; return "ttl-target.m3u8"; },
    prepare: async () => { prepares++; if (prepares > 1) valid = true; return true; },
    status: async () => ({ state, playlist_ready: valid, segment_ready: state === "ready", dependencies_ready: state === "ready", generation: 19 }),
    warm: async () => { warms++; state = "busy"; return false; },
    cancel: async () => { cancels++; },
    acquireBufferingLease: () => { leases++; return null; },
    trace: (event, data) => { if (event === "warm.prepared" || event === "warm.status") generations.push(data.generation); },
  });
  scheduler.update(base); await clock.advance(300);
  check(prepares === 2 && resolves === 1 && warms === 0 && leases === 0, "Expired raw playlist must refresh only the same resolved URL before any new lease/media admission");
  await clock.advance(1500);
  check(leases === 1 && warms === 0 && valid, "A refreshed valid busy intent may request a lease without counting a network failure");
  state = "idle"; await clock.advance(1500);
  check(warms === 1, "A valid idle plan must get a normal warm admission attempt so a busy response can register intent");
  state = "queued"; valid = false; await clock.advance(1500);
  check(prepares === 3 && warms === 1 && cancels === 0, "TTL refresh of a queued plan must preserve its real in-flight media and not enqueue a second heavy task");
  await clock.advance(1500);
  state = "ready"; await clock.advance(1500);
  check(resolves === 1 && warms === 1 && cancels === 0 && new Set(generations).size === 1, "Refresh must preserve URL resolution, target generation and queued handoff through ready");
  scheduler.stop();
  const failedClock = leaseTestClock();
  let failedPrepares = 0, failedResolves = 0, failedLeases = 0, exhausted = 0;
  const failed = createWarmupScheduler({
    ...failedClock,
    resolve: async () => { failedResolves++; return "ttl-failed.m3u8"; },
    prepare: async () => ++failedPrepares === 1,
    status: async () => ({ state: "busy", playlist_ready: false, segment_ready: false, dependencies_ready: false, generation: 1 }),
    warm: async () => { throw new Error("No media while raw playlist stays expired"); },
    cancel: async () => {},
    acquireBufferingLease: () => { failedLeases++; return null; },
    trace: event => { if (event === "warm.exhausted") exhausted++; },
  });
  failed.update(base); await failedClock.advance(6500);
  check(failedPrepares === 5 && failedResolves === 1 && failedLeases === 0 && exhausted === 1, "Four failed TTL refreshes must terminate without API replay or stale-plan leases");
  for (let i = 0; i < 20; i++) { failed.update(base); await failedClock.advance(300); }
  check(failedPrepares === 5 && exhausted === 1 && failedClock.jobs.size === 0, "Failed refresh exhaustion must remain terminal across timeupdates");
  failed.stop();
}

async function bufferedStallChecks() {
  const base: BufferedStallSnapshot = { active: true, wanted: true, rendered: true, paused: false, seeking: false, ended: false,
    time: 30, duration: 600, rate: 1, bufferEnd: 90, ranges: 1 };
  const harness = (initial: Partial<BufferedStallSnapshot> = {}) => {
    const clock = leaseTestClock(), snapshot = { ...base, ...initial }, targets: number[] = [], errors: string[] = [];
    const detector = createBufferedStallRecovery({ ...clock, sample: () => snapshot,
      nudge: target => { targets.push(target); snapshot.time = target; }, terminal: message => errors.push(message) });
    return { clock, snapshot, targets, errors, detector };
  };
  for (const gate of [{ active: false }, { wanted: false }, { rendered: false }, { paused: true }, { seeking: true }, { ended: true }, { bufferEnd: 30 }, { bufferEnd: 31.9 }, { ranges: 2 }, { rate: 0 }, { rate: 2, bufferEnd: 33.9 }]) {
    const h = harness(gate); h.detector.sync(); await h.clock.advance(10_000);
    check(h.targets.length === 0 && h.errors.length === 0 && h.clock.jobs.size === 0, "Inactive/paused/seeking/startup/network-starved/multi-range media must not be nudged or keep a polling timer");
    h.detector.destroy();
  }
  const normal = harness(); normal.detector.sync();
  for (let i = 0; i < 30; i++) { normal.snapshot.time += .5; normal.detector.sync(); await normal.clock.advance(500); }
  check(normal.targets.length === 0 && normal.errors.length === 0, "Normally progressing media must never trigger buffered-stall recovery");
  normal.detector.destroy(); check(normal.clock.jobs.size === 0, "Destroy must clear the old progress sampling timer");
  const stuck = harness(); stuck.detector.sync(); await stuck.clock.advance(2999);
  check(stuck.targets.length === 0, "Allow three seconds of browser/internal gap recovery before any nudge");
  await stuck.clock.advance(1);
  check(stuck.targets.length === 1 && Math.abs(stuck.targets[0] - 30.15) < .00001, "Single continuous buffered decode stall must nudge only 0.15 seconds");
  // 模拟自身微seek的seeking/seeked事件：仅清理timer，不把赋值误当真实推进。
  stuck.snapshot.seeking = true; stuck.detector.seeking();
  check(stuck.clock.jobs.size === 1, "Internal seeking must replace the old timer with one bounded completion monitor");
  stuck.snapshot.seeking = false; stuck.detector.seeked();
  await stuck.clock.advance(6000);
  check(stuck.targets.length === 2 && stuck.errors.length === 1 && stuck.clock.jobs.size === 0, "Two ineffective nudges must terminate once; internal microseek must not reset the episode budget");
  check(!isCodecFailure(stuck.errors[0]), "Buffered decode stall exhaustion must not poison HEVC codec capability");
  stuck.detector.sync(); await stuck.clock.advance(45_000);
  check(stuck.targets.length === 2 && stuck.errors.length === 1 && stuck.clock.jobs.size === 0, "Terminal recovery must never revive timers on future events");
  for (const gate of ["paused", "active", "wanted"] as const) {
    const h = harness(); h.detector.sync(); await h.clock.advance(1000);
    h.snapshot[gate] = gate === "paused"; h.detector.sync(); await h.clock.advance(5000);
    check(h.targets.length === 0 && h.clock.jobs.size === 0, "Manual pause/activity/wanted changes must cancel pending recovery before mutation");
    h.detector.destroy();
  }
  const userSeek = harness(); userSeek.detector.sync(); await userSeek.clock.advance(2000);
  userSeek.snapshot.time = 100; userSeek.snapshot.bufferEnd = 100; userSeek.snapshot.seeking = true; userSeek.detector.seeking();
  await userSeek.clock.advance(5000);
  userSeek.snapshot.seeking = false; userSeek.detector.seeked();
  check(userSeek.targets.length === 0 && userSeek.clock.jobs.size === 0, "User seek into an unbuffered target must not inherit the previous decode-stall timer");
  userSeek.detector.destroy();
  const neverSeeked = harness(); neverSeeked.detector.sync(); await neverSeeked.clock.advance(3000);
  neverSeeked.snapshot.seeking = true; neverSeeked.detector.seeking();
  for (let i = 0; i < 10; i++) neverSeeked.detector.sync();
  check(neverSeeked.clock.jobs.size === 1, "Repeated own-seeking events must retain exactly one bounded timer");
  await neverSeeked.clock.advance(3000); neverSeeked.detector.seeking();
  check(neverSeeked.clock.jobs.size === 1, "A delayed own seeking event must not disarm its existing completion monitor");
  await neverSeeked.clock.advance(4999);
  check(neverSeeked.errors.length === 0, "Allow the internal microseek its bounded eight-second completion budget");
  await neverSeeked.clock.advance(1);
  check(neverSeeked.targets.length === 1 && neverSeeked.errors.length === 1 && neverSeeked.clock.jobs.size === 0, "An internal seek that never emits seeked must terminate at eight seconds without a second nudge or orphaned timer");
  neverSeeked.detector.seeked(); neverSeeked.detector.sync(); await neverSeeked.clock.advance(45_000);
  check(neverSeeked.errors.length === 1 && neverSeeked.clock.jobs.size === 0, "Late seeked after terminal must not revive recovery");
  for (const gate of ["active", "wanted", "paused"] as const) {
    const h = harness(); h.detector.sync(); await h.clock.advance(3000);
    h.snapshot.seeking = true; h.detector.seeking();
    h.snapshot[gate] = gate === "paused"; h.detector.sync();
    await h.clock.advance(45_000);
    check(h.errors.length === 0 && h.clock.jobs.size === 0, "Inactivity, unwanted playback or manual pause must cancel the internal-seek monitor before terminal");
    h.detector.destroy();
  }
  const retired = harness(); retired.detector.sync(); await retired.clock.advance(3000);
  retired.snapshot.seeking = true; retired.detector.seeking(); retired.detector.destroy();
  retired.detector.seeking(); retired.detector.seeked(); await retired.clock.advance(45_000);
  check(retired.errors.length === 0 && retired.clock.jobs.size === 0, "Retired source and its late seek events must have no recovery callback");
  const override = harness(); override.detector.sync(); await override.clock.advance(3000);
  override.snapshot.seeking = true; override.detector.seeking();
  override.snapshot.time = 100; override.snapshot.bufferEnd = 100; override.detector.seeking();
  await override.clock.advance(45_000);
  check(override.targets.length === 1 && override.errors.length === 0 && override.clock.jobs.size === 0, "A real user seek replacing an internal microseek must clear its monitor and remain untouched");
  override.detector.destroy();
  const windowed = harness(); windowed.detector.sync(); await windowed.clock.advance(3000);
  windowed.snapshot.time += 1; windowed.detector.sync(); await windowed.clock.advance(3000);
  windowed.snapshot.time += 1; windowed.detector.sync(); await windowed.clock.advance(3000);
  check(windowed.targets.length === 2 && windowed.errors.length === 1, "Separate stall episodes still share at most two nudges in a forty-five-second window");
  const renewed = harness(); renewed.detector.sync(); await renewed.clock.advance(3000);
  renewed.snapshot.time += 1; renewed.detector.sync(); await renewed.clock.advance(3000);
  renewed.snapshot.time += 1; renewed.detector.sync();
  for (let i = 0; i < 50; i++) { renewed.snapshot.time += .1; renewed.detector.sync(); await renewed.clock.advance(1000); }
  await renewed.clock.advance(3000);
  check(renewed.targets.length === 3 && renewed.errors.length === 0, "Only real sustained progress plus a new forty-five-second window may admit a later independent stall");
  renewed.detector.destroy();
  const tail = harness({ duration: 30.2 }); tail.detector.sync(); await tail.clock.advance(3000);
  check(tail.targets.length === 1 && tail.targets[0] <= 30.05 + .00001, "Nudge must stay within both the actual buffered range and the media end");
  tail.detector.destroy();
}

function recoveryQualityChecks() {
  let now = 100;
  const budget = createRecoveryBudget(3, 45_000, () => now);
  check(budget.next() === 250 && budget.next() === 500 && budget.next() === 750 && budget.next() === null, "Recovery must terminate after exactly three attempts");
  const timed = createRecoveryBudget(3, 1000, () => now);
  check(timed.next() === 250, "First fatal error has one recovery attempt");
  now += 1001;
  check(timed.next() === null, "Wall-clock recovery budget must terminate before attempt count if expired");
  const handlers = new Map<string, (...args: any[]) => void>();
  let starts = 0, recoveries = 0, terminals = 0, destroyed = 0;
  const fake = { on: (event: string, handler: (...args: any[]) => void) => handlers.set(event, handler), startLoad: () => starts++, recoverMediaError: () => recoveries++, stopLoad: () => {}, destroy: () => destroyed++ } as unknown as Hls;
  const dispose = installHlsRecovery(fake, { active: () => true, time: () => 5, terminal: () => terminals++, trace: () => {} });
  const emit = (data: unknown) => handlers.get(Hls.Events.ERROR)?.(Hls.Events.ERROR, data);
  emit({ fatal: false, type: Hls.ErrorTypes.NETWORK_ERROR, details: Hls.ErrorDetails.FRAG_LOAD_ERROR });
  emit({ fatal: false, type: Hls.ErrorTypes.MEDIA_ERROR, details: Hls.ErrorDetails.BUFFER_STALLED_ERROR });
  check(starts === 0 && recoveries === 0, "Nonfatal errors must use hls.js internal retries, never blindly startLoad");
  emit({ fatal: true, type: Hls.ErrorTypes.MEDIA_ERROR, details: Hls.ErrorDetails.BUFFER_ADD_CODEC_ERROR });
  emit({ fatal: true, type: Hls.ErrorTypes.MEDIA_ERROR, details: Hls.ErrorDetails.BUFFER_ADD_CODEC_ERROR });
  check(terminals === 1 && destroyed === 1 && recoveries === 0, "Unsupported codec must terminate once without media recovery storms");
  dispose();
  check(qualityLabel({ width: 720, height: 1280, codec: "avc1.64001f" }) === "720p", "Portrait video quality must use the short edge");
  check(qualityLabel({ width: 1920, height: 1080, codec: "" }) === "1080p", "Landscape quality must report actual resolution");
  check(qualityLabel({ width: 0, height: 0, codec: "" }) === "标准/自动", "Unknown video size must not be labelled SD");
  check(codecLabel("avc1.64001f") === "AVC" && codecLabel("hvc1.1.6.L93.B0") === "HEVC", "Codec labels must be independent from pixel resolution");
  check(!isCodecFailure("networkError/fragLoadError") && isCodecFailure("mediaError/bufferAddCodecError"), "Network errors must not poison the HEVC decode capability cache");
}
