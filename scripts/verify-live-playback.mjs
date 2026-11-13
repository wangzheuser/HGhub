// Real WebView2 verification: never mocks APIs, media or IPC; CDP is loopback-only.
import { chromium } from 'playwright-core';
import { mkdir, writeFile } from 'node:fs/promises';
import { resolve, dirname } from 'node:path';
import { parseArgs } from 'node:util';
const { values: args } = parseArgs({ options: { cdp: { type: 'string', default: 'http://127.0.0.1:9227' }, inspect: { type: 'boolean' }, reload: { type: 'boolean' }, 'warm-checks': { type: 'boolean' }, 'warm-only': { type: 'boolean' }, 'paused-ready': { type: 'boolean' }, 'episode-only': { type: 'boolean' }, output: { type: 'string' } } });
const endpoint = new URL(args.cdp);
if (!['127.0.0.1', 'localhost', '[::1]'].includes(endpoint.hostname)) throw new Error('CDP must be loopback');
const output = resolve(args.output || `test-results/live-playback/${new Date().toISOString().replace(/[:.]/g, '-')}`);
await mkdir(dirname(output), { recursive: true });
await mkdir(output, { recursive: false });
const report = { started: new Date().toISOString(), transport: 'real Tauri WebView2 + Rust + upstream', mocked: false, startup_kind: args.reload ? 'UI reload with Rust/disk caches retained; classify using cache/log evidence' : 'Existing window; no navigation-startup claim', cases: [] };
const sanitize = value => String(value).replace(/https?:\/\/[^\s"<>]+/g, '[resource-url]').slice(0, 600);
let page, loggingBefore, api, swipeBefore, mediaBefore;
try {
  const browser = await chromium.connectOverCDP(args.cdp);
  page = browser.contexts().flatMap(c => c.pages()).find(p => /localhost:1420|127\.0\.0\.1:1420/.test(p.url()));
  if (!page) throw new Error('HGhub WebView target missing');
  await page.bringToFront();
  if (args.reload) {
    await page.addInitScript(() => {
      window.__liveStartupFrames = [];
      const seen = new WeakSet();
      const scan = () => document.querySelectorAll('video').forEach(v => {
        if (seen.has(v)) return;
        seen.add(v);
        v.requestVideoFrameCallback((_now, meta) => window.__liveStartupFrames.push({ navigation_to_frame_ms: performance.now(), media_time: meta.mediaTime, width: v.videoWidth, height: v.videoHeight }));
      });
      new MutationObserver(scan).observe(document, { childList: true, subtree: true });
      document.addEventListener('DOMContentLoaded', scan);
    });
    await page.reload({ waitUntil: 'domcontentloaded' });
  }
  report.browser = browser.version();
  report.is_tauri = await page.evaluate(() => '__TAURI_INTERNALS__' in window);
  if (!report.is_tauri) throw new Error('Expected the real Tauri window, not a browser fixture');
  await page.evaluate(async () => { window.__liveApi = (await import('/src/api.ts')); window.__liveVideo = () => Array.from(document.querySelectorAll('.player-root')).at(-1)?.querySelector('video'); });
  api = (cmd, params = {}) => page.evaluate(async ({ cmd, params }) => (await import('/src/api.ts')).rawCall(cmd, params), { cmd, params });
  report.initial_cache = await api('cache_stats');
  const line = await api('line_state');
  report.line_host = new URL(line.base).hostname;
  if (args.inspect) {
    report.buttons = await page.getByRole('button').evaluateAll(nodes => [...new Set(nodes.map(n => n.getAttribute('aria-label') || n.innerText).filter(Boolean))].slice(0, 45));
    report.page_text = sanitize(await page.locator('body').innerText());
    report.media = await page.locator('video').last().evaluateAll(nodes => nodes.map(v => ({ time: v.currentTime, duration: Number.isFinite(v.duration) ? v.duration : null, ready: v.readyState, paused: v.paused, error: v.error?.message })));
    report.diagnostics = await page.evaluate(() => ({ hls_error: window.__hlsErr, prefer_hevc: JSON.parse(localStorage.getItem('hghub_app_v1') || '{}').preferHevc }));
  } else {
    loggingBefore = await api('playback_log_status');
    await page.evaluate(async () => { const d = await import('/src/playbackDiagnostics.ts'); await d.initPlaybackLog(); await d.setPlaybackLog(true); });
    await page.waitForFunction(allowPaused => { const v = window.__liveVideo(); return v && v.readyState >= 2 && (v.currentTime > 0 || (allowPaused && v.paused)) && Number.isFinite(v.duration) && v.duration > 0; }, !!args['paused-ready'], { timeout: 90000 });
    report.startup_frames = await page.evaluate(() => window.__liveStartupFrames || []);
    const video = page.locator('.player-root').last().locator('video');
    swipeBefore = await page.getByRole('button', { name: '上下划切集', exact: true }).last().getAttribute('aria-pressed');
    mediaBefore = await video.evaluate(v => ({ muted: v.muted, volume: v.volume, rate: v.playbackRate }));
    report.preferences_before = { swipe_episode: swipeBefore, ...mediaBefore };
    await video.evaluate(async v => { v.muted = true; await v.play(); });
    await page.waitForFunction(()=>{const v=window.__liveVideo();return v&&!v.paused&&v.currentTime>0&&v.getVideoPlaybackQuality().totalVideoFrames>0},null,{timeout:15000});
    const state = () => page.evaluate(() => { const v = window.__liveVideo(); return v ? { time: v.currentTime, duration: v.duration, ready: v.readyState, paused: v.paused, seeking: v.seeking, frames: v.getVideoPlaybackQuality?.().totalVideoFrames, dropped: v.getVideoPlaybackQuality?.().droppedVideoFrames, buffered: Array.from({ length: v.buffered.length }, (_, i) => [v.buffered.start(i), v.buffered.end(i)]) } : null; });
    async function scenario(name, action, timeout = 45000) {
      const entry = { name, before: await state() }; report.cases.push(entry);
      try {
        await page.evaluate(allowSameSource => {
          window.__liveCleanup?.();
          window.__liveStart = performance.now(); window.__liveFrame = null;
          const oldSource = window.__liveVideo()?.currentSrc;
          const seen = new WeakSet(), cleanups = [];
          const arm = v => {
            if (seen.has(v)) return;
            seen.add(v);
            const frames = new Set();
            const frame = () => {
              const id = v.requestVideoFrameCallback(() => {
                frames.delete(id);
                if (!v.paused && v.readyState >= 2 && !window.__liveFrame) window.__liveFrame = { elapsed_ms: performance.now() - window.__liveStart, time: v.currentTime, width: v.videoWidth, height: v.videoHeight };
              });
              frames.add(id);
            };
            const changed = () => { if (v.currentSrc && v.currentSrc !== oldSource) frame(); };
            const played = () => { if (allowSameSource) frame(); else changed(); };
            v.addEventListener('seeked', frame); v.addEventListener('playing', played); v.addEventListener('loadeddata', changed);
            cleanups.push(() => { v.removeEventListener('seeked', frame); v.removeEventListener('playing', played); v.removeEventListener('loadeddata', changed); for (const id of frames) v.cancelVideoFrameCallback(id); });
            changed();
          };
          const scan = () => document.querySelectorAll('video').forEach(arm);
          const observer = new MutationObserver(scan); observer.observe(document, { childList: true, subtree: true }); scan();
          window.__liveCleanup = () => { observer.disconnect(); cleanups.forEach(fn => fn()); };
        }, name === 'suspend_resume');
        const target = await action();
        await page.waitForFunction(({ target, switchSource }) => {
          const v = window.__liveVideo();
          return v && v.readyState >= 2 && !v.seeking && !v.paused && (switchSource ? window.__liveFrame && v.currentTime > .15 : window.__liveFrame && Math.abs(v.currentTime - target) < 2);
        }, { target: target ?? 0, switchSource: target == null }, { timeout });
        entry.after = await state();
        entry.elapsed_ms = await page.evaluate(() => window.__liveFrame?.elapsed_ms ?? performance.now() - window.__liveStart);
        entry.status = 'passed';
      } catch (error) { entry.status = 'failed'; entry.error = sanitize(error.message); }
      console.log(JSON.stringify(entry));
      await writeFile(`${output}/report.json`, JSON.stringify(report, null, 2));
    }
    async function seek(fraction, drag = false) {
      const slider = page.getByRole('slider', { name: '播放进度' }).first();
      await slider.scrollIntoViewIfNeeded();
      const box = await slider.boundingBox();
      const duration = (await state()).duration;
      if (!box) throw new Error('Seek bar missing');
      const x = box.x + box.width * fraction, y = box.y + box.height / 2;
      if (drag) {
        await page.mouse.move(box.x + box.width * .2, y); await page.mouse.down();
        await page.mouse.move(x, y, { steps: 8 });
        await page.waitForTimeout(180); await page.mouse.up();
      } else await page.mouse.click(x, y);
      return duration * fraction;
    }
    if (!args['warm-only']) {
    await scenario('seek_nearby', async () => {
      const v = await state();
      const range = v.buffered.find(([a, b]) => a <= v.time && b > v.time);
      const target = range ? Math.max(range[0] + .3, Math.min(v.time + 3, range[1] - .3)) : v.time + 3;
      return seek(target / v.duration);
    });
    await scenario('drag_distant', () => seek(.55, true));
    await scenario('seek_back', () => seek(.2));
    await scenario('seek_cached_distant', () => seek(.55));
    // Preparing next episode requires current forward buffer to become healthy.
    await page.waitForFunction(() => { const v = window.__liveVideo(); if (!v) return false; return Array.from({length:v.buffered.length},(_,i)=>[v.buffered.start(i),v.buffered.end(i)]).some(([a,b])=>a<=v.currentTime && b-v.currentTime>=8); }, null, { timeout: 45000 }).catch(() => {});
    await page.waitForTimeout(5000);
    await scenario('next_episode', async () => {
      const next = page.getByRole('button', { name: '下一集 E', exact: true });
      if (await next.count() === 0) throw new Error('Current video has no next-episode control');
      const old = await video.evaluate(v => v.currentSrc);
      // 用户先移动鼠标唤醒闲置控件，再点击；不要在不可命中的隐藏按钮上空等。
      const root = await page.locator('.player-root').last().boundingBox();
      if (root) { await page.mouse.move(root.x + root.width * .4, root.y + root.height * .4); await page.mouse.move(root.x + root.width * .45, root.y + root.height * .45); }
      await page.waitForFunction(() => !window.__liveVideo()?.closest('.player-root')?.classList.contains('is-idle'));
      await next.click();
      await page.waitForFunction(old => window.__liveVideo()?.currentSrc !== old, old, { timeout: 10000 });
      return null;
    });
    await scenario('seek_unbuffered_after_episode', () => seek(.82));
    await scenario('next_video', async () => {
      const box = page.locator('.player-root').last().locator('.overflow-y-scroll.snap-y');
      const old = await page.locator('video').last().evaluate(v => v.currentSrc);
      await box.evaluate(el => { el.scrollTop += el.clientHeight; });
      await page.waitForFunction(old => { const v = window.__liveVideo(); return v && v.currentSrc && v.currentSrc !== old; }, old, { timeout: 15000 });
      return null;
    });
    await scenario('suspend_resume', async () => {
      await page.getByRole('button', { name: '设置', exact: true }).click();
      await page.waitForFunction(() => window.__liveVideo()?.paused === true);
      await page.waitForTimeout(400);
      const idle = await api('cache_stats');
      report.suspended_cache = idle;
      if (idle.foreground_downloads !== 0 || idle.background_downloads !== 0 || idle.session_active) throw new Error('Inactive page retained media download ownership');
      await page.getByRole('button', { name: '首页', exact: true }).click();
      return null;
    });
    }
    if (args['warm-checks'] || args['warm-only']) {
      const mode = page.getByRole('button', { name: '上下划切集', exact: true }).last();
      if (swipeBefore === undefined) swipeBefore = await mode.getAttribute('aria-pressed');
      const wake = async () => {
        const r = await page.locator('.player-root').last().boundingBox();
        if (r) { await page.mouse.move(r.x + r.width * .4, r.y + r.height * .4); await page.mouse.move(r.x + r.width * .45, r.y + r.height * .45); }
      };
      const setMode = async enabled => { await wake(); if ((await mode.getAttribute('aria-pressed')) !== String(enabled)) await mode.click(); };
      async function waitWarm(acceptQueued = false, forEpisode = false) {
        const started = performance.now(); let last, candidateKey = '', target;
        while (performance.now() - started < 120000) {
          const candidate = await page.locator('.player-root').last().evaluate(el => ({ id: Number(el.dataset.warmId), ep: Number(el.dataset.warmEpisode), current_id: Number(el.dataset.videoId), current_ep: Number(el.dataset.episode) }));
          const intended = forEpisode ? candidate.id === candidate.current_id && candidate.ep === candidate.current_ep + 1 : candidate.id !== candidate.current_id;
          if (candidate.id && candidate.ep && intended) {
            const key = `${candidate.id}:${candidate.ep}`;
            if (key !== candidateKey) {
              candidateKey = key;
              // Resolve through the real shared API cache; never call warmup here or inject data.
              target = await page.evaluate(async candidate => {
                const info = await (await import('/src/api.ts')).api.play(candidate.id, candidate.ep, true);
                const supported = (await import('/src/components/VideoSurface.tsx')).canPlayHevc();
                const preferred = JSON.parse(localStorage.getItem('hghub_app_v1') || '{}').preferHevc;
                return preferred && supported && (info.streams?.hevc || info.video_url_hevc) || info.streams?.main || info.video_url;
              }, candidate);
            }
            if (target) {
              const status = await api('media_warmup_status', { url: target });
              last = { candidate, ...status, wait_ms: Math.round(performance.now() - started) };
              if ((status.state === 'ready' && status.playlist_ready && status.segment_ready && status.dependencies_ready) || (acceptQueued && status.state === 'queued')) return { observation: last, target };
              if (status.state === 'failed' && status.reason?.startsWith('unsupported_')) throw new Error(`Warm target unsupported: ${status.reason}`);
            }
          }
          await page.waitForTimeout(1500);
        }
        throw new Error(`Natural scheduler did not produce ready target: ${JSON.stringify(last)}`);
      }
      async function warmCase(name, enabled, action, acceptQueued = false) {
        try {
          let pausedPreparation;
          if (args['paused-ready']) {
            let stable=false;
            for(let attempt=0;attempt<3&&!stable;attempt++) {
              await page.waitForFunction(()=>{const v=window.__liveVideo();return v?.currentSrc&&v.readyState>=2&&!v.seeking},null,{timeout:60000});
              const source=await video.evaluate(v=>{v.pause();return v.currentSrc});
              await page.waitForTimeout(500);
              stable=await page.evaluate(source=>{const v=window.__liveVideo();return !!v&&v.currentSrc===source&&v.paused},source);
            }
            if(!stable)throw new Error('Native pause was replaced by a source transition');
            await page.waitForFunction(() => { const v=window.__liveVideo(); if(!v?.paused)return false; const wanted=Math.min(20,Math.max(0,v.duration-v.currentTime-.25)); for(let i=0;i<v.buffered.length;i++)if(v.buffered.start(i)<=v.currentTime&&v.buffered.end(i)-v.currentTime>=wanted)return true;return false; }, null, {timeout:90000});
            await setMode(!enabled);
            await page.waitForTimeout(300);
            pausedPreparation = { kind: 'actual native pause; normal HLS fills buffer; explicit play after episode switch if pause intent persists', state: await state() };
          }
          await setMode(enabled);
          const prepared = await waitWarm(acceptQueued, enabled);
          await scenario(name, () => action(prepared.observation.candidate));
          const entry = report.cases.at(-1); entry.preparation = prepared.observation; if(pausedPreparation)entry.paused_precondition=pausedPreparation;
          entry.destination = await page.locator('.player-root').last().evaluate(el => ({ id: Number(el.dataset.videoId), ep: Number(el.dataset.episode) }));
          if (entry.status === 'passed' && (entry.destination.id !== prepared.observation.candidate.id || entry.destination.ep !== prepared.observation.candidate.ep)) { entry.status = 'failed'; entry.error = 'Rendered destination does not match prepared target'; }
          if (prepared.observation.state === 'queued' && entry.status === 'passed') {
            const after = await api('media_warmup_status', { url: prepared.target });
            entry.handoff = { queued_observed: true, after_state: after.state, before_generation: prepared.observation.generation, after_generation: after.generation, same_generation: after.generation === prepared.observation.generation };
            if (after.state !== 'ready' || !entry.handoff.same_generation) { entry.status = 'failed'; entry.error = 'Queued warmup did not retain its generation through handoff'; }
          }
          if (prepared.observation.state === 'ready') entry.subsecond_target_met = entry.status === 'passed' && entry.elapsed_ms <= 1000;
        } catch (error) { report.cases.push({ name, status: 'failed', error: sanitize(error.message) }); }
      }
      const nextEpisode = async () => {
        await wake(); const old = await video.evaluate(v => v.currentSrc);
        await page.getByRole('button', { name: '下一集 E', exact: true }).last().click();
        await page.waitForFunction(old => window.__liveVideo()?.currentSrc !== old, old, { timeout: 15000 });
        if(args['paused-ready'] && await video.evaluate(v=>v.paused)) await page.locator('.player-root').last().locator('.primary-play').click();
        return null;
      };
      await warmCase('ready_next_episode', true, nextEpisode);
      if (!args['paused-ready'] && report.cases.at(-1)?.status === 'passed') await warmCase('queued_following_episode', true, nextEpisode, true);
      if(!args['episode-only']) await warmCase('ready_next_video', false, async candidate => {
        const old = await page.locator('video').last().evaluate(v => v.currentSrc);
        await page.locator('.player-root').last().locator('.overflow-y-scroll.snap-y').evaluate((el, id) => {
          const index = Array.from(el.children).findIndex(section => Number(section[Object.keys(section).find(key => key.startsWith('__reactFiber$'))]?.key) === id);
          if (index < 0) throw new Error('Prepared target absent from real feed');
          el.scrollTop = index * el.clientHeight;
        }, candidate.id);
        await page.waitForFunction(old => window.__liveVideo()?.currentSrc !== old, old, { timeout: 15000 }); return null;
      });
      await wake(); if ((await mode.getAttribute('aria-pressed')) !== swipeBefore) await mode.click();
    }
    report.quality = await page.evaluate(() => {
      const v = window.__liveVideo(), root = window.__liveVideo()?.closest('.player-root');
      return { width: v?.videoWidth, height: v?.videoHeight, resolution_label_matches: !!v?.videoWidth && root?.textContent?.includes(`${Math.min(v.videoWidth, v.videoHeight)}p`), quality: v?.getVideoPlaybackQuality?.().toJSON?.() };
    });
    if (!report.quality.resolution_label_matches) report.cases.push({ name: 'actual_resolution_label', status: 'failed', error: 'Rendered dimensions missing from quality label' });
    else report.cases.push({ name: 'actual_resolution_label', status: 'passed', dimensions: report.quality });
    try {
      const item = await page.locator('.player-root').last().evaluate(el => ({ id: Number(el.dataset.videoId), ep: Number(el.dataset.episode) }));
      const target = await page.evaluate(async item => { const info = await (await import('/src/api.ts')).api.play(item.id, item.ep, true); return info.streams?.main || info.video_url; }, item);
      const endpoint = `http://127.0.0.1:17650/hls?u=${encodeURIComponent(target)}`;
      const get = await fetch(endpoint, { redirect: 'error' });
      const bytes = (await get.arrayBuffer()).byteLength;
      const head = await fetch(endpoint, { method: 'HEAD', redirect: 'error' });
      const headBytes = (await head.arrayBuffer()).byteLength;
      const valid = get.status === 200 && head.status === 200 && Number(get.headers.get('content-length')) === bytes && head.headers.get('content-length') === get.headers.get('content-length') && headBytes === 0 && get.headers.get('accept-ranges') === 'none';
      report.cases.push({ name: 'real_http_manifest_get_head', status: valid ? 'passed' : 'failed', standard_variant: true, get_status: get.status, get_bytes: bytes, content_length: get.headers.get('content-length'), head_status: head.status, head_bytes: headBytes, accept_ranges: get.headers.get('accept-ranges') });
    } catch (error) { report.cases.push({ name: 'real_http_manifest_get_head', status: 'failed', error: sanitize(error.message) }); }
    report.final_cache = await api('cache_stats');
    await page.evaluate(async () => { const d = await import('/src/playbackDiagnostics.ts'); await d.flushPlaybackLog(); });
    const logs = await api('playback_log_read');
    // The application's diagnostic output is URL/credential-redacted already.
    await writeFile(`${output}/playback.log`, typeof logs === 'string' ? logs : JSON.stringify(logs));
    report.rendered_seek_samples = (typeof logs === 'string' ? logs : '').split('\n').flatMap(line => {
      try { const row = JSON.parse(line); if (row.event === 'web.seek.resumed' && (row.data?.at || row.ts) >= report.started) return [{ source: row.data.source, seek: row.data.seek, elapsed_ms: row.data.elapsed_ms, target: row.data.target, player: row.data.player }]; } catch {} return [];
    });
  }
  await page.screenshot({ path: `${output}/final.png` });
} catch (error) { report.error = sanitize(error.message); }
finally {
  if (page && swipeBefore !== undefined) {
    await page.evaluate(expected => { const b = document.querySelector('button[aria-label="上下划切集"]'); if (b && b.getAttribute('aria-pressed') !== expected) b.click(); }, swipeBefore).catch(() => {});
    await page.waitForFunction(expected => document.querySelector('button[aria-label="上下划切集"]')?.getAttribute('aria-pressed') === expected, swipeBefore, { timeout: 2000 }).catch(() => {});
  }
  if (page && mediaBefore) await page.evaluate(before => { const v=window.__liveVideo(); if(v){v.muted=before.muted;v.volume=before.volume;v.playbackRate=before.rate;} }, mediaBefore).catch(() => {});
  await page?.evaluate(() => window.__liveCleanup?.()).catch(() => {});
  if (loggingBefore && api) {
    await api('playback_log_set', { enabled: !!loggingBefore.enabled }).catch(() => {});
    await page.evaluate(async enabled => { const d = await import('/src/playbackDiagnostics.ts'); await d.setPlaybackLog(enabled); }, !!loggingBefore.enabled).catch(() => {});
  }
  report.finished = new Date().toISOString();
  report.passed = report.cases.filter(c => c.status === 'passed').length;
  report.failed = report.cases.filter(c => c.status !== 'passed').length;
  await writeFile(`${output}/report.json`, JSON.stringify(report, null, 2));
  console.log(JSON.stringify({ output, ...report }));
  // Disconnect this verifier without sending Browser.close to the user's WebView.
  process.exit(report.error || report.failed ? 1 : 0);
}
