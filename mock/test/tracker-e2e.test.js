// End-to-end checks of the real tracker binary against synthetic Claude and Codex files and the
// mock receiver. Live processes stand in for agents: `sleep` for Claude sessions, and this test
// process holds a Codex thread lock open. Build first: cargo build --release (from the repo root).
//
// Covers the verification plan in docs/DESIGN.md: 1, 2 and 6 at the integration level, and
// 3, 4, 5, 10, 11 and 12.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawn, execFileSync } from 'node:child_process';
import { createHash, randomBytes } from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import { startMockReceiver, STATUS_PATH } from '../receiver.js';
import { validateMessage } from '../validate.js';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..');
const BINARY = path.join(ROOT, 'target/release/agent-heartbeat');
const CANARY = 'CANARY-CONTENT-e2e-5d1b';
const CANARY_PATH = 'CANARY-PATH-e2e-77aa';
const THREAD = '01a0e2e0-0000-7000-8000-0000000000e2';

const skip = fs.existsSync(BINARY) ? false : 'build the binary first: cargo build --release';

function line(object) {
  return `${JSON.stringify(object)}\n`;
}

function lstartUtc(pid) {
  return execFileSync('ps', ['-o', 'lstart=', '-p', String(pid)], { env: { ...process.env, TZ: 'UTC' } }).toString().trim();
}

/** Every file under dir with its size, mtime and hash. Used to prove the sources stay untouched. */
function fingerprint(dir) {
  const out = {};
  for (const entry of fs.readdirSync(dir, { recursive: true, withFileTypes: true })) {
    if (!entry.isFile()) continue;
    const full = path.join(entry.parentPath, entry.name);
    const stat = fs.statSync(full);
    out[full] = `${stat.size}:${stat.mtimeMs}:${createHash('sha256').update(fs.readFileSync(full)).digest('hex')}`;
  }
  return out;
}

function makeSources(base) {
  const claude = path.join(base, 'claude');
  const codex = path.join(base, 'codex');
  const now = new Date().toISOString();
  const project = path.join(claude, 'projects', `-Users-x-${CANARY_PATH}`);
  fs.mkdirSync(path.join(project, 's-e2e', 'subagents'), { recursive: true });
  fs.mkdirSync(path.join(claude, 'sessions'), { recursive: true });
  const assistant = (id, stop, usage) =>
    line({
      parentUuid: 'p',
      isSidechain: false,
      message: { model: 'claude-opus-5-5', id, type: 'message', role: 'assistant', content: [{ type: 'text', text: CANARY }], stop_reason: stop, usage },
      requestId: `req-${id}`,
      type: 'assistant',
      timestamp: now,
      cwd: `/Users/x/${CANARY}`,
      gitBranch: CANARY,
    });
  fs.writeFileSync(path.join(project, 's-e2e.jsonl'), assistant('m1', 'end_turn', { input_tokens: 10, cache_creation_input_tokens: 20, cache_read_input_tokens: 70, output_tokens: 5 }));
  const child = path.join(project, 's-e2e', 'subagents', 'agent-e2e.jsonl');
  fs.writeFileSync(child, assistant('m2', 'tool_use', { input_tokens: 1, cache_read_input_tokens: 9, output_tokens: 2 }));

  const day = path.join(codex, 'sessions', '2026', '10', '02');
  fs.mkdirSync(day, { recursive: true });
  fs.mkdirSync(path.join(codex, 'thread-writer-locks'), { recursive: true });
  fs.writeFileSync(path.join(codex, 'thread-writer-locks', `${THREAD}.lock`), '');
  const rollout = path.join(day, `rollout-2026-10-02T10-00-00-${THREAD}.jsonl`);
  fs.writeFileSync(
    rollout,
    line({ timestamp: now, type: 'session_meta', payload: { id: THREAD, cwd: `/Users/x/${CANARY}`, source: 'vscode', base_instructions: { text: CANARY } } }) +
      line({ timestamp: now, type: 'event_msg', payload: { type: 'task_started', turn_id: 'turn-1' } }) +
      line({
        timestamp: now,
        type: 'token_usage_record',
        payload: { response_id: 'resp-1', usage: { input_tokens: 400, cached_input_tokens: 300, output_tokens: 6, total_tokens: 406 } },
      }),
  );
  return { claude, codex, child, rollout };
}

function registry(claudeHome, pid, status) {
  const entry = { pid, sessionId: 's-e2e', cwd: `/Users/x/${CANARY}`, procStart: lstartUtc(pid), status, name: CANARY, kind: 'interactive' };
  fs.writeFileSync(path.join(claudeHome, 'sessions', `${pid}.json`), JSON.stringify(entry));
}

async function waitFor(what, read, check, ms = 15_000) {
  const end = Date.now() + ms;
  let last;
  while (Date.now() < end) {
    last = await read();
    if (check(last)) return last;
    await new Promise((r) => setTimeout(r, 200));
  }
  assert.fail(`timed out waiting for ${what}; last value: ${JSON.stringify(last)}`);
}

test('the real tracker against the mock receiver', { skip, timeout: 120_000 }, async (t) => {
  const base = fs.mkdtempSync(path.join(os.tmpdir(), 'ah-e2e-'));
  const sources = makeSources(base);
  const before = fingerprint(base);
  const sleeper = spawn('sleep', ['300']);
  registry(sources.claude, sleeper.pid, 'busy');
  const lock = fs.openSync(path.join(sources.codex, 'thread-writer-locks', `${THREAD}.lock`), 'r'); // a live holder

  const secret = randomBytes(32).toString('hex');
  const receiver = await startMockReceiver({ port: 0, host: '127.0.0.1', secret });
  const stateDir = path.join(base, 'state');
  fs.mkdirSync(stateDir, { mode: 0o700 });
  fs.writeFileSync(path.join(stateDir, 'secret'), secret, { mode: 0o600 });
  fs.writeFileSync(
    path.join(stateDir, 'config.json'),
    JSON.stringify({ endpoint: `${receiver.url}/api/workstation/heartbeat`, secretFile: path.join(stateDir, 'secret'), activeIntervalSeconds: 1, idleIntervalSeconds: 2 }),
  );
  const env = { ...process.env, AGENT_HEARTBEAT_DIR: stateDir, AGENT_HEARTBEAT_CLAUDE_HOME: sources.claude, AGENT_HEARTBEAT_CODEX_HOME: sources.codex };
  const status = async () => (await fetch(`${receiver.url}${STATUS_PATH}`)).json();
  const startTracker = () => spawn(BINARY, ['run'], { env, stdio: 'ignore' });
  let tracker;
  t.after(async () => {
    tracker?.kill('SIGKILL');
    sleeper.kill();
    fs.closeSync(lock);
    await receiver.close();
  });

  await t.test('10: dry-run prints only approved fields and passes the schema', () => {
    const out = execFileSync(BINARY, ['dry-run'], { env }).toString();
    const payload = JSON.parse(out);
    assert.equal(validateMessage(payload).ok, true);
    assert.deepEqual(Object.keys(payload), ['schemaVersion', 'type', 'observedAt', 'ttlSeconds', 'usageDate', 'timeZone', 'agents', 'tokens']);
    assert.ok(!out.includes(CANARY) && !out.includes(CANARY_PATH) && !out.includes('s-e2e') && !out.includes(THREAD) && !out.includes('resp-1'));
    // 1: the busy session, its running child, and the Codex thread with an open turn and a held lock.
    assert.deepEqual(payload.agents, { active: 3, coverage: 'complete' });
    assert.deepEqual(payload.tokens, { input: 510, cachedInput: 379, output: 13, total: 523, coverage: 'complete' });
  });

  await t.test('the tracker comes online with the same numbers', async () => {
    tracker = startTracker();
    const s = await waitFor('online', status, (s) => s.status === 'online');
    assert.equal(s.agents.active, 3);
    assert.equal(s.tokensToday.total, 523);
  });

  await t.test('2 and 3: idle agents do not count, and zero active agents shows Offline while connected', async () => {
    registry(sources.claude, sleeper.pid, 'idle');
    fs.appendFileSync(sources.rollout, line({ timestamp: new Date().toISOString(), type: 'event_msg', payload: { type: 'task_complete', turn_id: 'turn-1' } }));
    // The child keeps running after its parent's turn ended: 1 active.
    await waitFor('one active child', status, (s) => s.agents.active === 1);
    fs.appendFileSync(sources.child, line({ type: 'assistant', message: { role: 'assistant', id: 'm3', stop_reason: 'end_turn' }, requestId: 'req-m3', timestamp: new Date().toISOString() }));
    const s = await waitFor('zero active', status, (s) => s.agents.active === 0);
    assert.equal(s.status, 'offline');
    assert.equal(s.tracker, 'connected');
  });

  await t.test('4 and 5: a clean stop shows Offline at once, with an unknown count', async () => {
    tracker.kill('SIGTERM');
    await new Promise((r) => tracker.once('exit', r));
    const s = await status();
    assert.equal(s.tracker, 'disconnected');
    assert.deepEqual(s.agents, { active: null, coverage: 'unavailable' });
  });

  await t.test('6: a restart does not double the tokens', async () => {
    tracker = startTracker();
    const s = await waitFor('connected again', status, (s) => s.tracker === 'connected');
    assert.equal(s.tokensToday.total, 523);
  });

  await t.test('4: a crashed tracker goes Offline when its lease ends', async () => {
    tracker.kill('SIGKILL');
    await new Promise((r) => tracker.once('exit', r));
    assert.equal((await status()).tracker, 'connected', 'the lease is still running');
    // Idle lease: 3 × 2 s = 6 s.
    const s = await waitFor('lease expiry', status, (s) => s.tracker === 'disconnected', 10_000);
    assert.equal(s.agents.active, null);
  });

  await t.test('12: an unreachable site does not stop the tracker', async () => {
    const config = JSON.parse(fs.readFileSync(path.join(stateDir, 'config.json')));
    config.endpoint = 'http://127.0.0.1:9/api/workstation/heartbeat';
    fs.writeFileSync(path.join(stateDir, 'config.json'), JSON.stringify(config));
    tracker = startTracker();
    await new Promise((r) => setTimeout(r, 3000));
    assert.equal(tracker.exitCode, null, 'still running');
    const log = fs.readFileSync(path.join(stateDir, 'logs', 'agent-heartbeat.log'), 'utf8');
    assert.match(log, /send: temporary failure/);
    tracker.kill('SIGTERM');
    await new Promise((r) => tracker.once('exit', r));
  });

  await t.test('11: logs and state hold no secret and no content; 12: the sources are untouched', () => {
    const log = fs.readFileSync(path.join(stateDir, 'logs', 'agent-heartbeat.log'), 'utf8');
    for (const forbidden of [secret, CANARY, CANARY_PATH, THREAD, 's-e2e', 'resp-1']) {
      assert.ok(!log.includes(forbidden), `the log contains ${forbidden.slice(0, 12)}`);
    }
    const state = fs.readFileSync(path.join(stateDir, 'state.json'), 'utf8');
    assert.ok(!state.includes(secret) && !state.includes(CANARY), 'private state holds no secret and no content');
    assert.equal((fs.statSync(path.join(stateDir, 'state.json')).mode & 0o777).toString(8), '600');
    assert.equal((fs.statSync(stateDir).mode & 0o777).toString(8), '700');
    // The test itself appended to two source files; everything else must be byte-identical.
    const after = fingerprint(base);
    const changed = Object.keys(after).filter((f) => f.startsWith(path.join(base, 'claude')) || f.startsWith(path.join(base, 'codex'))).filter((f) => before[f] !== after[f]);
    const expected = [sources.child, sources.rollout, path.join(sources.claude, 'sessions', `${sleeper.pid}.json`)].sort();
    assert.deepEqual(changed.sort(), expected);
  });
});
