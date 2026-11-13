// UI-only replay of the real 4808/E1 single-range decoder-stall sample.
import {chromium} from 'playwright-core';
import {mkdir,writeFile} from 'node:fs/promises';
import {resolve} from 'node:path';
const output=resolve(`test-results/live-buffered-stall/${new Date().toISOString().replace(/[:.]/g,'-')}`);
await mkdir(output,{recursive:true});
const report={started:new Date().toISOString(),transport:'real Tauri WebView2 + Rust + upstream/cache; UI history and episode controls only',video_id:4808,episode:1,manual_nudge:false,samples:[]};
let page,api,loggingBefore,modeBefore;
try{
 const browser=await chromium.connectOverCDP('http://127.0.0.1:9227');
 page=browser.contexts().flatMap(c=>c.pages()).find(p=>/^http:\/\/(localhost|127\.0\.0\.1):1420\//.test(p.url()));
 if(!page||!await page.evaluate(()=>'__TAURI_INTERNALS__'in window))throw new Error('Expected real HGhub WebView');
 await page.bringToFront();
 api=(cmd,params={})=>page.evaluate(async a=>(await import('/src/api.ts')).rawCall(a.cmd,a.params),{cmd,params});
 loggingBefore=await api('playback_log_status');
 await page.evaluate(async()=>{const d=await import('/src/playbackDiagnostics.ts');await d.initPlaybackLog();await d.setPlaybackLog(true)});
 await page.getByRole('button',{name:'我的',exact:true}).click();
 const history=page.getByRole('button',{name:'观看历史',exact:true});await history.click();
 const panel=history.locator('xpath=..').locator('xpath=following-sibling::div[1]');
 const cards=panel.locator('.grid.grid-cols-3 > button');await cards.first().waitFor({state:'visible',timeout:45000});
 const keys=await cards.evaluateAll(nodes=>nodes.map(el=>Number(el[Object.keys(el).find(k=>k.startsWith('__reactFiber$'))]?.key)));
 const index=keys.indexOf(4808);if(index<0)throw new Error('Known sample absent from actual history');
 await cards.nth(index).click();
 const root=page.locator('.player-root').last();
 await page.waitForFunction(()=>Array.from(document.querySelectorAll('.player-root')).at(-1)?.dataset.videoId==='4808');
 await page.waitForFunction(()=>{const v=Array.from(document.querySelectorAll('video')).at(-1);return v?.currentSrc&&v.readyState>=2},null,{timeout:60000});
 const change=async(ep,action)=>{
  const before=await root.evaluate(r=>({id:Number(r.dataset.videoId),ep:Number(r.dataset.episode),src:r.querySelector('video')?.currentSrc}));
  if(before.id!==4808)throw new Error('Sample changed before episode action');
  const box=await root.boundingBox();if(box)await page.mouse.move(box.x+box.width*.4,box.y+box.height*.4);
  await action();
  await page.waitForFunction(({ep,src})=>{const r=Array.from(document.querySelectorAll('.player-root')).at(-1),v=r?.querySelector('video');return r?.dataset.videoId==='4808'&&Number(r.dataset.episode)===ep&&v?.currentSrc&&v.currentSrc!==src&&v.readyState>=2&&!v.seeking},{ep,src:before.src},{timeout:60000});
 };
 // Use existing next/keyboard controls, without opening the width-animating episode dock.
 const box=await root.boundingBox();if(box)await page.mouse.move(box.x+box.width*.4,box.y+box.height*.4);
 const mode=root.getByRole('button',{name:'上下划切集',exact:true});
 modeBefore=await mode.getAttribute('aria-pressed');if(modeBefore!=='true')await mode.click();
 let ep=Number(await root.getAttribute('data-episode'));
 if(ep===1){await change(2,()=>root.getByRole('button',{name:'下一集 E',exact:true}).click());ep=2;}
 if(ep>10)throw new Error('Known sample episode outside bounded replay path');
 while(ep>1){await change(ep-1,()=>page.keyboard.press('ArrowUp'));ep--;}

 await page.waitForFunction(()=>{const v=Array.from(document.querySelectorAll('video')).at(-1);return v&&!v.paused&&v.readyState>=2&&v.currentTime>0&&v.currentTime<5},null,{timeout:60000});
 report.actor_steps=['profile.history.open(4808)','next/previous episode UI controls to enter fresh E1','natural playback across 29.962s'];
 const snapshot=()=>page.evaluate(()=>{const r=Array.from(document.querySelectorAll('.player-root')).at(-1),v=r.querySelector('video'),q=v.getVideoPlaybackQuality();let ahead=0;for(let i=0;i<v.buffered.length;i++)if(v.buffered.start(i)<=v.currentTime&&v.buffered.end(i)>v.currentTime)ahead=v.buffered.end(i)-v.currentTime;return{id:Number(r.dataset.videoId),ep:Number(r.dataset.episode),time:v.currentTime,ready:v.readyState,paused:v.paused,seeking:v.seeking,ahead,frames:q.totalVideoFrames,dropped:q.droppedVideoFrames,width:v.videoWidth,height:v.videoHeight,duration:v.duration,error:v.error?.code||null}});
 const first=await snapshot();report.initial=first;if(Math.abs(first.duration-595.733)>.25)throw new Error('Known sample duration changed; no identical-seam claim');
 const began=performance.now();let after=first;
 while(performance.now()-began<75000){
  after=await snapshot();report.samples.push({elapsed_ms:Math.round(performance.now()-began),...after});
  if(after.id!==4808||after.ep!==1||after.error)throw new Error('Sample changed or media error');
  if(after.time>=40)break;
  await page.waitForTimeout(1000);
 }
 report.nudge_events=await page.evaluate(async since=>{
  const text=await(await import('/src/api.ts')).rawCall('playback_log_read');
  return text.split('\n').flatMap(line=>{try{return[JSON.parse(line)]}catch{return[]}})
   .filter(e=>Date.parse(e.ts)>=Date.parse(since)&&e.data?.video_id===4808&&e.data?.episode===1&&e.event?.startsWith('web.recovery.buffered_stall.'))
   .map(e=>({at:e.ts,event:e.event,time:e.data.time,attempt:e.data.attempt,target:e.data.target,ahead_s:e.data.ahead_s,window_attempts:e.data.window_attempts}));
 },report.started);
 report.recovery_observed=report.nudge_events.some(e=>e.event.endsWith('.nudge'));
 report.functional_pass=after.time>=40&&!after.paused&&!after.error;
 report.classification=report.recovery_observed?'automatic bounded recovery observed':'no stall reproduced in this replay; natural seam crossing only';
}catch(e){report.error=String(e.message).replace(/https?:\/\/[^\s"<>]+/g,'[resource-url]');report.functional_pass=false;}
finally{
 if(page&&modeBefore!==undefined)try{const r=page.locator('.player-root').last(),m=r.getByRole('button',{name:'上下划切集',exact:true});if(await m.getAttribute('aria-pressed')!==modeBefore){const b=await r.boundingBox();if(b)await page.mouse.move(b.x+b.width*.4,b.y+b.height*.4);await m.click();}report.mode_restored=true}catch{report.mode_restored=false}
 if(loggingBefore&&page)try{await page.evaluate(async enabled=>(await import('/src/playbackDiagnostics.ts')).setPlaybackLog(enabled),loggingBefore.enabled);report.logging_restored=true}catch{report.logging_restored=false}
 report.finished=new Date().toISOString();await writeFile(`${output}/report.json`,JSON.stringify(report,null,2));console.log(JSON.stringify({output,...report}));process.exit(report.functional_pass?0:1);
}
