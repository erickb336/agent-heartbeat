// Validates one heartbeat message against contract/heartbeat.v1.schema.json, then applies the
// consistency rules that JSON Schema cannot express. The mock receiver runs it on every message
// it gets, and check-payload.js runs it on the tracker's dry-run output. See docs/CONTRACT.md.
//
// Error strings are "<JSON pointer>: <code>". They name schema paths and rule codes only. They
// never echo a value or an unknown property name from the input.

import { readFileSync } from 'node:fs';
import Ajv2020 from 'ajv/dist/2020.js';

/**
 * complete: every configured source reported. partial: a known subtotal (a lower bound).
 * unavailable: no source reported, so the numbers are null.
 * @typedef {'complete' | 'partial' | 'unavailable'} Coverage
 */

/**
 * @typedef {object} AgentsField
 * @property {number | null} active  null exactly when coverage is 'unavailable'.
 * @property {Coverage} coverage
 */

/**
 * @typedef {object} TokensField
 * @property {number | null} input        Processed input, cached input included.
 * @property {number | null} cachedInput  The part of input read from the prompt cache.
 * @property {number | null} output       Output, reasoning included.
 * @property {number | null} total        input + output.
 * @property {Coverage} coverage           All four numbers are null exactly when this is 'unavailable'.
 */

/**
 * @typedef {object} HeartbeatMessage
 * @property {1} schemaVersion
 * @property {'heartbeat'} type
 * @property {string} observedAt  UTC ISO-8601, tracker clock.
 * @property {number} ttlSeconds  The lease: connected until receivedAt + ttlSeconds. 0 = clean stop.
 * @property {string} usageDate   YYYY-MM-DD in timeZone; the date of observedAt.
 * @property {string} timeZone    IANA zone name.
 * @property {AgentsField} agents
 * @property {TokensField} tokens
 */

/**
 * @typedef {object} DailyUsageMessage
 * @property {1} schemaVersion
 * @property {'dailyUsage'} type
 * @property {string} observedAt
 * @property {string} usageDate   On or before the date of observedAt in timeZone.
 * @property {string} timeZone
 * @property {TokensField} tokens
 */

/** @typedef {HeartbeatMessage | DailyUsageMessage} Message */

/** @typedef {{ ok: true, message: Message } | { ok: false, errors: string[] }} ValidationResult */

export const SCHEMA_VERSION = 1;
const MAX_ERRORS = 20;

const schema = JSON.parse(readFileSync(new URL('../contract/heartbeat.v1.schema.json', import.meta.url), 'utf8'));

// ajv is CommonJS; under nodenext the default import is module.exports, whose .default is the class.
const ajv = new Ajv2020.default({ allErrors: true, strict: true });
const validateRoot = ajv.compile(schema);
// The per-type definitions give focused errors. The root schema stays the authority.
/** @type {Record<string, import('ajv').ValidateFunction>} */
const validateByType = {
  heartbeat: /** @type {import('ajv').ValidateFunction} */ (ajv.getSchema(`${schema.$id}#/$defs/heartbeat`)),
  dailyUsage: /** @type {import('ajv').ValidateFunction} */ (ajv.getSchema(`${schema.$id}#/$defs/dailyUsage`)),
};

/**
 * Validate one message. Returns the message itself (not a copy) when it is valid.
 * @param {unknown} value
 * @returns {ValidationResult}
 */
export function validateMessage(value) {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return fail(['/: type']);
  const record = /** @type {Record<string, unknown>} */ (value);
  if ('schemaVersion' in record && record.schemaVersion !== SCHEMA_VERSION) return fail(['/schemaVersion: unsupported']);
  const type = record.type;
  if (type !== 'heartbeat' && type !== 'dailyUsage') return fail([type === undefined ? '/: required type' : '/type: unknown']);

  if (!validateRoot(value)) {
    const branch = validateByType[type];
    branch(value);
    return fail(schemaErrors(branch.errors ?? validateRoot.errors ?? []));
  }

  const message = /** @type {Message} */ (value);
  const errors = consistencyErrors(message);
  return errors.length ? fail(errors) : { ok: true, message };
}

/**
 * The rules JSON Schema cannot express. Runs only on schema-valid messages.
 * @param {Message} m
 * @returns {string[]}
 */
function consistencyErrors(m) {
  /** @type {string[]} */
  const errors = [];
  const t = m.tokens;
  if (t.total !== null && t.input !== null && t.output !== null && t.total !== t.input + t.output) {
    errors.push('/tokens/total: not_input_plus_output');
  }
  if (t.cachedInput !== null && t.input !== null && t.cachedInput > t.input) {
    errors.push('/tokens/cachedInput: exceeds_input');
  }

  const observedMs = strictTime(m.observedAt);
  if (observedMs === null) errors.push('/observedAt: invalid_time');
  if (!isCalendarDate(m.usageDate)) errors.push('/usageDate: invalid_date');
  if (!isKnownTimeZone(m.timeZone)) errors.push('/timeZone: unknown');
  if (errors.length) return errors;

  // A heartbeat carries today's tokens, so its date is the date of its observation. A dailyUsage
  // corrects today or an earlier day, never a later one.
  const observedDate = dateIn(/** @type {number} */ (observedMs), m.timeZone);
  if (m.type === 'heartbeat' && m.usageDate !== observedDate) errors.push('/usageDate: not_observed_date');
  if (m.type === 'dailyUsage' && m.usageDate > observedDate) errors.push('/usageDate: future');
  return errors;
}

/**
 * Epoch ms of a schema-shaped timestamp, or null when it names no real instant
 * (for example 2026-02-30, which Date.parse silently moves to March).
 * @param {string} iso
 */
function strictTime(iso) {
  const t = Date.parse(iso);
  if (!Number.isFinite(t)) return null;
  const [whole, fraction = ''] = iso.slice(0, -1).split('.');
  return new Date(t).toISOString() === `${whole}.${fraction.padEnd(3, '0')}Z` ? t : null;
}

/** @param {string} date YYYY-MM-DD */
function isCalendarDate(date) {
  const t = Date.parse(`${date}T00:00:00Z`);
  return Number.isFinite(t) && new Date(t).toISOString().slice(0, 10) === date;
}

/** @type {Map<string, Intl.DateTimeFormat>} */
const dateFormatters = new Map();

/**
 * The calendar date (YYYY-MM-DD) of an instant in an IANA time zone. The Intl time-zone
 * database handles daylight-saving changes.
 * @param {number} epochMs @param {string} timeZone
 */
export function dateIn(epochMs, timeZone) {
  let f = dateFormatters.get(timeZone);
  if (!f) {
    f = new Intl.DateTimeFormat('en-CA', { timeZone, year: 'numeric', month: '2-digit', day: '2-digit' });
    // Zone names come from messages; a small cap keeps odd spellings from growing the cache.
    if (dateFormatters.size < 64) dateFormatters.set(timeZone, f);
  }
  return f.format(epochMs);
}

/** @param {string} timeZone */
function isKnownTimeZone(timeZone) {
  try {
    new Intl.DateTimeFormat('en-US', { timeZone });
    return true;
  } catch {
    return false;
  }
}

/**
 * Short, value-free codes from ajv errors. "if" errors repeat their "then"/"else" child, so they drop.
 * @param {import('ajv').ErrorObject[]} ajvErrors
 */
function schemaErrors(ajvErrors) {
  const codes = new Set();
  for (const e of ajvErrors) {
    if (e.keyword === 'if') continue;
    const path = e.instancePath || '/';
    // missingProperty comes from the schema's own "required" list, so it is safe to name.
    codes.add(e.keyword === 'required' ? `${path}: required ${e.params.missingProperty}` : `${path}: ${e.keyword}`);
  }
  return codes.size ? [...codes] : ['/: invalid'];
}

/** @param {string[]} errors @returns {ValidationResult} */
function fail(errors) {
  return { ok: false, errors: errors.slice(0, MAX_ERRORS) };
}
