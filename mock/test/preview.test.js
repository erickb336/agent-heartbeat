// The panel preview's refresh schedule (docs/CONTRACT.md, "Refresh"): check every 5 s while the tab
// is visible, never while it is hidden, and at once when it becomes visible again. The page's own
// script runs here with a fake clock, a fake document and a fake fetch.

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import vm from 'node:vm';

const html = readFileSync(new URL('../preview.html', import.meta.url), 'utf8');
const script = /<script>([\s\S]*?)<\/script>/.exec(html)?.[1] ?? '';
const status = JSON.parse(readFileSync(new URL('../../contract/samples/status-online.json', import.meta.url), 'utf8'));

/** Lets pending promise callbacks run; setImmediate is not faked. */
const settle = () => new Promise((resolve) => setImmediate(resolve));

test('the panel checks every 5 s while visible, never while hidden, and at once on return', async (t) => {
  t.mock.timers.enable({ apis: ['setTimeout', 'setInterval'] });
  /** @type {Record<string, () => void>} */
  const listeners = {};
  /** @type {Map<string, any>} */
  const nodes = new Map();
  const node = (/** @type {string} */ id) => {
    if (!nodes.has(id)) nodes.set(id, { textContent: '', dataset: {}, classList: { contains: () => false }, setAttribute() {}, removeAttribute() {} });
    return nodes.get(id);
  };
  const document = { hidden: false, getElementById: node, addEventListener: (/** @type {string} */ type, /** @type {() => void} */ fn) => (listeners[type] = fn) };
  let checks = 0;
  const fetch = async () => {
    checks += 1;
    return { ok: true, json: async () => status };
  };
  vm.runInNewContext(script, {
    document, fetch, setTimeout, clearTimeout, setInterval, AbortController, performance: { now: () => 0 }, Date, Math, String,
  });

  await settle();
  assert.equal(checks, 1, 'one check on load');
  assert.equal(node('pill').textContent, '● ONLINE');
  t.mock.timers.tick(4_999);
  await settle();
  assert.equal(checks, 1, 'not before 5 s');
  t.mock.timers.tick(1);
  await settle();
  assert.equal(checks, 2, 'again at 5 s');

  document.hidden = true;
  listeners.visibilitychange();
  t.mock.timers.tick(60_000);
  await settle();
  assert.equal(checks, 2, 'no checks while hidden');

  document.hidden = false;
  listeners.visibilitychange();
  await settle();
  assert.equal(checks, 3, 'a check at once when visible again');
  t.mock.timers.tick(5_000);
  await settle();
  assert.equal(checks, 4, 'then every 5 s again');
});

test('a check that is still waiting when the tab hides schedules no next check', async (t) => {
  t.mock.timers.enable({ apis: ['setTimeout', 'setInterval'] });
  /** @type {Record<string, () => void>} */
  const listeners = {};
  const node = () => ({ textContent: '', dataset: {}, classList: { contains: () => false }, setAttribute() {}, removeAttribute() {} });
  const document = { hidden: false, getElementById: node, addEventListener: (/** @type {string} */ type, /** @type {() => void} */ fn) => (listeners[type] = fn) };
  let checks = 0;
  /** @type {(value: unknown) => void} */
  let answer = () => {};
  const fetch = () => {
    checks += 1;
    return new Promise((resolve) => (answer = resolve));
  };
  vm.runInNewContext(script, {
    document, fetch, setTimeout, clearTimeout, setInterval, AbortController, performance: { now: () => 0 }, Date, Math, String,
  });
  assert.equal(checks, 1);
  document.hidden = true;
  listeners.visibilitychange();
  answer({ ok: true, json: async () => status }); // the answer arrives after the tab hid
  await settle();
  t.mock.timers.tick(60_000);
  await settle();
  assert.equal(checks, 1, 'no check while hidden');
  document.hidden = false;
  listeners.visibilitychange();
  await settle();
  assert.equal(checks, 2, 'a check at once when visible again');
});
