#!/usr/bin/env node
// Start the mock receiver from the command line:
//
//   node mock/server.js [--port 8787] [--host 127.0.0.1] [--max-ttl-seconds 300]
//
// --max-ttl-seconds caps every lease. A small value (for example 5) lets you watch a lease end
// without waiting minutes.
//
// It reads the bearer secret from MOCK_RECEIVER_SECRET. Without it, it makes a throwaway secret
// and prints it, for local tests only. It logs one line per request: method, route and status.

import { randomBytes } from 'node:crypto';
import { parseArgs } from 'node:util';
import { HEARTBEAT_PATH, STATUS_PATH, startMockReceiver } from './receiver.js';

const { values } = parseArgs({
  options: {
    port: { type: 'string', default: '8787' },
    host: { type: 'string', default: '127.0.0.1' },
    'max-ttl-seconds': { type: 'string', default: '300' },
  },
});

const port = Number(values.port);
if (!Number.isInteger(port) || port < 0 || port > 65_535) {
  console.error('--port must be an integer from 0 to 65535.');
  process.exit(2);
}

const maxTtlSeconds = Number(values['max-ttl-seconds']);
if (!Number.isInteger(maxTtlSeconds) || maxTtlSeconds < 0) {
  console.error('--max-ttl-seconds must be a non-negative integer.');
  process.exit(2);
}

const fromEnv = process.env.MOCK_RECEIVER_SECRET;
const secret = fromEnv || randomBytes(32).toString('base64url');

let receiver;
try {
  receiver = await startMockReceiver({
    port,
    host: values.host,
    secret,
    maxTtlSeconds,
    onRequest: ({ method, path, status }) => console.log(`${new Date().toISOString()} ${method} ${path} ${status}`),
  });
} catch (error) {
  console.error(`The mock receiver did not start: ${error instanceof Error ? error.message : 'unknown error'}`);
  process.exit(1);
}

console.log(`Mock receiver on ${receiver.url}`);
console.log(`  Panel preview  GET  ${receiver.url}/`);
console.log(`  Public status  GET  ${receiver.url}${STATUS_PATH}`);
console.log(`  Heartbeats     POST ${receiver.url}${HEARTBEAT_PATH}`);
console.log(
  fromEnv
    ? '  Secret: from MOCK_RECEIVER_SECRET (not printed).'
    : `  Throwaway secret for local tests only: ${secret}`,
);
console.log(`  Longest lease  ${maxTtlSeconds} s`);
console.log('State is in memory only. Press Ctrl-C to stop.');

const stop = async () => {
  await receiver.close();
  process.exit(0);
};
process.once('SIGINT', stop);
process.once('SIGTERM', stop);
