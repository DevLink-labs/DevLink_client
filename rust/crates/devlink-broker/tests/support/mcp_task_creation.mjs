// Launched by the opt-in Rust test. Imports a fresh build of actual MCP sources;
// only the SDK transport between the test client and MCP server is in memory.
import assert from 'node:assert/strict';
import { registerHooks } from 'node:module';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';
import { setTimeout as pause } from 'node:timers/promises';

const [build, brokerUrl, scenario] = process.argv.slice(2);
const sourceUrl = new URL('../../../../../packages/mcp-server/package.json', import.meta.url).href;
// Resolve only the copied build's package imports through existing dependencies.
// No install, node_modules copy, or symlink outside/inside the workspace is needed.
const buildUrl = pathToFileURL(`${build}/`).href;
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
const agentToken = process.env.DEVLINK_TEST_AGENT_TOKEN;
assert.notEqual(localToken, agentToken);
const broker = new BrokerClient({
  settings: { schemaVersion: 1, brokerUrl, localToken: scenario === 'local-auth' ? agentToken : localToken, agentId: '12' },
  requestTimeoutMs: 3000, callTimeoutMs: 5000, heartbeatMs: 60_000,
});
const server = createServer({ debug() {}, info() {}, error() {} }, broker);
const client = new Client({ name: 'rust-integration', version: '1' }, { versionNegotiation: { mode: 'legacy' } });
const [serverTransport, clientTransport] = InMemoryTransport.createLinkedPair();
const args = { recipientAgentId: '13', title: 'MCP integration', description: '실제 MCP 입력', taskContext: 'mock only' };

async function call() {
  const result = await client.callTool({ name: 'create_task', arguments: args });
  assert.deepEqual(JSON.parse(result.content[0].text), result.structuredContent);
  const encoded = JSON.stringify(result);
  for (const secret of [localToken, agentToken, 'untrusted-central-message-do-not-expose']) {
    assert.ok(!encoded.includes(secret), 'secret leaked into MCP result');
  }
  const value = result.structuredContent;
  const correlated = value.error ?? value;
  assert.equal(typeof correlated.requestId, 'string');
  assert.equal(typeof correlated.mcpRequestId, 'number');
  if (scenario !== 'local-auth') assert.equal(typeof correlated.requestKey, 'string', JSON.stringify(value));
  return { result, value };
}

try {
  await Promise.all([server.connect(serverTransport), client.connect(clientTransport)]);
  const { result, value } = await call();
  if (scenario === 'success' || scenario === 'delivery-unknown') {
    assert.equal(result.isError, undefined);
    assert.match(value.data.taskId, /^[1-9][0-9]*$/);
    assert.equal(value.data.receiverAgentId, '13');
    assert.equal(value.data.status, 'PENDING');
    assert.equal(value.data.replayed, false);
    assert.equal(value.requestKey, value.data.requestKey);
    assert.equal(value.data.delivery.status, scenario === 'success' ? 'ACK_PENDING' : 'DELIVERY_UNKNOWN');
    if (scenario === 'delivery-unknown') assert.equal(value.data.delivery.reasonCode, 'ACK_TIMEOUT');
    if (scenario === 'success') {
      // An explicit tool re-invocation is a NEW request, not an idempotent replay.
      const second = await call();
      assert.equal(second.result.isError, undefined);
      assert.notEqual(second.value.mcpRequestId, value.mcpRequestId);
      assert.notEqual(second.value.requestId, value.requestId);
      assert.notEqual(second.value.requestKey, value.requestKey);
      assert.notEqual(second.value.data.taskId, value.data.taskId);
    }
  } else {
    const expected = {
      'central-auth': ['CENTRAL_UNAUTHENTICATED', 'rejected'],
      timeout: ['CENTRAL_TIMEOUT', 'unknown'],
      'local-auth': ['BROKER_UNAUTHORIZED', 'rejected'],
    }[scenario];
    assert.ok(expected, 'unknown scenario');
    assert.equal(result.isError, true);
    assert.equal(value.error.code, expected[0]);
    assert.equal(value.error.outcome, expected[1]);
    if (scenario !== 'local-auth') {
      assert.equal(broker.state, 'bound');
      await broker.heartbeat(); // Central failure must not revoke local authentication.
    }
    await pause(250); // Rust also checks that no implicit mutation retry occurred.
  }
  console.log(JSON.stringify({ scenario, passed: true }));
} finally {
  await client.close();
  await broker.close();
  await server.close();
}
