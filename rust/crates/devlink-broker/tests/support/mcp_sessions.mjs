// Actual SDK -> actual MCP -> Rust HTTP; central identity and business are fixtures.
import assert from 'node:assert/strict';
import { registerHooks } from 'node:module';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';
import { setTimeout as pause } from 'node:timers/promises';
import { randomUUID } from 'node:crypto';

const [build, brokerUrl] = process.argv.slice(2);
const buildUrl = pathToFileURL(`${build}/`).href;
const sourceUrl = new URL('../../../../../packages/mcp-server/package.json', import.meta.url).href;
registerHooks({ resolve(specifier, context, nextResolve) {
  if ((context.parentURL?.startsWith(buildUrl) || context.parentURL === import.meta.url)
    && !specifier.startsWith('.') && !specifier.includes(':')) {
    return nextResolve(specifier, { ...context, parentURL: sourceUrl });
  }
  return nextResolve(specifier, context);
} });
const { Client } = await import('@modelcontextprotocol/client');
const { InMemoryTransport } = await import('@modelcontextprotocol/server');
const { BrokerClient } = await import(pathToFileURL(join(build, 'broker/client.js')));
const { createServer } = await import(pathToFileURL(join(build, 'server.js')));
const localToken = process.env.DEVLINK_TEST_LOCAL_TOKEN;
const hosts = [];
async function host(agentId = '12') {
  const broker = new BrokerClient({
    settings: { schemaVersion: 1, brokerUrl, localToken, agentId },
    requestTimeoutMs: 3000, callTimeoutMs: 5000,
    heartbeatMs: 300_000, retryLimit: 0,
  });
  const server = createServer({ debug() {}, info() {}, error() {} }, broker);
  const client = new Client({ name: 'session-contract', version: '1' }, { versionNegotiation: { mode: 'legacy' } });
  const [a, b] = InMemoryTransport.createLinkedPair();
  const h = { broker, client, server };
  hosts.push(h);
  await Promise.all([server.connect(a), client.connect(b)]);
  return h;
}
async function call(h, expected) {
  const result = await h.client.callTool({ name: 'create_task', arguments: {
    recipientAgentId: '13', title: 'session test', description: 'synthetic',
  } });
  const value = result.structuredContent;
  assert.deepEqual(JSON.parse(result.content[0].text), value);
  assert.ok(!JSON.stringify(value).includes(localToken));
  if (expected) {
    assert.equal(result.isError, true);
    assert.equal(value.error.code, expected);
    assert.equal(typeof value.error.requestId, 'string');
  } else {
    assert.equal(result.isError, undefined, JSON.stringify(value));
    assert.equal(value.data.status, 'PENDING');
    assert.equal(h.broker.state, 'bound');
  }
}
async function raw(path, body, code, method = 'POST') {
  const requestId = randomUUID();
  const response = await fetch(`${brokerUrl}${path}`, {
    method, headers: { authorization: `Bearer ${localToken}`, 'content-type': 'application/json' },
    body: JSON.stringify({ ...body, requestId }), signal: AbortSignal.timeout(3000),
  });
  const value = await response.json();
  assert.equal(value.requestId, requestId);
  assert.equal(response.ok, !code);
  if (code) assert.equal(value.error.code, code);
  return value.data;
}
try {
  const first = await host();
  await call(first);
  const impostor = await host('13');
  await call(impostor, 'BROKER_FORBIDDEN');
  assert.notEqual(impostor.broker.state, 'LOCAL_AUTH_FORBIDDEN');
  const owner = await raw('/v1/sessions', { instanceId: 'replacement', mode: 'fresh', agentId: '12' });
  const repeated = await raw('/v1/sessions', { instanceId: 'replacement', mode: 'recover', agentId: '12' });
  assert.equal(repeated.sessionId, owner.sessionId);
  await raw('/v1/sessions', { instanceId: 'late', mode: 'recover', agentId: '12' }, 'SESSION_CONFLICT');
  await call(first, 'SESSION_REPLACED');
  await first.broker.close();
  await raw(`/v1/sessions/${owner.sessionId}/heartbeat`, { instanceId: 'replacement' });
  await raw(`/v1/sessions/${owner.sessionId}`, { instanceId: 'replacement' }, undefined, 'DELETE');

  const recovering = await host();
  await call(recovering);
  // Real production 90s lease, no clock/TTL override. No automatic heartbeat.
  await pause(91_000);
  await call(recovering, 'SESSION_EXPIRED');
  await call(recovering); // explicit next invocation recovers the empty slot
  console.log(JSON.stringify({ passed: true, leaseSeconds: 90, central: 'fixture' }));
} finally {
  for (const h of hosts.reverse()) {
    await h.client.close();
    await h.broker.close();
    await h.server.close();
  }
}
