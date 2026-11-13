// 只验证本批离线逻辑/类型/语法；不启动项目、浏览器或网络服务，不调用cargo。
import { spawn } from 'node:child_process';
import { mkdir, writeFile } from 'node:fs/promises';
import { performance } from 'node:perf_hooks';
import { setPriority } from 'node:os';
const directory = 'test-results/viewing-implementation-20261113-1923';
await mkdir(directory, { recursive: true });
const stages = [
  ['warmup', process.execPath, ['--experimental-strip-types', '--max-old-space-size=128', 'scripts/test-viewing-warmup.mjs']],
  ['core', process.execPath, ['--experimental-strip-types', '--max-old-space-size=128', 'scripts/test-viewing-core.mjs']],
  ['activity', process.execPath, ['--experimental-strip-types', '--max-old-space-size=128', 'scripts/test-playback-activity-light.mjs']],
  ['typescript', process.execPath, ['--max-old-space-size=256', 'node_modules/typescript/bin/tsc', '--project', 'tsconfig.tests.json', '--noEmit', '--incremental', 'false']],
  ['rust_syntax_format_only', 'rustfmt', ['--edition', '2021', '--config', 'skip_children=true', '--check', 'src-tauri/src/net.rs', 'src-tauri/src/net_download_metrics.rs', 'src-tauri/src/net_ranged_tests.rs', 'src-tauri/src/playback_log.rs', 'src-tauri/src/media.rs']],
  ['diff_whitespace', 'git', ['-c', 'core.whitespace=blank-at-eol,blank-at-eof,space-before-tab,cr-at-eol', 'diff', '--check']],
];
const results = [];
for (const [name, executable, args] of stages) {
  const started = performance.now();
  const result = await new Promise(resolve => {
    const child = spawn(executable, args, { stdio: ['ignore', 'pipe', 'pipe'], windowsHide: true });
    let stdout = '', stderr = '', loweredPriority = false, error;
    try { if (child.pid) { setPriority(child.pid, 10); loweredPriority = true; } } catch { /* 仅最佳努力，不修改其它进程。 */ }
    child.stdout.on('data', data => { stdout += data; if (stdout.length > 262144) child.kill(); });
    child.stderr.on('data', data => { stderr += data; if (stderr.length > 262144) child.kill(); });
    child.on('error', value => { error = value.message; });
    child.on('close', (exitCode, signal) => resolve({ name, args, exit_code: exitCode, signal, lowered_priority: loweredPriority, elapsed_ms: Math.round(performance.now() - started), stdout, stderr, error }));
  });
  results.push(result);
  console.log(JSON.stringify({ stage: name, exit_code: result.exit_code, elapsed_ms: result.elapsed_ms }));
  if (result.exit_code !== 0) break;
}
const passed = results.length === stages.length && results.every(item => item.exit_code === 0);
const report = { created: new Date().toISOString(), scope: 'serial offline validation; no application/browser/upstream or Cargo build', passed, stages: results, heap_limits_mib: { parent: 96, pure_child: 128, typecheck_child: 256 }, note: 'V8 heap limits are not total process-RSS hard limits. Rustfmt is parse/style only, not Rust typecheck/test. Hook model is not browser/React DOM validation.' };
await writeFile(directory + '/validation.json', JSON.stringify(report, null, 2) + '\n');
if (!passed) process.exitCode = 1;
