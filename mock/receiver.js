// A local stand-in for the site's receiver (docs/CONTRACT.md). It accepts heartbeats, keeps
// aggregate state in memory only, serves the sanitized public status, and serves a preview of
// the "Quiet terminal" panel at /. It binds to the loopback interface only.
//
// Freshness is a lease: each accepted heartbeat keeps the tracker connected for its ttlSeconds,
// counted from receivedAt on the receiver's clock. ttlSeconds 0 ends the lease at once.
//
// It never logs request bodies or the Authorization header. The optional onRequest hook gets the
// method, the route and the status code, nothing else.

import { createHash, timingSafeEqual } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { createServer } from 'node:http';
import { dateIn, validateMessage } from './validate.js';

/** @typedef {import('./validate.js').Coverage} Coverage */
/** @typedef {import('./validate.js').Message} Message */
/** @typedef {import('./validate.js').TokensField} TokensField */
/** @typedef {import('node:http').IncomingMessage} IncomingMessage */
/** @typedef {import('node:http').ServerResponse} ServerResponse */

export const HEARTBEAT_PATH = '/api/workstation/heartbeat';
export const STATUS_PATH = '/api/workstation/status';
export const MAX_BODY_BYTES = 4096;
export const DEFAULT_TIME_ZONE = 'America/Los_Angeles';
export const STATUS_CACHE_CONTROL = 'public, max-age=2';
const RATE_LIMIT = 10; // accepted requests per window; the next one gets 429
const RATE_WINDOW_MS = 1000;
const LOOPBACK_HOSTS = new Set(['127.0.0.1', '::1', 'localhost']);

const PREVIEW_HTML = readFileSync(new URL('./preview.html', import.meta.url), 'utf8');
const PREVIEW_HEADERS = {
  'content-type': 'text/html; charset=utf-8',
  'cache-control': 'no-store',
  // The preview may talk to this origin only: no fonts, images or scripts from elsewhere.
  'content-security-policy':
    "default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
  'referrer-policy': 'no-referrer',
  'x-content-type-options': 'nosniff',
};

/**
 * The body of GET /api/workstation/status. See "Public status endpoint" in docs/CONTRACT.md.
 * @typedef {object} PublicStatus
 * @property {1} schemaVersion
 * @property {'online' | 'offline'} status        online = connected and agents.active >= 1.
 * @property {'connected' | 'disconnected' | 'never'} tracker  connected = the last lease has not ended.
 * @property {{ active: number | null, coverage: Coverage }} agents  Unknown (null) unless connected.
 * @property {({ usageDate: string } & TokensField) | null} tokensToday  null when today has no data.
 * @property {string | null} lastHeartbeatAt      Receiver clock time of the last accepted heartbeat.
 * @property {string} asOf                        Receiver clock time of this response.
 */

/**
 * @typedef {object} ReceiverOptions
 * @property {string} secretSha256Hex  SHA-256 of the bearer secret, 64 hex digits. The secret itself is not kept.
 * @property {() => number} [now]      Receiver clock, epoch ms. Tests inject a fake clock.
 * @property {number} [maxTtlSeconds]  The longest lease the receiver grants; a larger ttlSeconds is cut to this.
 * @property {number} [skewMs]         The largest accepted difference between observedAt and the receiver clock.
 * @property {string} [timeZone]       The zone that defines "today". Messages must use the same zone.
 * @property {(entry: { method: string, path: string, status: number }) => void} [onRequest]
 */

/**
 * @typedef {object} DailyEntry
 * @property {number | null} input
 * @property {number | null} cachedInput
 * @property {number | null} output
 * @property {number | null} total
 * @property {Coverage} coverage
 * @property {number} observedMs  Tracker clock; orders updates for this date.
 * @property {number} receivedMs  Receiver clock.
 */

/**
 * @typedef {object} LiveEntry
 * @property {number | null} active
 * @property {Coverage} coverage
 * @property {number} observedMs    Tracker clock; orders heartbeats.
 * @property {number} receivedMs    Receiver clock.
 * @property {number} leaseUntilMs  Receiver clock: receivedMs + the clamped ttlSeconds. Connected while now < this.
 */

/**
 * Create the receiver's request handler and in-memory state.
 * @param {ReceiverOptions} options
 * @returns {{ handle: (req: IncomingMessage, res: ServerResponse) => Promise<void>, snapshot: () => PublicStatus }}
 *   snapshot() returns what GET /api/workstation/status would return now.
 */
export function createReceiver({
  secretSha256Hex,
  now = Date.now,
  maxTtlSeconds = 300,
  skewMs = 120_000,
  timeZone = DEFAULT_TIME_ZONE,
  onRequest,
}) {
  if (typeof secretSha256Hex !== 'string' || !/^[0-9a-f]{64}$/i.test(secretSha256Hex)) {
    throw new TypeError('secretSha256Hex must be 64 hex digits');
  }
  if (!Number.isInteger(maxTtlSeconds) || maxTtlSeconds < 0) throw new TypeError('maxTtlSeconds must be a non-negative integer');
  const expectedHash = Buffer.from(secretSha256Hex, 'hex');

  /** @type {LiveEntry | null} */
  let live = null;
  /** @type {Map<string, DailyEntry>} keyed by usageDate */
  const daily = new Map();
  /** @type {number[]} receiver times of recent authenticated requests */
  const recent = [];

  /** @param {string | undefined} header */
  function authorized(header) {
    const match = /^Bearer ([^\s]+)$/i.exec(header ?? '');
    if (!match) return false;
    return timingSafeEqual(createHash('sha256').update(match[1], 'utf8').digest(), expectedHash);
  }

  function rateLimited() {
    const t = now();
    while (recent.length > 0 && recent[0] <= t - RATE_WINDOW_MS) recent.shift();
    if (recent.length >= RATE_LIMIT) return true;
    recent.push(t);
    return false;
  }

  /**
   * Upsert one day's totals when the observation is newer than the stored one.
   * Totals are daily snapshots, not deltas, so a retried message cannot add to them.
   * @param {string} usageDate @param {TokensField} tokens @param {number} observedMs @param {number} receivedMs
   */
  function upsertDaily(usageDate, tokens, observedMs, receivedMs) {
    const stored = daily.get(usageDate);
    if (stored && observedMs <= stored.observedMs) return false;
    const { input, cachedInput, output, total, coverage } = tokens;
    daily.set(usageDate, { input, cachedInput, output, total, coverage, observedMs, receivedMs });
    return true;
  }

  /** @param {Message} m @param {number} observedMs @param {number} receivedMs @returns {boolean} false when stale */
  function apply(m, observedMs, receivedMs) {
    if (m.type === 'dailyUsage') return upsertDaily(m.usageDate, m.tokens, observedMs, receivedMs);
    if (live && observedMs <= live.observedMs) return false;
    const leaseUntilMs = receivedMs + Math.min(m.ttlSeconds, maxTtlSeconds) * 1000;
    live = { active: m.agents.active, coverage: m.agents.coverage, observedMs, receivedMs, leaseUntilMs };
    upsertDaily(m.usageDate, m.tokens, observedMs, receivedMs);
    return true;
  }

  /** @returns {PublicStatus} */
  function snapshot() {
    const t = now();
    const connected = live !== null && live.leaseUntilMs > t;
    const agents = live !== null && connected
      ? { active: live.active, coverage: live.coverage }
      : { active: null, coverage: /** @type {Coverage} */ ('unavailable') };
    const today = dateIn(t, timeZone);
    const day = daily.get(today);
    return {
      schemaVersion: 1,
      status: connected && agents.active !== null && agents.active >= 1 ? 'online' : 'offline',
      tracker: live === null ? 'never' : connected ? 'connected' : 'disconnected',
      agents,
      tokensToday: day
        ? { usageDate: today, total: day.total, input: day.input, cachedInput: day.cachedInput, output: day.output, coverage: day.coverage }
        : null,
      lastHeartbeatAt: live === null ? null : new Date(live.receivedMs).toISOString(),
      asOf: new Date(t).toISOString(),
    };
  }

  /** @param {IncomingMessage} req @param {ServerResponse} res */
  async function ingest(req, res) {
    if (!authorized(req.headers.authorization)) {
      return sendJson(res, 401, { error: 'unauthorized' }, { 'www-authenticate': 'Bearer' });
    }
    if (rateLimited()) return sendJson(res, 429, { error: 'rate_limited' }, { 'retry-after': '1' });
    if (!isJsonMediaType(req.headers['content-type'])) return sendJson(res, 415, { error: 'unsupported_media_type' });

    const body = await readBody(req, MAX_BODY_BYTES);
    if (body === null) return sendJson(res, 413, { error: 'too_large' }, { connection: 'close' });

    let parsed;
    try {
      parsed = JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(body));
    } catch {
      return sendJson(res, 400, { error: 'invalid', details: ['/: not_json'] });
    }
    const result = validateMessage(parsed);
    if (!result.ok) return sendJson(res, 400, { error: 'invalid', details: result.errors });
    const message = result.message;
    // Daily totals are keyed by date in one zone; a date from another zone would mean another day.
    if (message.timeZone !== timeZone) return sendJson(res, 400, { error: 'invalid', details: ['/timeZone: not_receiver_zone'] });

    const receivedMs = now();
    const observedMs = Date.parse(message.observedAt);
    if (Math.abs(observedMs - receivedMs) > skewMs) return sendJson(res, 422, { error: 'clock_skew' });
    if (!apply(message, observedMs, receivedMs)) return sendJson(res, 409, { error: 'stale' });
    res.writeHead(204, { 'cache-control': 'no-store' }).end();
  }

  /** @param {IncomingMessage} req @param {ServerResponse} res */
  async function handle(req, res) {
    const path = pathOf(req.url);
    try {
      if (req.method === 'POST' && path === HEARTBEAT_PATH) await ingest(req, res);
      // Shared caches may keep the status for 2 s, so many visitors cost one read per 2 s (docs/CONTRACT.md).
      else if (req.method === 'GET' && path === STATUS_PATH) sendJson(res, 200, snapshot(), { 'cache-control': STATUS_CACHE_CONTROL });
      else if (req.method === 'GET' && path === '/') res.writeHead(200, PREVIEW_HEADERS).end(PREVIEW_HTML);
      else sendJson(res, 404, { error: 'not_found' });
    } catch {
      // A client that disconnects mid-body lands here. Report nothing about the cause.
      if (!res.headersSent) sendJson(res, 500, { error: 'internal' });
      else res.destroy();
    }
    onRequest?.({
      method: String(req.method).slice(0, 10),
      path: path === HEARTBEAT_PATH || path === STATUS_PATH || path === '/' ? path : '(other)',
      status: res.statusCode,
    });
  }

  return { handle, snapshot };
}

/**
 * Start the mock receiver on a loopback address. It refuses any other host.
 * @param {{ secret: string, port?: number, host?: string } & Omit<ReceiverOptions, 'secretSha256Hex'>} options
 * @returns {Promise<{ url: string, close: () => Promise<void> }>}
 */
export async function startMockReceiver({ port = 8787, host = '127.0.0.1', secret, ...options }) {
  if (!LOOPBACK_HOSTS.has(host)) throw new Error('The mock receiver binds only to 127.0.0.1, ::1 or localhost.');
  if (typeof secret !== 'string' || secret.length === 0) throw new TypeError('secret must be a non-empty string');

  const receiver = createReceiver({ ...options, secretSha256Hex: sha256Hex(secret) });
  const server = createServer(receiver.handle);
  server.requestTimeout = 10_000;
  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(port, host, () => {
      server.off('error', reject);
      resolve(undefined);
    });
  });
  const address = /** @type {import('node:net').AddressInfo} */ (server.address());
  const hostPart = address.family === 'IPv6' ? `[${address.address}]` : address.address;
  return {
    url: `http://${hostPart}:${address.port}`,
    close: () =>
      new Promise((resolve) => {
        server.close(() => resolve());
        server.closeAllConnections();
      }),
  };
}

/** @param {string} secret */
export function sha256Hex(secret) {
  return createHash('sha256').update(secret, 'utf8').digest('hex');
}

/**
 * Read the body, or return null as soon as it is larger than limit. The caller then answers 413
 * with "Connection: close", so the server stops reading this request.
 * @param {IncomingMessage} req @param {number} limit @returns {Promise<Buffer | null>}
 */
function readBody(req, limit) {
  if (Number(req.headers['content-length']) > limit) return Promise.resolve(null);
  return new Promise((resolve, reject) => {
    /** @type {Buffer[]} */
    const chunks = [];
    let size = 0;
    let settled = false;
    /** @param {Buffer} chunk */
    const onData = (chunk) => {
      size += chunk.length;
      if (size > limit) {
        settled = true;
        req.off('data', onData);
        resolve(null);
      } else {
        chunks.push(chunk);
      }
    };
    req.on('data', onData);
    req.on('end', () => {
      if (!settled) resolve(Buffer.concat(chunks));
    });
    req.on('error', reject);
    req.on('close', () => {
      if (!settled && !req.complete) reject(new Error('client closed the request'));
    });
  });
}

/** @param {string | undefined} header */
function isJsonMediaType(header) {
  return (header ?? '').split(';')[0].trim().toLowerCase() === 'application/json';
}

/** @param {string | undefined} url */
function pathOf(url) {
  try {
    return new URL(url ?? '/', 'http://localhost').pathname;
  } catch {
    return '';
  }
}

/** @param {ServerResponse} res @param {number} status @param {unknown} body @param {Record<string, string>} [headers] */
function sendJson(res, status, body, headers = {}) {
  res
    .writeHead(status, {
      'content-type': 'application/json; charset=utf-8',
      'cache-control': 'no-store',
      'x-content-type-options': 'nosniff',
      ...headers,
    })
    .end(JSON.stringify(body));
}
