import assert from 'node:assert/strict';
import { writeFile } from 'node:fs/promises';
import test from 'node:test';
import { BrokerClient } from '../dist/broker/client.js';
import { BrokerHttp } from '../dist/broker/http.js';
import { operationRoutes } from '../dist/broker/contracts.js';
import { readBrokerSettings, validateSettings } from '../dist/broker/settings.js';
import { fakeBroker } from './helpers/fake-broker.mjs';
import { privateSettings, allowOtherUserRead } from './helpers/private-settings.mjs';

const delay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
async function fixture(t, options = {}) {
  const broker = await fakeBroker();
  const client = new BrokerClient({ settings: { ...broker.settings, agentId: 'agent-a' }, heartbeatMs: 60_000, ...options });
  t.after(async () => { await client.close(); await broker.close(); });
  return { broker, client };
}
const code = (expected) => (error) => error.code === expected;

test('lazy bootstrap, trusted context, explicit routes and unchanged business data', async (t) => {
  const { broker, client } = await fixture(t);
  assert.equal(client.state, 'inactive');
  assert.equal(broker.calls.length, 0);
  for (const operation of Object.keys(operationRoutes)) {
    const result = await client.execute(operation, { sample: operation }, { mcpRequestId: 27 });
    if (operation === 'register_agent') assert.equal(result.data.agentId, 'agent-a');
    else if (operation === 'get_updates') assert.equal(result.data.coverage, 'local');
    else if (operation === 'find_agent') assert.equal(result.data.insufficientReason, 'NO_ELIGIBLE_AGENTS');
    else {
      assert.equal(result.data.taskId, 'task-1');
      assert.equal(result.data.status, 'PENDING');
      assert.equal(result.data.delivery.status, 'FAILED');
    }
    const call = broker.calls.findLast((c) => c.path === operationRoutes[operation]);
    assert.deepEqual(call.body.context, { userId: 'trusted-user', teamId: 'trusted-team', agentId: 'agent-a', sessionId: [...broker.sessions.keys()][0] });
    assert.equal(call.body.mcpRequestId, 27);
    assert.equal(call.body.requestId, result.requestId);
  }
  assert.equal(broker.calls.filter((c) => c.path === '/v1/sessions').length, 1);
});

test('no configured agent: registration binds the authenticated unbound session', async (t) => {
  const broker = await fakeBroker();
  const client = new BrokerClient({ settings: broker.settings });
  t.after(async () => { await client.close(); await broker.close(); });
  await assert.rejects(client.execute('list_agents', {}), code('AGENT_UNREGISTERED'));
  assert.equal(broker.calls.length, 1);
  assert.equal(client.state, 'unbound');
  await client.execute('register_agent', { name: 'worker' });
  assert.equal(client.state, 'bound');
  await client.execute('list_agents', {});
  assert.equal(broker.calls.at(-1).body.context.agentId, 'registered-agent');
});

test('concurrent out-of-order responses retain request correlation and create one session', async (t) => {
  const { broker, client } = await fixture(t);
  const order = [];
  broker.setHook(async ({ path, body, send }) => {
    if (path !== operationRoutes.get_tasks) return false;
    await delay((8 - body.arguments.index) * 8);
    order.push(body.arguments.index);
    send({ data: { index: body.arguments.index } });
    return true;
  });
  const results = await Promise.all(Array.from({ length: 8 }, (_, index) => client.execute('get_tasks', { index })));
  assert.deepEqual(results.map((r) => r.data.index), [0, 1, 2, 3, 4, 5, 6, 7]);
  assert.notDeepEqual(order, [0, 1, 2, 3, 4, 5, 6, 7]);
  assert.equal(new Set(results.map((r) => r.requestId)).size, 8);
  assert.equal(broker.calls.filter((c) => c.path === '/v1/sessions').length, 1);
  assert.equal(client.pendingCount, 0);
});

test('timeout and cancellation free pending requests, preserve unknown outcomes and never replay business', async (t) => {
  const { broker, client } = await fixture(t, { requestTimeoutMs: 80, retryBaseMs: 10_000 });
  await client.execute('get_tasks', {});
  broker.setHook(({ path }) => path === operationRoutes.create_task);
  await assert.rejects(client.execute('create_task', {}), (e) => e.code === 'BROKER_TIMEOUT' && e.outcome === 'unknown');
  assert.equal(client.pendingCount, 0);
  const controller = new AbortController();
  const pending = client.execute('create_task', {}, { signal: controller.signal });
  await delay(20);
  controller.abort();
  await assert.rejects(pending, code('REQUEST_ABORTED'));
  assert.equal(client.pendingCount, 0);
  assert.equal(broker.calls.filter((c) => c.path === operationRoutes.create_task).length, 2);
});

test('close aborts business calls and bounds unregistration even when the broker hangs', async (t) => {
  const { broker, client } = await fixture(t, { shutdownTimeoutMs: 60 });
  await client.execute('get_tasks', {});
  broker.setHook(({ path, req }) => path === operationRoutes.create_task || req.method === 'DELETE');
  const pending = client.execute('create_task', {});
  const rejection = assert.rejects(pending, code('CLIENT_CLOSED'));
  await delay(20);
  const started = Date.now();
  await Promise.all([client.close(), client.close(), rejection]);
  assert.ok(Date.now() - started < 1000);
  assert.equal(client.pendingCount, 0);
  assert.equal(client.state, 'closed');
});

test('fresh replacement fences stale heartbeat/delete; losing instance stops recovery', async (t) => {
  const { broker, client: a } = await fixture(t);
  const advanced = new BrokerHttp(broker.settings);
  t.after(() => advanced.close());
  await a.execute('get_tasks', {});
  const old = a.session;
  const replacement = await advanced.request('/v1/sessions', { instanceId: 'explicit-owner', mode: 'fresh', agentId: 'agent-a' });
  const winner = broker.owners.get('agent-a');
  await assert.rejects(a.heartbeat(), code('SESSION_REPLACED'));
  const count = broker.calls.length;
  await assert.rejects(a.execute('get_tasks', {}), code('SESSION_REPLACED'));
  assert.equal(broker.calls.length, count);
  await assert.rejects(advanced.request(`/v1/sessions/${old.sessionId}`, { instanceId: a.instanceId }, { method: 'DELETE' }), code('SESSION_REPLACED'));
  await a.close();
  assert.equal(broker.owners.get('agent-a'), winner);
  await advanced.request(`/v1/sessions/${replacement.sessionId}/heartbeat`, { instanceId: 'explicit-owner' });
});

test('after broker restart the first recovery wins; a later recovery cannot steal ownership', async (t) => {
  const { broker, client: a } = await fixture(t);
  const b = new BrokerClient({ settings: { ...broker.settings, agentId: 'agent-a' }, retryBaseMs: 60_000 });
  t.after(() => b.close());
  await a.execute('get_tasks', {});
  const advanced = new BrokerHttp(broker.settings); t.after(() => advanced.close());
  await advanced.request('/v1/sessions', { instanceId: b.instanceId, mode: 'fresh', agentId: 'agent-a' });
  await b.execute('get_tasks', {}); // A has not observed its replacement yet.
  broker.restart();
  await assert.rejects(a.heartbeat(), (e) => ['SESSION_EXPIRED', 'BROKER_UNAVAILABLE'].includes(e.code));
  await assert.rejects(b.heartbeat(), (e) => ['SESSION_EXPIRED', 'BROKER_UNAVAILABLE'].includes(e.code));
  await a.heartbeat();
  const winner = broker.owners.get('agent-a');
  await assert.rejects(b.heartbeat(), code('SESSION_CONFLICT'));
  assert.equal(b.state, 'SESSION_CONFLICT');
  assert.equal(broker.owners.get('agent-a'), winner);
  const recoveries = broker.calls.filter((c) => c.path === '/v1/sessions').slice(-2);
  assert.ok(recoveries.every((c) => c.body.mode === 'recover'));
  await a.execute('get_tasks', {});
});

test('90 second broker-clock lease expires independently of client timestamps', async (t) => {
  const { broker, client } = await fixture(t);
  await client.execute('get_tasks', {});
  broker.advance(89_999);
  await client.heartbeat();
  broker.advance(90_000);
  await assert.rejects(client.execute('get_tasks', { timestamp: Number.MAX_SAFE_INTEGER }), code('SESSION_EXPIRED'));
  await client.heartbeat();
  assert.equal(client.state, 'bound');
});

test('a lost session response retries with the same instance and recovery mode', async (t) => {
  const { broker, client } = await fixture(t, { retryBaseMs: 60_000 });
  let first = true;
  broker.setHook(({ path, res }) => {
    if (path === '/v1/sessions' && first) { first = false; res.destroy(); return true; }
    return false;
  });
  await assert.rejects(client.execute('get_tasks', {}), code('BROKER_CONNECTION_CLOSED'));
  await client.execute('get_tasks', {});
  const calls = broker.calls.filter((c) => c.path === '/v1/sessions');
  assert.deepEqual(calls.map((c) => c.body.mode), ['recover', 'recover']);
  assert.equal(calls[0].body.instanceId, calls[1].body.instanceId);
});

test('bounded background session recovery does not replay a failed business request', async (t) => {
  const { broker, client } = await fixture(t, { retryBaseMs: 8, retryLimit: 2, requestTimeoutMs: 30 });
  await client.execute('get_tasks', {});
  broker.setHook(() => true);
  await assert.rejects(client.execute('create_task', {}), code('BROKER_TIMEOUT'));
  await delay(180);
  assert.equal(broker.calls.filter((c) => c.path === '/v1/sessions').length, 3);
  assert.equal(broker.calls.filter((c) => c.path === operationRoutes.create_task).length, 1);
  const count = broker.calls.length;
  await delay(80);
  assert.equal(broker.calls.length, count);
  assert.equal(client.pendingCount, 0);
});

test('untrusted response IDs, redirects, malformed and oversized bodies are rejected', async (t) => {
  const { broker } = await fixture(t);
  const http = new BrokerHttp(broker.settings, 1000);
  t.after(() => http.close());
  for (const response of [
    (res) => res.end(JSON.stringify({ requestId: 'wrong', data: {} })),
    (res) => res.writeHead(302, { location: `${broker.settings.brokerUrl}/secret` }).end(),
    (res) => res.end('invalid-json'),
    (res) => res.end('x'.repeat(262_145)),
  ]) {
    broker.setHook(({ res }) => { response(res); return true; });
    await assert.rejects(http.request('/v1/test', { requestId: 'expected' }), code('BROKER_PROTOCOL_ERROR'));
    assert.equal(http.pendingCount, 0);
  }
  assert.equal(broker.calls.length, 4); // The redirect was never followed.
});

test('authentication errors are terminal even with a non-JSON response; safe errors omit credentials', async (t) => {
  const { broker, client } = await fixture(t);
  broker.setHook(({ res }) => { res.writeHead(401).end('private raw broker detail'); return true; });
  await assert.rejects(client.execute('get_tasks', {}), (e) => e.code === 'BROKER_UNAUTHORIZED' && !JSON.stringify(e).includes(broker.settings.localToken));
  await assert.rejects(client.execute('get_tasks', {}), code('BROKER_UNAUTHORIZED'));
  assert.equal(broker.calls.length, 1);
});

test('only explicit literal loopback settings are accepted', () => {
  const base = { schemaVersion: 1, localToken: 'local-token-1234567890' };
  for (const brokerUrl of ['https://127.0.0.1:43127', 'http://localhost:43127', 'http://127.1:43127', 'http://2130706433:43127', 'http://127.0.0.2:43127', 'http://127.0.0.1:43127/path', 'http://user@127.0.0.1:43127', 'http://127.0.0.1:43127/?x=1']) {
    assert.throws(() => validateSettings({ ...base, brokerUrl }));
  }
});

test('a descriptor readable by another OS user is rejected', { timeout: 15000 }, async (t) => {
  const settings = { schemaVersion: 1, brokerUrl: 'http://127.0.0.1:43127', localToken: 'local-token-1234567890' };
  const file = await privateSettings(settings);
  t.after(() => file.cleanup());
  await allowOtherUserRead(file.path);
  await assert.rejects(readBrokerSettings(file.path), code('CREDENTIAL_FILE_INVALID'));
});

test('private settings snapshot works on this OS; invalid snapshots expose no file contents', { timeout: 15000 }, async (t) => {
  const broker = await fakeBroker();
  const file = await privateSettings(broker.settings);
  t.after(async () => { try { await file.cleanup(); } finally { await broker.close(); } });
  assert.deepEqual(await readBrokerSettings(file.path), broker.settings);
  await writeFile(file.path, '{"localToken":"do-not-leak-this"');
  await assert.rejects(readBrokerSettings(file.path), (e) => e.code === 'CREDENTIAL_FILE_INVALID' && !String(e).includes('do-not-leak'));
  await assert.rejects(readBrokerSettings('relative.json'), code('CREDENTIAL_FILE_INVALID'));
});
