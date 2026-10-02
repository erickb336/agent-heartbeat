// The mock receiver over real HTTP on 127.0.0.1, with a fake receiver clock.

import assert from 'node:assert/strict';
import { readdirSync, readFileSync } from 'node:fs';
import { request } from 'node:http';
import { describe, test } from 'node:test';
import Ajv2020 from 'ajv/dist/2020.js';
import { createReceiver, HEARTBEAT_PATH, MAX_BODY_BYTES, startMockReceiver, STATUS_PATH } from '../receiver.js';
import { dateIn } from '../validate.js';

const SECRET = 'test-only-secret-0123456789abcdef0123456789abcdef';
const ZONE = 'America/Los_Angeles';
const START = Date.parse('2026-10-02T17:04:16.000Z'); // 10:04 in Seattle
const ACTIVE_TTL_S = 90; // the tracker's lease while agents run
const IDLE_TTL_S = 300; // the tracker's lease while no agent runs
const samplesDir = new URL('../../contract/samples/', import.meta.url);

/**
 * The documented shape of GET /api/workstation/status (docs/CONTRACT.md). Strict at every level,
 * so any field outside the shape fails.
 */
const coverage = { enum: ['complete', 'partial', 'unavailable'] };
const count = { type: ['integer', 'null'], minimum: 0 };
const time = { type: 'string', pattern: '^\\d{4}-\\d{2}-\\d{2}T\\d{2}:\\d{2}:\\d{2}\\.\\d{3}Z$' };
const STATUS_SCHEMA = {
  type: 'object',
  additionalProperties: false,
  required: ['schemaVersion', 'status', 'tracker', 'agents', 'tokensToday', 'lastHeartbeatAt', 'asOf'],
  properties: {
    schemaVersion: { const: 1 },
    status: { enum: ['online', 'offline'] },
    tracker: { enum: ['connected', 'disconnected', 'never'] },
    agents: {
      type: 'object',
      additionalProperties: false,
      required: ['active', 'coverage'],
      properties: { active: count, coverage },
    },
    tokensToday: {
      anyOf: [
        { type: 'null' },
        {
          type: 'object',
          additionalProperties: false,
          required: ['usageDate', 'total', 'input', 'cachedInput', 'output', 'coverage'],
          properties: {
            usageDate: { type: 'string', pattern: '^\\d{4}-\\d{2}-\\d{2}$' },
            total: count,
            input: count,
            cachedInput: count,
            output: count,
            coverage,
          },
        },
      ],
    },
    lastHeartbeatAt: { anyOf: [{ type: 'null' }, time] },
    asOf: time,
  },
};
const checkStatusShape = new Ajv2020.default({ allErrors: true }).compile(STATUS_SCHEMA);

/** @param {unknown} body */
function assertStatusShape(body) {
  assert.ok(checkStatusShape(body), JSON.stringify(checkStatusShape.errors));
}

/**
 * Start a receiver on 127.0.0.1 with a fake clock. Each test gets its own state.
 * @param {import('node:test').TestContext} t
 * @param {Partial<import('../receiver.js').ReceiverOptions>} [options]
 */
async function setup(t, options = {}) {
  const clock = { now: START };
  const { url, close } = await startMockReceiver({ port: 0, secret: SECRET, now: () => clock.now, ...options });
  t.after(close);

  /** Advance the receiver clock. @param {number} ms */
  const advance = (ms) => {
    clock.now += ms;
  };

  /**
   * A heartbeat observed at the receiver's current time (or at an offset from it).
   * @param {{ active?: number | null, agentsCoverage?: string, total?: number, offsetMs?: number, tokensCoverage?: string, ttl?: number }} [o]
   */
  const heartbeat = (o = {}) => {
    const at = clock.now + (o.offsetMs ?? 0);
    const total = o.total ?? 128_400;
    const active = o.active === undefined ? 3 : o.active;
    return {
      schemaVersion: 1,
      type: 'heartbeat',
      observedAt: new Date(at).toISOString(),
      ttlSeconds: o.ttl ?? ACTIVE_TTL_S,
      usageDate: dateIn(at, ZONE),
      timeZone: ZONE,
      agents: { active, coverage: o.agentsCoverage ?? (active === null ? 'unavailable' : 'complete') },
      tokens: {
        input: total - Math.min(8_400, total),
        cachedInput: Math.min(1_000, total - Math.min(8_400, total)),
        output: Math.min(8_400, total),
        total,
        coverage: o.tokensCoverage ?? 'complete',
      },
    };
  };

  /** @param {string} usageDate @param {number} total @param {number} [offsetMs] */
  const dailyUsage = (usageDate, total, offsetMs = 0) => ({
    schemaVersion: 1,
    type: 'dailyUsage',
    observedAt: new Date(clock.now + offsetMs).toISOString(),
    usageDate,
    timeZone: ZONE,
    tokens: { input: total - 100, cachedInput: 0, output: 100, total, coverage: 'complete' },
  });

  /**
   * @param {unknown} body  An object is sent as JSON; a string is sent as is.
   * @param {{ secret?: string | null, contentType?: string, authorization?: string }} [o]
   */
  const post = async (body, o = {}) => {
    /** @type {Record<string, string>} */
    const headers = { 'content-type': o.contentType ?? 'application/json' };
    const secret = o.secret === undefined ? SECRET : o.secret;
    if (o.authorization) headers.authorization = o.authorization;
    else if (secret !== null) headers.authorization = `Bearer ${secret}`;
    const response = await fetch(url + HEARTBEAT_PATH, {
      method: 'POST',
      headers,
      body: typeof body === 'string' ? body : JSON.stringify(body),
    });
    const text = await response.text();
    return { status: response.status, headers: response.headers, body: text ? JSON.parse(text) : null };
  };

  /** @returns {Promise<any>} */
  const status = async () => {
    const response = await fetch(url + STATUS_PATH);
    assert.equal(response.status, 200);
    const body = await response.json();
    assertStatusShape(body);
    return body;
  };

  return { url, advance, heartbeat, dailyUsage, post, status };
}

describe('authentication', () => {
  test('a missing, wrong or non-bearer secret gets 401 and changes nothing', async (t) => {
    const r = await setup(t);
    for (const options of [{ secret: null }, { secret: 'wrong-secret' }, { authorization: `Basic ${SECRET}` }, { secret: '' }]) {
      const response = await r.post(r.heartbeat(), options);
      assert.equal(response.status, 401);
      assert.deepEqual(response.body, { error: 'unauthorized' });
    }
    assert.equal((await r.status()).tracker, 'never');
  });

  test('the right secret is accepted with 204 and no body', async (t) => {
    const r = await setup(t);
    const response = await r.post(r.heartbeat());
    assert.equal(response.status, 204);
    assert.equal(response.body, null);
  });

  test('the receiver needs a 64-digit SHA-256 hex, not the secret itself', () => {
    assert.throws(() => createReceiver({ secretSha256Hex: SECRET }), TypeError);
  });
});

describe('request checks', () => {
  test('a body that is not application/json gets 415', async (t) => {
    const r = await setup(t);
    assert.equal((await r.post(r.heartbeat(), { contentType: 'text/plain' })).status, 415);
    r.advance(1);
    assert.equal((await r.post(r.heartbeat(), { contentType: 'application/json; charset=utf-8' })).status, 204);
  });

  test(`a body of ${MAX_BODY_BYTES} bytes is accepted, and one byte more gets 413`, async (t) => {
    const r = await setup(t);
    const json = JSON.stringify(r.heartbeat());
    assert.equal((await r.post(json.padEnd(MAX_BODY_BYTES, ' '))).status, 204);
    r.advance(1);
    const response = await r.post(JSON.stringify(r.heartbeat()).padEnd(MAX_BODY_BYTES + 1, ' '));
    assert.equal(response.status, 413);
    assert.deepEqual(response.body, { error: 'too_large' });
  });

  test('a streamed body with no declared length gets 413 once it passes the limit', async (t) => {
    const r = await setup(t);
    const target = new URL(r.url + HEARTBEAT_PATH);
    const code = await new Promise((resolve, reject) => {
      const req = request({
        host: target.hostname,
        port: target.port,
        path: target.pathname,
        method: 'POST',
        headers: { authorization: `Bearer ${SECRET}`, 'content-type': 'application/json' }, // chunked
      });
      req.on('response', (res) => {
        res.resume();
        resolve(res.statusCode);
      });
      req.on('error', reject);
      for (let i = 0; i < 6; i++) req.write(' '.repeat(1024));
      req.end();
    });
    assert.equal(code, 413);
  });

  test('a body that is not JSON, or not a valid message, gets 400 with codes only', async (t) => {
    const r = await setup(t);
    assert.deepEqual((await r.post('{"schemaVersion":1,')).body, { error: 'invalid', details: ['/: not_json'] });

    const leaky = { ...r.heartbeat(), project: 'planted-project-name' };
    const response = await r.post(leaky);
    assert.equal(response.status, 400);
    assert.equal(response.body.error, 'invalid');
    assert.ok(response.body.details.includes('/: additionalProperties'));
    assert.ok(!JSON.stringify(response.body).includes('planted'));

    const sample = JSON.parse(readFileSync(new URL('invalid-total-not-sum.json', samplesDir), 'utf8'));
    assert.deepEqual((await r.post(sample)).body, { error: 'invalid', details: ['/tokens/total: not_input_plus_output'] });
    assert.equal((await r.status()).tracker, 'never');
  });

  test('a message in another time zone gets 400', async (t) => {
    const r = await setup(t);
    const message = { ...r.heartbeat(), timeZone: 'Europe/Berlin', usageDate: dateIn(START, 'Europe/Berlin') };
    const response = await r.post(message);
    assert.equal(response.status, 400);
    assert.deepEqual(response.body.details, ['/timeZone: not_receiver_zone']);
  });

  test('observedAt more than 120 s from the receiver clock gets 422', async (t) => {
    const r = await setup(t);
    for (const offsetMs of [-120_001, 120_001, -3_600_000]) {
      const response = await r.post(r.heartbeat({ offsetMs }));
      assert.equal(response.status, 422);
      assert.deepEqual(response.body, { error: 'clock_skew' });
    }
    assert.equal((await r.post(r.heartbeat({ offsetMs: -120_000 }))).status, 204);
  });

  test('only the three routes exist', async (t) => {
    const r = await setup(t);
    const cases = [
      ['GET', '/nope'],
      ['GET', HEARTBEAT_PATH],
      ['POST', STATUS_PATH],
      ['PUT', HEARTBEAT_PATH],
      ['GET', '/api/workstation'],
    ];
    for (const [method, path] of cases) {
      const response = await fetch(r.url + path, { method });
      assert.equal(response.status, 404, `${method} ${path}`);
    }
  });
});

describe('ordering', () => {
  test('a heartbeat that is not newer than the last accepted one gets 409 and changes nothing', async (t) => {
    const r = await setup(t);
    const first = r.heartbeat({ active: 3, total: 1_000 });
    assert.equal((await r.post(first)).status, 204);
    r.advance(5_000);
    assert.deepEqual((await r.post(first)).body, { error: 'stale' }); // same observedAt
    const older = r.heartbeat({ active: 7, total: 9_000, offsetMs: -10_000 });
    assert.equal((await r.post(older)).status, 409);

    const status = await r.status();
    assert.equal(status.agents.active, 3);
    assert.equal(status.tokensToday.total, 1_000);
    assert.equal(status.lastHeartbeatAt, new Date(START).toISOString());
  });

  test('a dailyUsage older than the stored total for that day gets 409', async (t) => {
    const r = await setup(t);
    const today = dateIn(START, ZONE);
    assert.equal((await r.post(r.dailyUsage(today, 5_000))).status, 204);
    assert.equal((await r.post(r.dailyUsage(today, 4_000, -1_000))).status, 409);
    assert.equal((await r.status()).tokensToday.total, 5_000);
  });
});

describe('daily totals', () => {
  test('retries and repeats cannot inflate usage: totals are snapshots, not deltas', async (t) => {
    const r = await setup(t);
    assert.equal((await r.post(r.heartbeat({ total: 1_000 }))).status, 204);
    for (let i = 0; i < 3; i++) {
      r.advance(2_000);
      assert.equal((await r.post(r.heartbeat({ total: 1_000 }))).status, 204); // a new snapshot, same total
    }
    const replay = r.heartbeat({ total: 1_000 });
    await r.post(replay);
    assert.equal((await r.post(replay)).status, 409); // an exact replay
    r.advance(2_000);
    const correction = r.dailyUsage(dateIn(START, ZONE), 1_000);
    await r.post(correction);
    assert.equal((await r.post(correction)).status, 409);
    assert.equal((await r.status()).tokensToday.total, 1_000);
  });

  test('a dailyUsage never changes freshness or the active count', async (t) => {
    const r = await setup(t);
    const today = dateIn(START, ZONE);
    assert.equal((await r.post(r.dailyUsage(today, 2_000))).status, 204);
    let status = await r.status();
    assert.equal(status.tracker, 'never');
    assert.deepEqual(status.agents, { active: null, coverage: 'unavailable' });
    assert.equal(status.tokensToday.total, 2_000);

    r.advance(1_000);
    await r.post(r.heartbeat({ active: 2, total: 3_000, ttl: 90 }));
    r.advance(80_000);
    assert.equal((await r.post(r.dailyUsage(today, 3_500))).status, 204);
    r.advance(10_000); // the 90 s lease of the heartbeat has ended
    status = await r.status();
    assert.equal(status.tracker, 'disconnected');
    assert.equal(status.lastHeartbeatAt, new Date(START + 1_000).toISOString());
    assert.equal(status.tokensToday.total, 3_500);
  });

  test("yesterday's total never shows as today's", async (t) => {
    // 23:59:50 in Seattle on 2026-10-01.
    const r = await setup(t);
    r.advance(Date.parse('2026-10-02T06:59:50.000Z') - START);
    assert.equal((await r.post(r.heartbeat({ total: 50_000 }))).status, 204);
    assert.equal((await r.status()).tokensToday.usageDate, '2026-10-01');

    r.advance(15_000); // 00:00:05 on 2026-10-02
    let status = await r.status();
    assert.equal(status.tracker, 'connected');
    assert.equal(status.tokensToday, null);

    // The tracker corrects yesterday after midnight. It must not appear as today's total.
    assert.equal((await r.post(r.dailyUsage('2026-10-01', 51_000))).status, 204);
    assert.equal((await r.status()).tokensToday, null);

    r.advance(1_000);
    assert.equal((await r.post(r.heartbeat({ total: 200 }))).status, 204);
    status = await r.status();
    assert.deepEqual(
      { usageDate: status.tokensToday.usageDate, total: status.tokensToday.total },
      { usageDate: '2026-10-02', total: 200 },
    );
  });
});

describe('rate limit', () => {
  test('more than 10 requests in 1 s get 429 with Retry-After: 1', async (t) => {
    const r = await setup(t);
    for (let i = 0; i < 10; i++) {
      const response = await r.post(r.heartbeat({ offsetMs: i }));
      assert.equal(response.status, 204);
    }
    const limited = await r.post(r.heartbeat({ offsetMs: 10 }));
    assert.equal(limited.status, 429);
    assert.equal(limited.headers.get('retry-after'), '1');
    r.advance(1_000);
    assert.equal((await r.post(r.heartbeat({ offsetMs: 11 }))).status, 204);
  });
});

describe('public status', () => {
  test('before any heartbeat: offline, tracker never, count unknown', async (t) => {
    const r = await setup(t);
    const status = await r.status();
    assert.deepEqual(status, {
      schemaVersion: 1,
      status: 'offline',
      tracker: 'never',
      agents: { active: null, coverage: 'unavailable' },
      tokensToday: null,
      lastHeartbeatAt: null,
      asOf: new Date(START).toISOString(),
    });
  });

  test('a fresh heartbeat with active agents is online', async (t) => {
    const r = await setup(t);
    await r.post(r.heartbeat({ active: 3, total: 128_400 }));
    r.advance(12_000);
    const status = await r.status();
    assert.equal(status.status, 'online');
    assert.equal(status.tracker, 'connected');
    assert.deepEqual(status.agents, { active: 3, coverage: 'complete' });
    assert.deepEqual(status.tokensToday, {
      usageDate: '2026-10-02',
      total: 128_400,
      input: 120_000,
      cachedInput: 1_000,
      output: 8_400,
      coverage: 'complete',
    });
  });

  test('check 3: zero active agents and a fresh heartbeat: offline, tracker connected, active 0', async (t) => {
    const r = await setup(t);
    await r.post(r.heartbeat({ active: 0 }));
    const status = await r.status();
    assert.equal(status.status, 'offline');
    assert.equal(status.tracker, 'connected');
    assert.deepEqual(status.agents, { active: 0, coverage: 'complete' });
  });

  test('check 4: when heartbeats stop, the status goes offline when the lease ends', async (t) => {
    const r = await setup(t);
    await r.post(r.heartbeat({ active: 2, ttl: ACTIVE_TTL_S }));
    r.advance(ACTIVE_TTL_S * 1000 - 1);
    let status = await r.status();
    assert.equal(status.status, 'online');
    assert.equal(status.tracker, 'connected');
    r.advance(1); // receivedAt + ttlSeconds is now, not in the future
    status = await r.status();
    assert.equal(status.status, 'offline');
    assert.equal(status.tracker, 'disconnected');
  });

  test('check 5: a disconnected tracker gives an unknown count, not 0', async (t) => {
    const r = await setup(t);
    await r.post(r.heartbeat({ active: 0, ttl: IDLE_TTL_S }));
    r.advance(IDLE_TTL_S * 1000 - 1);
    assert.deepEqual((await r.status()).agents, { active: 0, coverage: 'complete' });
    r.advance(1);
    const status = await r.status();
    assert.equal(status.tracker, 'disconnected');
    assert.deepEqual(status.agents, { active: null, coverage: 'unavailable' });
    assert.notEqual(status.tokensToday, null); // today's total stays, as of the last report
  });

  test('ttlSeconds 0 is a clean stop: offline and an unknown count at once', async (t) => {
    const r = await setup(t);
    await r.post(r.heartbeat({ active: 3 }));
    assert.equal((await r.status()).status, 'online');
    r.advance(1_000);
    assert.equal((await r.post(r.heartbeat({ active: 3, ttl: 0 }))).status, 204);
    const status = await r.status();
    assert.equal(status.status, 'offline');
    assert.equal(status.tracker, 'disconnected');
    assert.deepEqual(status.agents, { active: null, coverage: 'unavailable' });
    assert.equal(status.lastHeartbeatAt, new Date(START + 1_000).toISOString());
    assert.equal(status.tokensToday.total, 128_400);
  });

  test('each heartbeat replaces the lease: a shorter lease ends sooner', async (t) => {
    const r = await setup(t);
    await r.post(r.heartbeat({ active: 0, ttl: IDLE_TTL_S }));
    r.advance(10_000);
    await r.post(r.heartbeat({ active: 1, ttl: ACTIVE_TTL_S }));
    r.advance(ACTIVE_TTL_S * 1000 - 1);
    assert.equal((await r.status()).status, 'online');
    r.advance(1); // the idle lease would still run, but the newer lease has ended
    assert.equal((await r.status()).tracker, 'disconnected');
  });

  test('the lease starts at receivedAt on the receiver clock, not at observedAt', async (t) => {
    const r = await setup(t);
    await r.post(r.heartbeat({ offsetMs: -100_000, ttl: ACTIVE_TTL_S })); // the tracker clock is 100 s behind
    let status = await r.status();
    assert.equal(status.tracker, 'connected');
    assert.equal(status.lastHeartbeatAt, new Date(START).toISOString());
    r.advance(ACTIVE_TTL_S * 1000 - 1);
    status = await r.status();
    assert.equal(status.tracker, 'connected');
  });

  test('the receiver clamps ttlSeconds to maxTtlSeconds (default 300)', async (t) => {
    const r = await setup(t);
    await r.post(r.heartbeat({ ttl: 600 }));
    r.advance(300_000 - 1);
    assert.equal((await r.status()).tracker, 'connected');
    r.advance(1);
    assert.equal((await r.status()).tracker, 'disconnected');

    const strict = await setup(t, { maxTtlSeconds: 60 });
    await strict.post(strict.heartbeat({ ttl: ACTIVE_TTL_S }));
    strict.advance(60_000);
    assert.equal((await strict.status()).tracker, 'disconnected');

    assert.throws(() => createReceiver({ secretSha256Hex: '0'.repeat(64), maxTtlSeconds: -1 }), TypeError);
  });

  test('partial counts are online only with at least one known agent', async (t) => {
    const r = await setup(t);
    await r.post(r.heartbeat({ active: 2, agentsCoverage: 'partial', tokensCoverage: 'partial' }));
    let status = await r.status();
    assert.equal(status.status, 'online');
    assert.deepEqual(status.agents, { active: 2, coverage: 'partial' });
    assert.equal(status.tokensToday.coverage, 'partial');

    r.advance(1_000);
    await r.post(r.heartbeat({ active: 0, agentsCoverage: 'partial' }));
    status = await r.status();
    assert.equal(status.status, 'offline');
    assert.equal(status.tracker, 'connected');
  });

  test('a connected tracker that cannot see agents is offline with an unknown count', async (t) => {
    const r = await setup(t);
    const message = {
      ...r.heartbeat({ active: null }),
      tokens: { input: null, cachedInput: null, output: null, total: null, coverage: 'unavailable' },
    };
    assert.equal((await r.post(message)).status, 204);
    const status = await r.status();
    assert.equal(status.status, 'offline');
    assert.equal(status.tracker, 'connected');
    assert.deepEqual(status.agents, { active: null, coverage: 'unavailable' });
    assert.equal(status.tokensToday.total, null);
  });

  test('the status never has fields outside its documented shape', async (t) => {
    const r = await setup(t);
    // Plant extra data where it would leak if the receiver copied messages through.
    const leaky = r.heartbeat();
    assert.equal((await r.post({ ...leaky, project: 'x' })).status, 400);
    assert.equal((await r.post(leaky)).status, 204);
    const status = await r.status(); // status() checks the strict shape
    assert.deepEqual(Object.keys(status).sort(), [
      'agents', 'asOf', 'lastHeartbeatAt', 'schemaVersion', 'status', 'tokensToday', 'tracker',
    ]);
    const response = await fetch(r.url + STATUS_PATH);
    assert.equal(response.headers.get('content-type'), 'application/json; charset=utf-8');
    assert.equal(response.headers.get('cache-control'), 'no-store');
  });

  test('the status samples in contract/samples match the documented shape', () => {
    const names = readdirSync(samplesDir).filter((n) => n.startsWith('status-'));
    assert.ok(names.length >= 3);
    for (const name of names) assertStatusShape(JSON.parse(readFileSync(new URL(name, samplesDir), 'utf8')));
  });
});

describe('preview page', () => {
  test('GET / serves a self-contained page that talks only to its own origin', async (t) => {
    const r = await setup(t);
    const response = await fetch(r.url + '/?from=test');
    assert.equal(response.status, 200);
    assert.match(String(response.headers.get('content-type')), /^text\/html/);
    assert.match(String(response.headers.get('content-security-policy')), /default-src 'none'.*connect-src 'self'/);
    const html = await response.text();
    assert.ok(html.includes('Behind the keyboard.'));
    assert.ok(html.includes(STATUS_PATH));
    assert.doesNotMatch(html, /(src|href)\s*=\s*["']?(https?:)?\/\//i);
    assert.doesNotMatch(html, /@import|url\(/i);
  });
});

describe('startMockReceiver', () => {
  test('refuses any host other than the loopback interface', async () => {
    for (const host of ['0.0.0.0', '::', '192.168.1.20', 'example.com', '']) {
      await assert.rejects(startMockReceiver({ host, port: 0, secret: SECRET }), /binds only to/);
    }
  });

  test('needs a secret', async () => {
    await assert.rejects(startMockReceiver({ port: 0, secret: '' }), TypeError);
  });

  test('reports method, route and status only, never the secret or the body', async (t) => {
    /** @type {object[]} */
    const entries = [];
    const r = await setup(t, { onRequest: (entry) => entries.push(entry) });
    await r.post(r.heartbeat());
    await r.post({ planted: 'planted-body-value' });
    await fetch(r.url + '/private/path?token=planted-query');
    assert.deepEqual(entries, [
      { method: 'POST', path: HEARTBEAT_PATH, status: 204 },
      { method: 'POST', path: HEARTBEAT_PATH, status: 400 },
      { method: 'GET', path: '(other)', status: 404 },
    ]);
    const text = JSON.stringify(entries);
    assert.ok(!text.includes(SECRET) && !text.includes('planted'));
  });
});
