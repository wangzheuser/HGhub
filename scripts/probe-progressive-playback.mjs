// Isolated feasibility probe only: local AES-128 TS fixture, never production APIs/media.
// The desktop custom protocol remains full-response; this probe does not enable streaming there.
import assert from 'node:assert/strict';
import {createServer} from 'node:http';
import {createCipheriv} from 'node:crypto';
import {execFileSync} from 'node:child_process';
import {readFile,writeFile,mkdir} from 'node:fs/promises';
import {resolve,join} from 'node:path';
import {parseArgs} from 'node:util';
import {chromium} from 'playwright-core';
const {values:args}=parseArgs({options:{output:{type:'string'},'browser-executable':{type:'string'}}});
const output=resolve(args.output||`test-results/progressive-probe/${new Date().toISOString().replace(/[:.]/g,'-')}`);
await mkdir(resolve(output,'..'),{recursive:true});await mkdir(output);
const fixture=join(output,'fixture.ts');
execFileSync(process.env.FFMPEG_PATH||'ffmpeg',['-hide_banner','-loglevel','error','-filter_threads','1','-f','lavfi','-i','testsrc2=size=320x180:rate=15','-f','lavfi','-i','sine=frequency=440:sample_rate=44100','-t','8','-c:v','libx264','-threads','1','-preset','ultrafast','-g','15','-sc_threshold','0','-pix_fmt','yuv420p','-c:a','aac','-f','mpegts',fixture],{stdio:'inherit',timeout:30000});
const clear=await readFile(fixture);const key=Buffer.alloc(16,37),iv=Buffer.alloc(16);
const cipher=createCipheriv('aes-128-cbc',key,iv);const encrypted=Buffer.concat([cipher.update(clear),cipher.final()]);
const hls=await readFile('node_modules/hls.js/dist/hls.mjs');
const chunkSize=16384,interval=60,totalMs=Math.ceil(encrypted.length/chunkSize)*interval;
const report={started:new Date().toISOString(),transport:'isolated local HTTP AES-128 TS; not native Tauri',upstream_requests:0,production_streaming_enabled:false,bytes:encrypted.length,simulated_complete_ms:totalMs,cases:[]};
const sockets=new Set(),timers=new Set();let browser;
const server=createServer((req,res)=>{
 const url=new URL(req.url,'http://localhost');
 if(url.pathname==='/hls.mjs'){res.writeHead(200,{'Content-Type':'text/javascript'});res.end(hls);return;}
 if(url.pathname==='/key'){res.writeHead(200,{'Content-Type':'application/octet-stream','Content-Length':16});res.end(key);return;}
 if(url.pathname==='/list.m3u8'){
  res.writeHead(200,{'Content-Type':'application/vnd.apple.mpegurl'});
  res.end(`#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:9\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-KEY:METHOD=AES-128,URI="/key",IV=0x00000000000000000000000000000000\n#EXTINF:8.0,\n/${url.searchParams.get('mode')}.ts\n#EXT-X-ENDLIST\n`);return;
 }
 if(url.pathname==='/whole.ts'){
  const timer=setTimeout(()=>{timers.delete(timer);res.writeHead(200,{'Content-Type':'video/mp2t','Content-Length':encrypted.length});res.end(encrypted)},totalMs);
  timers.add(timer);res.on('close',()=>{clearTimeout(timer);timers.delete(timer)});return;
 }
 if(url.pathname==='/progressive.ts'){
  res.writeHead(200,{'Content-Type':'video/mp2t','Content-Length':encrypted.length});res.flushHeaders();let offset=0;
  const timer=setInterval(()=>{if(res.destroyed){clearInterval(timer);timers.delete(timer);return;}const end=Math.min(offset+chunkSize,encrypted.length);res.write(encrypted.subarray(offset,end));offset=end;if(offset===encrypted.length){clearInterval(timer);timers.delete(timer);res.end();}},interval);
  timers.add(timer);res.on('close',()=>{clearInterval(timer);timers.delete(timer)});return;
 }
 if(url.pathname==='/'){
  res.writeHead(200,{'Content-Type':'text/html'});
  res.end(`<!doctype html><meta charset="utf-8"><video muted playsinline width="640" height="360"></video><script type="module">
import Hls from '/hls.mjs';let engine;
window.run=async mode=>{
 const v=document.querySelector('video');engine?.destroy();v.pause();v.removeAttribute('src');v.load();
 const start=performance.now(),errors=[],qualityStart=v.getVideoPlaybackQuality?.();let loaded=null,firstByte=null;
 const h=engine=new Hls({progressive:mode==='progressive',enableWorker:true,maxBufferLength:10,manifestLoadingMaxRetry:0,fragLoadingMaxRetry:0});
 h.on(Hls.Events.FRAG_LOADED,(_e,d)=>{loaded=d.frag.stats.loading.end-d.frag.stats.loading.start;firstByte=d.frag.stats.loading.first-d.frag.stats.loading.start});
 return new Promise((done,reject)=>{
  const timer=setTimeout(()=>reject(new Error('No decoded frame within deadline')),20000);
  h.on(Hls.Events.ERROR,(_e,d)=>{errors.push({type:d.type,detail:d.details,fatal:d.fatal});if(d.fatal){clearTimeout(timer);reject(new Error(d.type+'/'+d.details))}});
  v.addEventListener('playing',()=>{v.requestVideoFrameCallback(()=>{clearTimeout(timer);const q=v.getVideoPlaybackQuality?.();done({mode,first_frame_ms:performance.now()-start,complete_load_ms_at_first_frame:loaded,first_byte_ms_at_first_frame:firstByte,width:v.videoWidth,height:v.videoHeight,frames:q?.totalVideoFrames,dropped:q?.droppedVideoFrames,errors,progressive:h.config.progressive,worker:h.config.enableWorker})})},{once:true});
  h.on(Hls.Events.MANIFEST_PARSED,()=>void v.play().catch(reject));h.loadSource('/list.m3u8?mode='+mode);h.attachMedia(v);
 });
};window.ready=true;
</script>`);return;
 }
 res.writeHead(404);res.end();
});
server.on('connection',s=>{sockets.add(s);s.on('close',()=>sockets.delete(s))});
try{
 await new Promise((ok,bad)=>{server.once('error',bad);server.listen(0,'127.0.0.1',ok)});
 const origin=`http://127.0.0.1:${server.address().port}`;
 browser=await chromium.launch({headless:true,executablePath:args['browser-executable'],args:['--autoplay-policy=no-user-gesture-required']});
 const context=await browser.newContext();await context.route('**/*',route=>new URL(route.request().url()).origin===origin?route.continue():route.abort());
 const page=await context.newPage();await page.goto(origin);await page.waitForFunction(()=>window.ready);
 report.browser=browser.version();
 for(const mode of ['whole','progressive']){const metrics=await page.evaluate(mode=>window.run(mode),mode);report.cases.push(metrics);console.log(JSON.stringify(metrics));}
 const [whole,progressive]=report.cases;
 assert(whole.first_frame_ms>=totalMs-100,'Buffered baseline must wait for full delivery');
 assert(progressive.progressive,'Streaming loader must actually be enabled');
 assert(progressive.first_frame_ms<totalMs-200,'Streaming AES must produce a real frame before full object delivery');
 report.status='passed';
}catch(e){report.status='failed';report.error=e.message;}
finally{
 await browser?.close();for(const t of timers){clearTimeout(t);clearInterval(t)}for(const s of sockets)s.destroy();await new Promise(done=>server.close(done));
 report.finished=new Date().toISOString();await writeFile(join(output,'report.json'),JSON.stringify(report,null,2));console.log(JSON.stringify({output,...report}));process.exitCode=report.status==='passed'?0:1;
}
