// Observation-only sustained playback check in the already running HGhub WebView.
import {chromium} from 'playwright-core';
import {mkdir,writeFile} from 'node:fs/promises';
import {resolve,dirname} from 'node:path';
const output=resolve(`test-results/live-steady/${new Date().toISOString().replace(/[:.]/g,'-')}`);
await mkdir(dirname(output),{recursive:true});await mkdir(output);
const report={started:new Date().toISOString(),transport:'real Tauri WebView2; no API/media/IPC mocking',window_ms:20000};
try{
 const browser=await chromium.connectOverCDP('http://127.0.0.1:9227');
 const page=browser.contexts().flatMap(c=>c.pages()).find(p=>/^http:\/\/(localhost|127\.0\.0\.1):1420\//.test(p.url()));
 if(!page||!await page.evaluate(()=>'__TAURI_INTERNALS__'in window))throw new Error('Real HGhub WebView missing');
 await page.bringToFront();
 await page.waitForFunction(() => { const v=Array.from(document.querySelectorAll('video')).filter(v=>v.getClientRects().length).at(-1); return v&&!v.paused&&v.readyState>=2&&v.duration-v.currentTime>v.playbackRate*22; },null,{timeout:90000});
 const metrics=await page.evaluate(ms=>new Promise((done,reject)=>{
  const v=Array.from(document.querySelectorAll('video')).filter(v=>v.getClientRects().length).at(-1);
  if(!v||v.paused||v.readyState<2||v.duration-v.currentTime<ms/1000*v.playbackRate+2){reject(new Error('Need an already playing source with enough remaining duration'));return;}
  const quality=()=>{const q=v.getVideoPlaybackQuality();return {totalVideoFrames:q.totalVideoFrames,droppedVideoFrames:q.droppedVideoFrames}};
  const start=performance.now(),source=v.currentSrc,before={time:v.currentTime,rate:v.playbackRate,...quality()},events=[];
  let waitingSince=v.readyState<3?start:null,waitingMs=0;
  const wait=()=>{if(waitingSince===null)waitingSince=performance.now();events.push('waiting')};
  const play=()=>{if(waitingSince!==null){waitingMs+=performance.now()-waitingSince;waitingSince=null;}};
  const err=()=>events.push('media_error');
  v.addEventListener('waiting',wait);v.addEventListener('playing',play);v.addEventListener('error',err);
  setTimeout(()=>{
   v.removeEventListener('waiting',wait);v.removeEventListener('playing',play);v.removeEventListener('error',err);
   if(waitingSince!==null)waitingMs+=performance.now()-waitingSince;
   const after={time:v.currentTime,...quality()},elapsed=performance.now()-start;
   const frames=after.totalVideoFrames-before.totalVideoFrames,dropped=after.droppedVideoFrames-before.droppedVideoFrames;
   done({elapsed_ms:elapsed,media_advance_s:after.time-before.time,rate:before.rate,width:v.videoWidth,height:v.videoHeight,frames,dropped,dropped_ratio:frames>0?dropped/frames:null,waiting_ms:waitingMs,rebuffer_ratio:waitingMs/elapsed,advance_deficit_s:Math.max(0,elapsed/1000-(after.time-before.time)/before.rate),waiting_events:events.filter(e=>e==='waiting').length,source_unchanged:v.currentSrc===source,paused:v.paused,media_error:v.error?.code||null,events});
  },ms);
 }),report.window_ms);
 Object.assign(report,metrics);
 report.functional_pass=metrics.source_unchanged&&!metrics.paused&&!metrics.media_error&&metrics.frames>0&&metrics.media_advance_s>0;
 report.smoothness_target_met=report.functional_pass&&metrics.rebuffer_ratio<=.01&&metrics.dropped_ratio<=.01;
 report.actual_hd=Math.min(metrics.width,metrics.height)>=720;
}catch(e){report.error=String(e.message).replace(/https?:\/\/[^\s"<>]+/g,'[resource-url]');report.functional_pass=false;}
finally{report.finished=new Date().toISOString();await writeFile(`${output}/report.json`,JSON.stringify(report,null,2));console.log(JSON.stringify({output,...report}));process.exit(report.functional_pass?0:1);}
