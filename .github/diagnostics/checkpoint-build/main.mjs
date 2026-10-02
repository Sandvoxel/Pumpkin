import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { createReadStream } from 'node:fs';
import { readFile, writeFile, mkdir, readdir } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const variant = process.env.VARIANT;
if (!['baseline', 'prototype'].includes(variant)) throw new Error('Invalid diagnostic variant');
const runId = process.env.GITHUB_RUN_ID;
const attempt = process.env.GITHUB_RUN_ATTEMPT;
if (!/^\d+$/.test(runId) || !/^\d+$/.test(attempt)) throw new Error('Invalid run identity');
const scratch = path.join(process.env.RUNNER_TEMP, `thin-checkpoints-${variant}-${runId}-${attempt}`);
await mkdir(scratch, { recursive: true });
const state = { variant, run_id: runId, attempt, artifacts: [], failures: [] };
let monitor;
let interrupted = false;
for (const signal of ['SIGINT', 'SIGTERM']) process.on(signal, () => {
  interrupted = true;
  monitor?.kill(signal);
});

async function hashFile(file) {
  const hash = createHash('sha256');
  for await (const chunk of createReadStream(file)) hash.update(chunk);
  return hash.digest('hex');
}

async function saveState() {
  await writeFile('diagnostic-output/checkpoint-state.json', JSON.stringify(state, null, 2) + '\n');
}

function run(command, args, { visible = false, timeout = 300000, measured = false } = {}) {
  return new Promise((resolve, reject) => {
    const child = spawn(command, args, { stdio: visible ? 'inherit' : ['ignore', 'pipe', 'pipe'] });
    if (measured) monitor = child;
    let output = '';
    child.stdout?.on('data', chunk => { if (output.length < 65536) output += chunk.toString(); });
    // Discard SDK stderr instead of risking signed URLs in an error message.
    child.stderr?.on('data', () => {});
    const timer = timeout ? setTimeout(() => child.kill('SIGKILL'), timeout) : null;
    child.on('error', () => { clearTimeout(timer); reject(new Error('Diagnostic subprocess could not start')); });
    child.on('close', (code, signal) => {
      clearTimeout(timer);
      resolve({ code, signal, output });
    });
  });
}

let telemetryOffset = 0;
async function checkpoint(label, kind, verify) {
  await saveState();
  const started = Date.now();
  const file = path.join(scratch, `${label}.tar.gz`);
  const packed = await run('python3', [path.join(here, 'bundle.py'), kind, file, String(telemetryOffset)]);
  if (packed.code !== 0) throw new Error(`Could not prepare bounded ${label} archive`);
  const bundle = JSON.parse(packed.output);
  const sha256 = await hashFile(file);
  const requestPath = path.join(scratch, `${label}-request.json`);
  const resultPath = path.join(scratch, `${label}-result.json`);
  const request = { name: `${variant}-profile-${runId}-${attempt}-${label}`, file,
    result: resultPath, download: path.join(scratch, `${label}-download`), verify, sha256 };
  await writeFile(requestPath, JSON.stringify(request));
  const uploaded = await run(process.execPath, [path.join(here, 'upload.mjs'), requestPath]);
  if (uploaded.code !== 0) throw new Error(`Artifact ${label} upload or verification failed`);
  const result = JSON.parse(await readFile(resultPath, 'utf8'));
  if (result.error || !result.id || verify && !result.verified)
    throw new Error(`Artifact ${label} result incomplete`);
  const record = { label, ...result, archive_sha256: sha256, ...bundle,
    elapsed_seconds: (Date.now() - started) / 1000, utc: new Date().toISOString() };
  state.artifacts.push(record);
  if (kind === 'periodic') telemetryOffset = bundle.telemetry_end;
  await saveState();
  console.log(`CHECKPOINT ${JSON.stringify(record)}`);
}

const uploadedProfiles = new Set();
async function completedProfiles() {
  const profileRoot = 'diagnostic-output/profiles';
  for (const name of await readdir(profileRoot)) {
    if (!/^(pumpkin|pumpkin_wasm_host_v0_[12])-[0-9]+$/.test(name) || uploadedProfiles.has(name)) continue;
    try { await readFile(path.join(profileRoot, name, 'result.json')); } catch { continue; }
    await checkpoint(`trace-${name}`, `profile:${name}`, false);
    uploadedProfiles.add(name);
  }
}

function waitUntilOrExit(deadline, finished) {
  return new Promise(resolve => {
    const timer = setTimeout(() => resolve(false), Math.max(0, deadline - Date.now()));
    finished.then(() => { clearTimeout(timer); resolve(true); });
  });
}

function oomCount(values) {
  return Number((values?.['memory.events'] || '').match(/^oom_kill (\d+)$/m)?.[1] || 0);
}

try {
  if (!process.env.ACTIONS_RUNTIME_TOKEN || !process.env.ACTIONS_RESULTS_URL)
    throw new Error('Existing Actions artifact runtime authorization unavailable');
  const probe = await run('python3', [path.join(here, '..', 'monitor.py'), '--probe'], { visible: true });
  if (probe.code !== 0) throw new Error('Telemetry probe failed before build');
  const probeResult = JSON.parse(await readFile('diagnostic-output/probe/result.json', 'utf8'));
  const probeSnapshot = JSON.parse(await readFile('diagnostic-output/probe/snapshot.json', 'utf8'));
  const groups = Object.values(probeSnapshot.current.cgroups);
  if (probeResult.exit_code !== 0 || probeResult.maximum_sampled_aggregate_rss_bytes < 16 * 1024 * 1024 ||
      !probeResult.process_peaks.some(p => p.cpu_seconds > 0) ||
      !(probeSnapshot.current.memory.MemAvailable_kib > 0) ||
      !groups.some(g => /^\d+$/.test(g['memory.current'] || '') && /oom_kill \d+/.test(g['memory.events'] || '')) ||
      !(await readFile('diagnostic-output/probe/time.txt', 'utf8')).includes('Maximum resident set size'))
    throw new Error('Telemetry probe evidence incomplete');
  await checkpoint('preflight', 'preflight', true);
  console.log('PREFLIGHT_OK: sampled CPU/RSS, memory, cgroup counters and time; artifact round trip verified.');
  if (interrupted) throw new Error('Interrupted before build');

  const started = Date.now();
  const finished = run('python3', [path.join(here, '..', 'monitor.py')],
    { visible: true, timeout: 0, measured: true });
  for (const minute of [5, 10, 15, 25, 45]) {
    if (await waitUntilOrExit(started + minute * 60000, finished)) break;
    try { await checkpoint(`minute-${minute}`, 'periodic', false); await completedProfiles(); }
    catch {
      state.failures.push({ checkpoint_minute: minute, utc: new Date().toISOString() });
      await saveState();
      console.log(`CHECKPOINT_FAILED minute=${minute}; full local telemetry retained for final archive`);
    }
  }
  const build = await finished;
  monitor = undefined;
  const measured = JSON.parse(await readFile('diagnostic-output/result.json', 'utf8'));
  state.build = { code: build.code, signal: build.signal,
    controller_elapsed_seconds: (Date.now() - started) / 1000,
    monitor_elapsed_seconds: measured.elapsed_seconds };
  const footprint = await run('bash', [path.join(here, '..', 'footprint.sh')], { visible: true });
  state.footprint_exit_code = footprint.code;
  const snapshot = JSON.parse(await readFile('diagnostic-output/snapshot.json', 'utf8'));
  state.oom_kill_deltas = Object.fromEntries(Object.entries(snapshot.current.cgroups)
    .map(([name, group]) => [name, oomCount(group) - oomCount(snapshot.before.cgroups[name])]));
  const log = await readFile('diagnostic-output/build.log', 'utf8');
  const finalRustc = log.split('\n').filter(line => line.includes('--crate-name pumpkin '));
  state.final_rustc_thin_cgu1_verified = finalRustc.some(line =>
    line.includes('-C lto=thin') && line.includes('-C codegen-units=1') &&
    line.includes('--target aarch64-unknown-linux-musl'));
  await saveState();
  await completedProfiles();
  await checkpoint('final', 'final', true);
  const success = build.code === 0 && footprint.code === 0 && state.final_rustc_thin_cgu1_verified &&
    Object.values(state.oom_kill_deltas).every(delta => delta === 0) && !interrupted;
  console.log(`BUILD_RESULT ${JSON.stringify({ ...state.build, evidence_verified: true,
    final_rustc_thin_cgu1_verified: state.final_rustc_thin_cgu1_verified,
    oom_kill_deltas: state.oom_kill_deltas, success })}`);
  process.exitCode = success ? 0 : 1;
} catch (error) {
  monitor?.kill('SIGTERM');
  // All messages created here are fixed diagnostic text, never SDK/network errors.
  console.log(`DIAGNOSTIC_FAILED: ${error instanceof Error ? error.message : 'Unknown diagnostic error'}`);
  process.exitCode = 1;
}
