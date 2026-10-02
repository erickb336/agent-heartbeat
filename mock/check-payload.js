#!/usr/bin/env node
// Check one message against the contract: the schema plus the consistency rules.
//
//   agent-heartbeat dry-run | node mock/check-payload.js
//
// Exit 0: the message is valid. Exit 1: it is not; the error codes go to stderr, one per line.
// The codes name paths and rules only, never values, so the output is safe to keep in CI logs.

import { validateMessage } from './validate.js';

const MAX_INPUT_BYTES = 1 << 20;

/** @type {Buffer[]} */
const chunks = [];
let size = 0;
for await (const chunk of process.stdin) {
  size += chunk.length;
  if (size > MAX_INPUT_BYTES) {
    console.error('/: too_large');
    process.exit(1);
  }
  chunks.push(chunk);
}

let value;
try {
  value = JSON.parse(Buffer.concat(chunks).toString('utf8'));
} catch {
  console.error('/: not_json');
  process.exit(1);
}

const result = validateMessage(value);
if (!result.ok) {
  for (const code of result.errors) console.error(code);
  process.exit(1);
}
