// The two command-line entries: check-payload.js and server.js.

import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { describe, test } from 'node:test';
import { fileURLToPath } from 'node:url';
import { dateIn } from '../validate.js';

const mockDir = fileURLToPath(new URL('..', import.meta.url));
const samplesDir = new URL('../../contract/samples/', import.meta.url);
/** @param {string} name */
const sampleText = (name) => readFileSync(new URL(name, samplesDir), 'utf8');

/** @param {string} input */
function checkPayload(input) {
  return spawnSync(process.execPath, ['check-payload.js'], { cwd: mockDir, input, encoding: 'utf8' });
}

describe('check-payload.js', () => {
  test('exits 0 and prints nothing for a valid message', () => {
    for (const name of ['heartbeat-complete.json', 'daily-usage-correction.json']) {
      const run = checkPayload(sampleText(name));
      assert.equal(run.status, 0, run.stderr);
      assert.equal(run.stdout + run.stderr, '');
    }
  });

  test('exits 1 with one error code per line for an invalid message', () => {
    const run = checkPayload(sampleText('invalid-cached-exceeds-input.json'));
    assert.equal(run.status, 1);
    assert.equal(run.stderr, '/tokens/cachedInput: exceeds_input\n');
  });

  test('exits 1 for input that is not JSON', () => {
    const run = checkPayload('not json');
    assert.equal(run.status, 1);
    assert.equal(run.stderr, '/: not_json\n');
  });
});

/**
 * Start server.js and wait until it prints its address.
 * @param {import('node:test').TestContext} t @param {string[]} args @param {Record<string, string>} [env]
 */
async function startServer(t, args, env = {}) {
  const child = spawn(process.execPath, ['server.js', ...args], {
    cwd: mockDir,
    env: { ...process.env, MOCK_RECEIVER_SECRET: '', ...env },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  t.after(() => child.kill('SIGKILL'));
  let stdout = '';
  child.stdout.setEncoding('utf8');
  await new Promise((resolve, reject) => {
    child.stdout.on('data', (text) => {
      stdout += text;
      if (stdout.includes('Ctrl-C')) resolve(undefined);
    });
    child.once('exit', (code) => reject(new Error(`server.js exited with ${code}`)));
  });
  const url = /Mock receiver on (http:\/\/\S+)/.exec(stdout)?.[1];
  assert.ok(url, stdout);
  return { child, url, stdout };
}

/** @param {string} url @param {string} secret */
async function postHeartbeat(url, secret) {
  const now = Date.now();
  const message = JSON.parse(sampleText('heartbeat-complete.json'));
  message.observedAt = new Date(now).toISOString();
  message.usageDate = dateIn(now, 'America/Los_Angeles');
  const response = await fetch(`${url}/api/workstation/heartbeat`, {
    method: 'POST',
    headers: { authorization: `Bearer ${secret}`, 'content-type': 'application/json' },
    body: JSON.stringify(message),
  });
  return response.status;
}

describe('server.js', () => {
  test('without MOCK_RECEIVER_SECRET it prints a throwaway secret that works', async (t) => {
    const { child, url, stdout } = await startServer(t, ['--port', '0']);
    assert.match(url, /^http:\/\/127\.0\.0\.1:\d+$/);
    const secret = /Throwaway secret for local tests only: (\S+)/.exec(stdout)?.[1];
    assert.ok(secret && secret.length >= 43);
    assert.equal(await postHeartbeat(url, secret), 204);
    const status = await (await fetch(`${url}/api/workstation/status`)).json();
    assert.equal(status.status, 'online');

    const exited = new Promise((resolve) => child.once('exit', resolve));
    child.kill('SIGTERM');
    assert.equal(await exited, 0);
  });

  test('with MOCK_RECEIVER_SECRET it uses that secret and does not print it', async (t) => {
    const secret = 'env-provided-test-secret-0123456789abcdef';
    const { url, stdout } = await startServer(t, ['--port', '0'], { MOCK_RECEIVER_SECRET: secret });
    assert.ok(!stdout.includes(secret));
    assert.equal(await postHeartbeat(url, secret), 204);
    assert.equal(await postHeartbeat(url, 'another-secret'), 401);
  });

  test('--max-ttl-seconds caps every lease', async (t) => {
    const secret = 'env-provided-test-secret-0123456789abcdef';
    const { url } = await startServer(t, ['--port', '0', '--max-ttl-seconds', '0'], { MOCK_RECEIVER_SECRET: secret });
    assert.equal(await postHeartbeat(url, secret), 204);
    const status = await (await fetch(`${url}/api/workstation/status`)).json();
    assert.equal(status.tracker, 'disconnected');
  });

  test('refuses a host outside the loopback interface', () => {
    const run = spawnSync(process.execPath, ['server.js', '--port', '0', '--host', '0.0.0.0'], { cwd: mockDir, encoding: 'utf8' });
    assert.equal(run.status, 1);
    assert.match(run.stderr, /binds only to 127\.0\.0\.1/);
  });
});
