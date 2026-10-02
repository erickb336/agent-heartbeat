// The message validator: the schema, the samples and the rules JSON Schema cannot express.

import assert from 'node:assert/strict';
import { readdirSync, readFileSync } from 'node:fs';
import { describe, test } from 'node:test';
import { validateMessage } from '../validate.js';

const samplesDir = new URL('../../contract/samples/', import.meta.url);
/** @param {string} name @returns {any} */
const sample = (name) => JSON.parse(readFileSync(new URL(name, samplesDir), 'utf8'));
const sampleNames = readdirSync(samplesDir).filter((n) => n.endsWith('.json'));

/** The error each invalid sample must produce. Every invalid-*.json file needs an entry. */
const INVALID_SAMPLES = {
  'invalid-extra-top-level-field.json': '/: additionalProperties',
  'invalid-extra-nested-field.json': '/agents: additionalProperties',
  'invalid-total-not-sum.json': '/tokens/total: not_input_plus_output',
  'invalid-cached-exceeds-input.json': '/tokens/cachedInput: exceeds_input',
  'invalid-unavailable-with-count.json': '/agents/active: type',
  'invalid-schema-version.json': '/schemaVersion: unsupported',
  'invalid-daily-usage-with-agents.json': '/: additionalProperties',
  'invalid-heartbeat-other-date.json': '/usageDate: not_observed_date',
  'invalid-daily-usage-future-date.json': '/usageDate: future',
  'invalid-missing-ttl.json': '/: required ttlSeconds',
  'invalid-ttl-too-large.json': '/ttlSeconds: maximum',
};

/** @param {any} value */
function errorsOf(value) {
  const result = validateMessage(value);
  assert.equal(result.ok, false, 'expected the message to be invalid');
  return /** @type {{ ok: false, errors: string[] }} */ (result).errors;
}

/** @returns {any} */
const heartbeat = () => sample('heartbeat-complete.json');
/** @returns {any} */
const dailyUsage = () => sample('daily-usage-correction.json');

describe('samples', () => {
  const valid = sampleNames.filter((n) => n.startsWith('heartbeat-') || n.startsWith('daily-usage-'));
  const invalid = sampleNames.filter((n) => n.startsWith('invalid-'));

  test('there are valid and invalid samples of both message types', () => {
    assert.ok(valid.includes('heartbeat-complete.json'));
    assert.ok(valid.includes('heartbeat-partial.json'));
    assert.ok(valid.includes('heartbeat-unavailable-agents.json'));
    assert.ok(valid.includes('heartbeat-clean-stop.json'));
    assert.ok(valid.includes('daily-usage-correction.json'));
    assert.deepEqual(invalid.sort(), Object.keys(INVALID_SAMPLES).sort());
  });

  for (const name of valid) {
    test(`${name} is valid`, () => {
      const value = sample(name);
      const result = validateMessage(value);
      assert.deepEqual(result, { ok: true, message: value });
    });
  }

  for (const [name, code] of Object.entries(INVALID_SAMPLES)) {
    test(`${name} is rejected with "${code}"`, () => {
      assert.ok(errorsOf(sample(name)).includes(code));
    });
  }
});

describe('extra fields', () => {
  /** @type {Array<[string, () => any, (m: any) => object]>} */
  const places = [
    ['heartbeat root', heartbeat, (m) => m],
    ['heartbeat agents', heartbeat, (m) => m.agents],
    ['heartbeat tokens', heartbeat, (m) => m.tokens],
    ['dailyUsage root', dailyUsage, (m) => m],
    ['dailyUsage tokens', dailyUsage, (m) => m.tokens],
  ];
  for (const [label, make, target] of places) {
    test(`are rejected in the ${label}, and the error names neither the field nor its value`, () => {
      const message = make();
      Object.assign(target(message), { plantedFieldName: 'planted-secret-value' });
      const errors = errorsOf(message);
      assert.ok(errors.some((e) => e.endsWith(': additionalProperties')), errors.join());
      const text = JSON.stringify(errors);
      assert.ok(!text.includes('plantedFieldName') && !text.includes('planted-secret-value'));
    });
  }

  test('a dailyUsage message cannot carry an agent count', () => {
    assert.ok(errorsOf({ ...dailyUsage(), agents: { active: 1, coverage: 'complete' } }).includes('/: additionalProperties'));
  });
});

describe('errors never echo input values', () => {
  test('wrong types and patterns produce paths and codes only', () => {
    const planted = '/Users/someone/private-project';
    const message = heartbeat();
    message.timeZone = planted;
    message.observedAt = planted;
    message.agents.active = planted;
    const text = JSON.stringify(errorsOf(message));
    assert.ok(!text.includes('someone') && !text.includes('private-project'), text);
  });

  test('an unknown type value is not echoed', () => {
    assert.deepEqual(errorsOf({ ...heartbeat(), type: 'secret-type-name' }), ['/type: unknown']);
  });
});

describe('consistency rules', () => {
  test('total must equal input + output', () => {
    const message = heartbeat();
    message.tokens.total += 1;
    assert.deepEqual(errorsOf(message), ['/tokens/total: not_input_plus_output']);
  });

  test('cached input may equal input but not exceed it', () => {
    const equal = heartbeat();
    equal.tokens.cachedInput = equal.tokens.input;
    assert.equal(validateMessage(equal).ok, true);
    const over = heartbeat();
    over.tokens.cachedInput = over.tokens.input + 1;
    assert.deepEqual(errorsOf(over), ['/tokens/cachedInput: exceeds_input']);
  });

  test('the rules also hold for dailyUsage', () => {
    const message = dailyUsage();
    message.tokens.output += 5;
    assert.deepEqual(errorsOf(message), ['/tokens/total: not_input_plus_output']);
  });

  test('timestamps and dates must name real instants and days', () => {
    assert.deepEqual(errorsOf({ ...heartbeat(), observedAt: '2026-02-30T17:00:00Z', usageDate: '2026-03-02' }), ['/observedAt: invalid_time']);
    assert.deepEqual(errorsOf({ ...dailyUsage(), usageDate: '2026-02-30' }), ['/usageDate: invalid_date']);
    assert.deepEqual(errorsOf({ ...heartbeat(), timeZone: 'Mars/Olympus_Mons' }), ['/timeZone: unknown']);
  });

  test('a heartbeat date is the date of observedAt in its time zone', () => {
    // 2026-10-03T06:59:59Z is 23:59:59 on 2026-10-02 in Seattle; one second later it is 2026-10-03.
    const before = { ...heartbeat(), observedAt: '2026-10-03T06:59:59.999Z', usageDate: '2026-10-02' };
    assert.equal(validateMessage(before).ok, true);
    const after = { ...before, observedAt: '2026-10-03T07:00:00Z' };
    assert.deepEqual(errorsOf(after), ['/usageDate: not_observed_date']);
  });

  test('a dailyUsage may correct an earlier day or today, but not a later day', () => {
    assert.equal(validateMessage({ ...dailyUsage(), usageDate: '2026-09-15' }).ok, true);
    assert.equal(validateMessage({ ...dailyUsage(), usageDate: '2026-10-02' }).ok, true);
    assert.deepEqual(errorsOf({ ...dailyUsage(), usageDate: '2026-10-03' }), ['/usageDate: future']);
  });
});

describe('null and coverage rules', () => {
  test('unavailable coverage needs null numbers, and other coverage needs integers', () => {
    const unavailableTokens = { input: null, cachedInput: null, output: null, total: null, coverage: 'unavailable' };
    assert.equal(validateMessage({ ...heartbeat(), tokens: unavailableTokens }).ok, true);
    assert.ok(errorsOf({ ...heartbeat(), tokens: { ...unavailableTokens, coverage: 'partial' } }).length > 0);
    assert.ok(errorsOf({ ...heartbeat(), tokens: { ...unavailableTokens, total: 0 } }).length > 0);
    assert.ok(errorsOf({ ...heartbeat(), agents: { active: null, coverage: 'complete' } }).includes('/agents/active: type'));
  });

  test('numbers are bounded non-negative integers', () => {
    for (const active of [-1, 1.5, 1001]) {
      assert.equal(validateMessage({ ...heartbeat(), agents: { active, coverage: 'complete' } }).ok, false);
    }
    const huge = heartbeat();
    huge.tokens = { input: 1e12, cachedInput: 0, output: 1, total: 1e12 + 1, coverage: 'complete' };
    assert.ok(errorsOf(huge).includes('/tokens/total: maximum'));
  });
});

describe('lease', () => {
  test('ttlSeconds is a whole number of seconds from 0 to 600', () => {
    for (const ttlSeconds of [0, 30, 90, 300, 600]) assert.equal(validateMessage({ ...heartbeat(), ttlSeconds }).ok, true);
    for (const ttlSeconds of [-1, 601, 1.5, '90', null]) assert.equal(validateMessage({ ...heartbeat(), ttlSeconds }).ok, false);
  });

  test('a dailyUsage has no lease', () => {
    assert.ok(errorsOf({ ...dailyUsage(), ttlSeconds: 90 }).includes('/: additionalProperties'));
  });
});

describe('message shape', () => {
  test('anything that is not an object is rejected', () => {
    for (const value of [null, [], 'heartbeat', 1, true, undefined]) {
      assert.deepEqual(errorsOf(value), ['/: type']);
    }
  });

  test('a missing field is named from the schema', () => {
    const { agents, ...rest } = heartbeat();
    assert.deepEqual(errorsOf(rest), ['/: required agents']);
    const { type, ...untyped } = heartbeat();
    assert.deepEqual(errorsOf(untyped), ['/: required type']);
  });

  test('an unknown schema version is rejected before anything else', () => {
    assert.deepEqual(errorsOf({ schemaVersion: 2, anything: 'goes' }), ['/schemaVersion: unsupported']);
    assert.deepEqual(errorsOf({ ...heartbeat(), schemaVersion: '1' }), ['/schemaVersion: unsupported']);
  });

  test('a valid message comes back as the same object', () => {
    const message = heartbeat();
    const result = validateMessage(message);
    assert.ok(result.ok && result.message === message);
  });
});
