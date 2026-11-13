import React, { useState } from "react";
import { createRoot } from "react-dom/client";
import Feed from "../src/components/Feed";
import { AppProvider, useStore } from "../src/store";
import { optimizationUnitChecks } from "./playback-optimization";
import { api, type MediaWarmupStatus } from "../src/api";
import "../src/index.css";
import { flushPlaybackLog, initPlaybackLog, playbackLogApi, setPlaybackLog } from "../src/playbackDiagnostics";
import Settings from "../src/pages/Settings";
import Player from "../src/components/Player";
import Hls from "hls.js";

// 仅将自定义媒体协议映射到本地测试服务器；播放器、HLS 和鼠标事件均使用真实实现。
const proxy = new URLSearchParams(location.search).get("proxy") || "http://127.0.0.1:17779";
const open = XMLHttpRequest.prototype.open;
XMLHttpRequest.prototype.open = function (this: XMLHttpRequest, method: string, target: string | URL, async: boolean = true, user?: string | null, credential?: string | null) {
  const url = new URL(String(target), location.href);
  if (url.hostname === "hg.localhost" || (url.hostname === "localhost" && url.protocol === "hg:")) {
    target = `${proxy}${url.pathname}${url.search}`;
  }
  return open.call(this, method, target, async, user, credential);
};

const fixture = {
  seeks: [] as number[], errors: [] as string[],
  setActive: (_active: boolean) => {},
  setSource: (_id: number) => {},
  expirePlayUrls: () => { const now = Date.now; Date.now = () => now() + 60_001; },
  setPreferHevc: (_on: boolean) => {},
  preferHevc: false,
  codecRecoveries: 0,
  failCodec: () => testHls?.trigger(Hls.Events.ERROR, { type: Hls.ErrorTypes.MEDIA_ERROR, details: Hls.ErrorDetails.BUFFER_ADD_CODEC_ERROR, fatal: true, error: new Error("fixture codec unsupported") }),
  commands: [] as { command: string; url?: string; active?: boolean; id?: number; ep?: number }[],
  warmupAccepted: true,
  warmupState: "ready" as MediaWarmupStatus["state"],
  warmupDependencies: true,
  prepareAccepted: true,
  failNetwork: (fatal = false) => testHls?.trigger(Hls.Events.ERROR, { type: Hls.ErrorTypes.NETWORK_ERROR, details: Hls.ErrorDetails.FRAG_LOAD_ERROR, fatal, error: new Error("fixture network") }),
  failMedia: () => testHls?.trigger(Hls.Events.ERROR, { type: Hls.ErrorTypes.MEDIA_ERROR, details: Hls.ErrorDetails.BUFFER_STALLED_ERROR, fatal: true, error: new Error("fixture media") }),
  sessionDelay: 0,
  starts: [] as number[],
  renderedSources: [] as string[],
  unitChecks: optimizationUnitChecks,
  setBuffer: (end: number) => {
    const v = document.querySelector("video");
    if (!v) throw new Error("Fixture video missing");
    Object.defineProperty(v, "buffered", { configurable: true, value: { length: end > 0 ? 1 : 0, start: () => 0, end: () => end } });
    v.dispatchEvent(new Event("progress"));
    v.dispatchEvent(new Event("timeupdate"));
  },
  clearBufferOverride: () => {
    const v = document.querySelector("video");
    if (v) { Reflect.deleteProperty(v, "buffered"); v.dispatchEvent(new Event("progress")); }
  },
  logs: [] as { event: string; data: Record<string, unknown> }[],
  setLogging: setPlaybackLog,
  flushLogs: flushPlaybackLog,
  rejectLoggingSave: false,
  pauseBuffering: () => testHls?.pauseBuffering(),
  resumeBuffering: () => testHls?.resumeBuffering(),
  engineState: () => ({ loading: testHls?.loadingEnabled, buffering: testHls?.bufferingEnabled, levels: testHls?.levels.length }),
};
let testHls: Hls | undefined;
const diagnosticTest = new URLSearchParams(location.search).has("diagnostics");
const legacyTest = new URLSearchParams(location.search).has("legacy");
const initiallyInactive = new URLSearchParams(location.search).has("inactive");
const optimizationTest = new URLSearchParams(location.search).has("optimization");
const mobileTest = new URLSearchParams(location.search).has("mobile");
if (optimizationTest) {
  // HEVC 测试流实际承载 AVC：明确模拟能力，不依赖运行机器的硬件解码支持。
  const supported = new URLSearchParams(location.search).get("hevc") !== "unsupported";
  const original = MediaSource.isTypeSupported.bind(MediaSource);
  MediaSource.isTypeSupported = type => /hvc1|hev1/i.test(type) ? supported : original(type);
}
if (diagnosticTest) {
  // Test-only handle: exercise the real engine's public API without rewriting production modules.
  const attach = Hls.prototype.attachMedia;
  Hls.prototype.attachMedia = function (...args) { testHls = this; return attach.apply(this, args); };
  const recover = Hls.prototype.recoverMediaError;
  Hls.prototype.recoverMediaError = function (...args) { fixture.codecRecoveries += 1; return recover.apply(this, args); };
  const startLoad = Hls.prototype.startLoad;
  Hls.prototype.startLoad = function (...args) { fixture.starts.push(args[0] ?? -1); return startLoad.apply(this, args); };
  let enabled = false;
  playbackLogApi.status = async () => ({ enabled, path: "playback.log", error: null });
  playbackLogApi.set = async (on) => {
    if (fixture.rejectLoggingSave) throw new Error("fixture: disk unavailable");
    return { enabled: enabled = on, path: "playback.log", error: null };
  };
  playbackLogApi.write = async entries => {
    if (!enabled) return;
    fixture.logs.push(...entries);
    const collect = (window as typeof window & { collectPlaybackLogs?: (entries: typeof fixture.logs) => Promise<void> }).collectPlaybackLogs;
    await collect?.(entries);
  };
  playbackLogApi.read = async () => fixture.logs.map(e => JSON.stringify(e)).join("\n");
  api.lineState = async () => ({ base: "https://fixture.invalid", default: "https://fixture.invalid", fixed: [] });
  void initPlaybackLog();
}
Object.assign(window, { playerSeekFixture: fixture });
document.addEventListener("seeking", (e) => {
  if (e.target instanceof HTMLVideoElement) fixture.seeks.push(e.target.currentTime);
}, true);
document.addEventListener("playing", (e) => {
  if (!(e.target instanceof HTMLVideoElement)) return;
  const engine = testHls, source = engine?.url || "";
  e.target.requestVideoFrameCallback(() => {
    if (testHls === engine && source) fixture.renderedSources.push(new URL(source).searchParams.get("u") || source);
  });
}, true);
window.addEventListener("error", (e) => fixture.errors.push(e.message));
api.boot = async () => ({ user: null, logged_in: false, base: "https://fixture.invalid", strict_security: true });
api.play = async (id, ep) => {
  fixture.commands.push({ command: "play", id, ep });
  return { id, ep_num: ep, title: "Seek fixture", duration: 90,
    video_url: optimizationTest ? `https://media.fixture.invalid/list-${id}-${ep}.m3u8` : "https://media.fixture.invalid/list.m3u8",
    video_url_hevc: optimizationTest ? `https://media.fixture.invalid/hevc-${id}-${ep}.m3u8` : undefined };
};
api.mediaPrefetch = async url => { fixture.commands.push({ command: "prefetch", url }); return true; };
api.mediaPrefetchCancel = async url => { fixture.commands.push({ command: "cancel", url }); };
api.mediaSession = async (url, active) => {
  fixture.commands.push({ command: "session", url, active });
  if (fixture.sessionDelay) await new Promise(resolve => setTimeout(resolve, fixture.sessionDelay));
  fixture.commands.push({ command: "session_ready", url, active });
};
const fixtureWarmTargets = new Set<string>();
api.mediaPrepare = async url => { fixture.commands.push({ command: "prepare", url }); return fixture.prepareAccepted; };
api.mediaWarmupCancel = async url => { fixture.commands.push({ command: "warmup_cancel", url }); fixtureWarmTargets.delete(url); };
api.mediaWarmupStatus = async url => {
  fixture.commands.push({ command: "warmup_status", url });
  const state = fixtureWarmTargets.has(url) || fixture.warmupState === "busy" ? fixture.warmupState : "idle";
  return { state, playlist_ready: true, segment_ready: state === "ready", dependencies_ready: fixture.warmupDependencies, generation: 1 };
};
api.related = async id => { fixture.commands.push({ command: "related", id }); return { items: [] }; };
api.mediaWarmup = async url => { fixture.commands.push({ command: "warmup", url }); if (fixture.warmupAccepted) fixtureWarmTargets.add(url); return fixture.warmupAccepted; };
api.video = async () => ({
  id: 991, title: "Seek fixture", description: "", cover: "", tags: [], score: 0,
  episode_count: 1, total_episodes: 1,
  episodes: optimizationTest ? [1, 2].map(ep_num => ({ ep_num, title: String(ep_num), duration: 90, status: 1 })) : [{ ep_num: 1, title: "one", duration: 90, status: 1 }],
  counts: { like: 0, collect: 0, comment: 0, hot: 0 },
  author: { id: 1, name: "Fixture" }, is_finished: true, is_original: 0,
});
function FixturePreferences() {
  const store = useStore();
  fixture.setPreferHevc = store.setPreferHevc;
  fixture.preferHevc = store.preferHevc;
  return null;
}
function Fixture() {
  const [source, setSource] = useState(991);
  fixture.setSource = setSource;
  const [active, setActive] = useState(!initiallyInactive);
  const [settings, setSettings] = useState(false);
  fixture.setActive = setActive;
  return <AppProvider>
    <FixturePreferences />
    <div style={{ position: "relative", width: 900, height: 540 }}>
      {legacyTest
        ? <Player src={optimizationTest ? `https://media.fixture.invalid/list-${source}-1.m3u8` : "https://media.fixture.invalid/list.m3u8"} autoPlay active={active} className="w-full h-full" diagnosticContext={{ video_id: source, active }} />
        : <Feed videoIds={optimizationTest ? [source, source + 1] : [991]} mode="home" active={active} bottomInset={0} />}
    </div>
    <button onClick={() => setActive((v) => !v)}>切换播放页激活状态</button>
    {diagnosticTest && <button onClick={() => setSettings(v => !v)}>诊断设置</button>}
    {settings && <div style={{ position: "relative", width: 900, height: 700 }}><Settings onBack={() => setSettings(false)} wide /></div>}
  </AppProvider>;
}
createRoot(document.querySelector("#root")!).render(<Fixture />);

// 自动检查真正的浏览器命中测试，而非仅匹配 CSS 字符串。
async function checkIdleSeekTarget() {
  const deadline = performance.now() + 20000;
  while (performance.now() < deadline) {
    const video = document.querySelector("video");
    const input = document.querySelector<HTMLInputElement>('input[aria-label="播放进度"]');
    if (video && input && video.currentTime > 0 && document.querySelector(".player-root.is-idle")) {
      const rect = input.getBoundingClientRect();
      if (document.elementFromPoint(rect.x + rect.width * .8, rect.y + rect.height / 2) !== input) {
        throw new Error("闲置进度条点击穿透到视频");
      }
      document.querySelector("#results")!.textContent = JSON.stringify({ passed: 1, failed: 0 });
      return;
    }
    await new Promise<void>((resolve) => requestAnimationFrame(() => resolve()));
  }
  throw new Error("等待播放或控制条隐藏超时");
}
if (!legacyTest && !initiallyInactive && !mobileTest) void checkIdleSeekTarget().catch((error) => {
  fixture.errors.push(String(error));
  document.querySelector("#results")!.textContent = JSON.stringify({ passed: 0, failed: 1, error: String(error) });
});
