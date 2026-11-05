import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { setTimeout as delay } from 'node:timers/promises';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { Client } from '@modelcontextprotocol/client';
import { getDefaultEnvironment, StdioClientTransport } from '@modelcontextprotocol/client/stdio';
import { InMemoryTransport } from '@modelcontextprotocol/server';
import { BrokerClient } from '../dist/broker/client.js';
import { BrokerHttp } from '../dist/broker/http.js';
import { createServer } from '../dist/server.js';
import { brokerEnvelopeSchema } from '../dist/broker/envelopes.js';
import { fakeBroker } from './helpers/fake-broker.mjs';
import { privateSettings } from './helpers/private-settings.mjs';

const gate = () => { let resolve; const promise = new Promise((r) => { resolve = r; }); return { promise, resolve }; };
const code = (expected) => (error) => error.code === expected;
const faultOf = (promise) => promise.then(() => assert.fail('Expected rejection'), (error) => error);
async function fixture(t, options = {}, unbound = false) {
  const broker = await fakeBroker();
  const client = new BrokerClient({ settings: { ...broker.settings, ...(unbound ? {} : { agentId: 'agent-a' }) },
    heartbeatMs: 60_000, retryBaseMs: 60_000, ...options });
  t.after(async () => { await client.close(); await broker.close(); });
  return { broker, client };
}
async function drained(client) {
  const deadline = Date.now() + 1000;
  while (client.pendingCount && Date.now() < deadline) await delay(5);
  assert.equal(client.pendingCount, 0);
}

test('wire error fixtures retain safe correlation and distinguish authentication, domain and transport failures', { timeout: 10000 }, async (t) => {
  const fixtures = JSON.parse(await readFile(new URL('../../../contracts/local-broker/v1/error-fixtures.json', import.meta.url), 'utf8'));
  for (const sample of fixtures) {
    assert.ok(brokerEnvelopeSchema.safeParse({ requestId: 'sample-request', error: sample.error }).success, sample.name);
    const { broker, client } = await fixture(t);
    await client.execute('get_tasks', {});
    broker.setHook(({ body, send }) => {
      if (body.operation !== 'create_task') return false;
      send({ error: { ...sample.error, message: 'private raw database/token details', debug: broker.settings.localToken } }, sample.status);
      return true;
    });
    const error = await faultOf(client.execute('create_task', {}, { mcpRequestId: 'original-rpc-id' }));
    for (const field of ['code', 'category', 'outcome', 'taskId', 'requestKey']) assert.equal(error[field], sample.expected[field], `${sample.name}: ${field}`);
    assert.equal(error.mcpRequestId, 'original-rpc-id');
    assert.equal(error.requestId, broker.calls.at(-1).body.requestId);
    assert.ok(!JSON.stringify(error).includes('private raw'));
    assert.ok(!JSON.stringify(error).includes(broker.settings.localToken));
    const count = broker.calls.length;
    broker.setHook(undefined);
    if (sample.expected.terminal) {
      await assert.rejects(client.execute('get_tasks', {}), code(sample.expected.code));
      assert.equal(broker.calls.length, count);
    } else {
      await client.execute('get_tasks', {});
      assert.equal(broker.calls.filter((call) => call.path === '/v1/sessions').length, 1);
    }
  }
});

test('stored PENDING delivery failures and repeated central results stay successful and unchanged', async (t) => {
  const { broker, client } = await fixture(t);
  for (const status of ['FAILED', 'UNKNOWN']) {
    const stored = { taskId: 'saved-task', status: 'PENDING', recipientAgentId: 'recipient-a', requestKey: 'broker-key', delivery: { status }, result: { previous: 'central-result' } };
    broker.setHook(({ body, send }) => {
      if (body.operation !== 'create_task') return false;
      assert.equal(body.requestKey, undefined);
      send({ requestKey: 'broker-key', data: stored }); return true;
    });
    const first = await client.execute('create_task', { recipientAgentId: 'recipient-a' }, { mcpRequestId: 0 });
    const repeated = await client.execute('create_task', { recipientAgentId: 'recipient-a' }, { mcpRequestId: 'new-explicit-call' });
    assert.deepEqual(first.data, stored); assert.deepEqual(repeated.data, stored);
    assert.equal(first.requestKey, 'broker-key'); assert.equal(first.mcpRequestId, 0);
    assert.notEqual(first.requestId, repeated.requestId);
  }
  assert.equal(broker.calls.filter((call) => call.body.operation === 'create_task').length, 4);
  assert.equal(broker.calls.filter((call) => call.body.operation === 'update_task_status').length, 0);
});

test('stored creation keys do not replace or conflict with later read/update call keys', async (t) => {
  const { broker, client } = await fixture(t);
  const stored = { taskId: 'existing-task', requestKey: 'original-create-key', status: 'PENDING' };
  for (const operation of ['update_task_status', 'complete_task', 'get_task_result']) {
    for (const envelopeKey of ['this-call-key', undefined]) {
      broker.setHook(({ body, send }) => {
        if (body.operation !== operation) return false;
        send({ ...(envelopeKey ? { requestKey: envelopeKey } : {}), data: stored }); return true;
      });
      const result = await client.execute(operation, { taskId: 'existing-task' });
      assert.deepEqual(result.data, stored);
      assert.equal(result.requestKey, envelopeKey);
    }
  }
});

test('unscoped control-plane 403 stops claim, heartbeat and bind; raw denial is safe too', async (t) => {
  for (const operation of ['claim', 'heartbeat', 'bind', 'raw-claim', 'malformed-claim']) {
    const { broker, client } = await fixture(t);
    if (operation === 'heartbeat' || operation === 'bind') await client.execute('get_tasks', {});
    broker.setHook(({ path, res, send }) => {
      const matches = operation.includes('claim') ? path === '/v1/sessions' : path.endsWith(`/${operation}`);
      if (!matches) return false;
      if (operation === 'raw-claim') res.writeHead(403).end('private credential failure');
      else if (operation === 'malformed-claim') send({ error: { code: 'FORBIDDEN', scope: 'unknown-future-scope', taskId: 'invalid metadata' } }, 403);
      else send({ error: { code: 'FORBIDDEN' } }, 403);
      return true;
    });
    const error = await faultOf(operation === 'heartbeat' ? client.heartbeat()
      : client.execute(operation === 'bind' ? 'register_agent' : 'get_tasks', {}));
    assert.equal(error.code, 'LOCAL_AUTH_FORBIDDEN');
    assert.equal(client.state, 'LOCAL_AUTH_FORBIDDEN');
    const count = broker.calls.length;
    await assert.rejects(client.execute('get_tasks', {}), code('LOCAL_AUTH_FORBIDDEN'));
    assert.equal(broker.calls.length, count);
    assert.ok(!String(error).includes('private credential'));
  }
});

test('validated session codes on control 403 retain their reason and recovery policy', async (t) => {
  for (const sessionCode of ['SESSION_EXPIRED', 'SESSION_CONFLICT', 'SESSION_REPLACED']) {
    const { broker, client } = await fixture(t);
    await client.execute('get_tasks', {});
    broker.setHook(({ path, send }) => {
      if (!path.endsWith('/heartbeat')) return false;
      send({ error: { scope: 'session', code: sessionCode } }, 403); return true;
    });
    await assert.rejects(client.heartbeat(), code(sessionCode));
    broker.setHook(undefined);
    if (sessionCode === 'SESSION_EXPIRED') {
      await client.execute('get_tasks', {});
      assert.equal(client.state, 'bound');
      assert.equal(broker.calls.filter((call) => call.path === '/v1/sessions').length, 2);
    } else {
      assert.equal(client.state, sessionCode);
      const count = broker.calls.length;
      await assert.rejects(client.execute('get_tasks', {}), code(sessionCode));
      assert.equal(broker.calls.length, count);
    }
  }
});

test('timeout, cancellation and connection loss never replay work or change a stored task to FAILED', { timeout: 5000 }, async (t) => {
  for (const mode of ['timeout', 'abort', 'disconnect']) {
    const { broker, client } = await fixture(t, { requestTimeoutMs: 50 });
    await client.execute('get_tasks', {});
    const entered = gate();
    let stored;
    broker.setHook(({ body, res, send }) => {
      if (body.operation === 'update_task_status') { stored.status = body.arguments.status; send({ data: stored }); return true; }
      if (body.operation === 'complete_task') { stored.status = 'COMPLETED'; send({ data: stored }); return true; }
      if (body.operation !== 'create_task') return false;
      stored = { taskId: 'saved-but-response-lost', status: 'PENDING' };
      entered.resolve();
      if (mode === 'disconnect') res.destroy();
      return true;
    });
    const controller = new AbortController();
    const result = faultOf(client.execute('create_task', {}, { signal: controller.signal, mcpRequestId: 'rpc-lost' }));
    await entered.promise;
    if (mode === 'abort') controller.abort();
    const error = await result;
    assert.equal(error.code, mode === 'timeout' ? 'BROKER_TIMEOUT' : mode === 'abort' ? 'REQUEST_ABORTED' : 'BROKER_CONNECTION_CLOSED');
    assert.equal(error.outcome, 'unknown'); assert.equal(error.category, 'transport');
    assert.equal(error.mcpRequestId, 'rpc-lost'); assert.equal(error.taskId, undefined); assert.equal(error.requestKey, undefined);
    await drained(client);
    assert.equal(stored.status, 'PENDING');
    assert.equal(broker.calls.filter((call) => call.body.operation === 'create_task').length, 1);
    assert.equal(broker.calls.filter((call) => ['update_task_status', 'complete_task'].includes(call.body.operation)).length, 0);
  }
});

test('registration requestKey survives bind failure while internal IDs do not replace the original call ID', async (t) => {
  const { broker, client } = await fixture(t, {}, true);
  broker.setHook(({ path, body, send }) => {
    if (body.operation === 'register_agent') { send({ data: { agentId: 'registered-agent', requestKey: 'register-key' } }); return true; }
    if (path.endsWith('/bind')) { send({ requestKey: 'control-key', error: { scope: 'session', code: 'SESSION_EXPIRED' } }, 409); return true; }
    return false;
  });
  const error = await faultOf(client.execute('register_agent', { name: 'worker' }, { mcpRequestId: 'host-call' }));
  assert.equal(error.requestKey, 'register-key'); assert.equal(error.mcpRequestId, 'host-call');
  assert.equal(error.requestId, broker.calls.find((call) => call.body.operation === 'register_agent').body.requestId);
  assert.notEqual(error.requestId, broker.calls.at(-1).body.requestId);
});

test('ambiguous envelopes, conflicting keys and invalid metadata fail safely', async (t) => {
  const { broker } = await fixture(t);
  const http = new BrokerHttp(broker.settings);
  t.after(() => http.close());
  for (const envelope of [
    { data: {}, error: { code: 'TASK_SAVE_FAILED' } }, {},
    { requestKey: 'one-key', error: { code: 'TASK_SAVE_FAILED', requestKey: 'other-key' } },
    { error: { code: 'TASK_SAVE_FAILED', taskId: 'private invalid token value' } },
  ]) {
    broker.setHook(({ send }) => { send(envelope); return true; });
    const error = await faultOf(http.request('/v1/test', { requestId: 'expected' }));
    assert.equal(error.code, 'BROKER_PROTOCOL_ERROR'); assert.equal(error.outcome, 'unknown');
    assert.equal(error.taskId, undefined); assert.equal(error.requestKey, undefined);
    assert.ok(!String(error).includes('private invalid'));
  }
});

test('a non-success HTTP status cannot discard a task ID and key that arrived in data', async (t) => {
  const { broker, client } = await fixture(t);
  broker.setHook(({ body, send }) => {
    if (body.operation !== 'create_task') return false;
    send({ data: { taskId: 'already-stored-task', status: 'PENDING', requestKey: 'arrived-key', privateDebug: 'do-not-copy-raw-error-data' } }, 500);
    return true;
  });
  const error = await faultOf(client.execute('create_task', {}, { mcpRequestId: 0 }));
  assert.equal(error.code, 'BROKER_SERVER_ERROR'); assert.equal(error.outcome, 'unknown');
  assert.equal(error.taskId, 'already-stored-task'); assert.equal(error.requestKey, 'arrived-key');
  assert.equal(error.mcpRequestId, 0);
  assert.ok(!JSON.stringify(error).includes('do-not-copy-raw-error-data'));
  assert.equal(broker.calls.filter((call) => call.body.operation === 'create_task').length, 1);
});

test('a single call deadline bounds session, central registration and bind together', { timeout: 5000 }, async (t) => {
  const { broker, client } = await fixture(t, { requestTimeoutMs: 2000, callTimeoutMs: 500 }, true);
  broker.setHook(async ({ path, body, send }) => {
    if (path === '/v1/sessions' || body.operation === 'register_agent') await delay(25);
    if (path.endsWith('/bind')) await delay(1000);
    if (body.operation === 'register_agent') { send({ requestKey: 'known-register-key', data: { agentId: 'registered-agent' } }); return true; }
    return false;
  });
  const start = Date.now();
  const error = await faultOf(client.execute('register_agent', { name: 'worker' }, { mcpRequestId: 'deadline-rpc' }));
  assert.equal(error.code, 'BROKER_TIMEOUT'); assert.equal(error.outcome, 'unknown');
  assert.equal(error.requestKey, 'known-register-key');
  assert.ok(Date.now() - start < 1000);
  await drained(client); await delay(100);
  assert.notEqual(client.state, 'bound');
  assert.equal(broker.calls.filter((call) => call.body.operation === 'register_agent').length, 1);
});

test('a mutation aborted before any response preserves the caller-known task and request key', async (t) => {
  const { broker, client } = await fixture(t);
  await client.execute('get_tasks', {});
  const entered = gate();
  broker.setHook(({ body }) => {
    if (body.operation !== 'update_task_status') return false;
    entered.resolve();
    return true;
  });
  const controller = new AbortController();
  const pending = faultOf(client.execute('update_task_status', {
    taskId: 'known-task', requestKey: 'known-key-before-response', version: '1', status: 'IN_PROGRESS',
  }, { signal: controller.signal, mcpRequestId: 'known-mcp-id' }));
  await entered.promise;
  controller.abort();
  const error = await pending;
  assert.equal(error.code, 'REQUEST_ABORTED');
  assert.equal(error.outcome, 'unknown');
  assert.equal(error.taskId, 'known-task');
  assert.equal(error.requestKey, 'known-key-before-response');
  assert.equal(error.mcpRequestId, 'known-mcp-id');
  assert.equal(broker.calls.filter(call => call.body.operation === 'update_task_status').length, 1);
});

test('question and answer keys survive a lost response so the retry can reuse them', async (t) => {
  const { broker, client } = await fixture(t);
  await client.execute('get_tasks', {});
  for (const [operation, args] of [
    ['ask_question', { taskId: '101', message: 'Which API?', requestKey: 'issued-question-key' }],
    ['answer_question', { taskId: '101', questionEventId: '501', message: 'v2', requestKey: 'issued-answer-key' }],
  ]) {
    const entered = gate();
    broker.setHook(({ body }) => {
      if (body.operation !== operation) return false;
      entered.resolve();
      return true;
    });
    const controller = new AbortController();
    const pending = faultOf(client.execute(operation, args, { signal: controller.signal, mcpRequestId: operation }));
    await entered.promise;
    controller.abort();
    const error = await pending;
    assert.equal(error.code, 'REQUEST_ABORTED');
    assert.equal(error.outcome, 'unknown');
    assert.equal(error.requestKey, args.requestKey);
    assert.equal(error.taskId, '101');
    await drained(client);
  }
});

test('a caller create_task key survives a lost response so the retry can reuse it', async (t) => {
  const { broker, client } = await fixture(t);
  await client.execute('get_tasks', {});
  const entered = gate();
  broker.setHook(({ body }) => {
    if (body.operation !== 'create_task') return false;
    entered.resolve();
    return true;
  });
  const controller = new AbortController();
  const args = { recipientAgentId: 'agent-b', title: 'x', description: 'y', requestKey: 'agent-chosen-key' };
  const pending = faultOf(client.execute('create_task', args, { signal: controller.signal, mcpRequestId: 'lost-create' }));
  await entered.promise;
  controller.abort();
  const error = await pending;
  assert.equal(error.code, 'REQUEST_ABORTED');
  assert.equal(error.outcome, 'unknown');
  assert.equal(error.requestKey, 'agent-chosen-key');
  assert.equal(error.taskId, undefined);
  assert.deepEqual(broker.calls.find(call => call.body.operation === 'create_task').body.arguments, args);
});

test('one timed-out caller cannot abort a shared session claim or send its late business call', { timeout: 5000 }, async (t) => {
  const { broker, client } = await fixture(t, { requestTimeoutMs: 500, callTimeoutMs: 60 });
  const entered = gate(); const release = gate();
  t.after(() => release.resolve());
  broker.setHook(async ({ path }) => { if (path !== '/v1/sessions') return false; entered.resolve(); await release.promise; return false; });
  const first = faultOf(client.execute('create_task', {}, { mcpRequestId: 'expired' }));
  await entered.promise;
  assert.equal((await first).code, 'BROKER_TIMEOUT');
  const second = client.execute('get_tasks', {}, { mcpRequestId: 'alive' });
  release.resolve(); await second;
  assert.equal(broker.calls.filter((call) => call.path === '/v1/sessions').length, 1);
  assert.equal(broker.calls.filter((call) => call.body.operation === 'create_task').length, 0);
});

test('a caller deadline preserves a healthy shared session and a concurrent registration', { timeout: 5000 }, async (t) => {
  const { broker, client } = await fixture(t, { requestTimeoutMs: 1500, callTimeoutMs: 600 });
  await client.execute('get_tasks', {});
  const slowEntered = gate(); const registrationEntered = gate(); const releaseRegistration = gate();
  t.after(() => releaseRegistration.resolve());
  broker.setHook(async ({ body }) => {
    if (body.operation === 'get_task_result') { slowEntered.resolve(); return true; }
    if (body.operation === 'register_agent') { registrationEntered.resolve(); await releaseRegistration.promise; }
    return false;
  });
  const slow = faultOf(client.execute('get_task_result', { taskId: 'existing-task' }));
  await slowEntered.promise; await delay(400);
  const registration = client.execute('register_agent', { name: 'refresh' }).then((value) => ({ value }), (error) => ({ error }));
  await registrationEntered.promise;
  const timedOut = await slow;
  assert.equal(timedOut.code, 'BROKER_TIMEOUT'); assert.equal(timedOut.outcome, 'unknown');
  assert.equal(timedOut.taskId, 'existing-task');
  assert.equal(client.state, 'bound');
  assert.equal(broker.calls.filter((call) => call.path === '/v1/sessions').length, 1);
  releaseRegistration.resolve();
  const completed = await registration;
  assert.equal(completed.error, undefined);
  assert.equal(client.state, 'bound');
  assert.equal(broker.calls.filter((call) => call.path.endsWith('/bind')).length, 1);
});

test('queued registration deadline and client close prevent subsequent IO', { timeout: 5000 }, async (t) => {
  for (const mode of ['deadline', 'close']) {
    const { broker, client } = await fixture(t, { callTimeoutMs: mode === 'deadline' ? 60 : 1000 });
    const entered = gate();
    broker.setHook(({ body }) => { if (body.operation !== 'register_agent') return false; entered.resolve(); return true; });
    const first = faultOf(client.execute('register_agent', { name: 'first' }));
    const second = faultOf(client.execute('register_agent', { name: 'queued' }));
    await entered.promise;
    if (mode === 'close') await client.close();
    const results = await Promise.all([first, second]);
    assert.ok(results.every((error) => error.code === (mode === 'close' ? 'CLIENT_CLOSED' : 'BROKER_TIMEOUT')));
    await drained(client);
    assert.equal(broker.calls.filter((call) => call.body.operation === 'register_agent').length, 1);
  }
});

test('SDK argument errors and unknown-tool protocol errors retain their wire ID without a broker call', async (t) => {
  const { broker, client: adapter } = await fixture(t);
  const server = createServer({ debug() {}, info() {}, error() {} }, adapter);
  const [serverTransport, transport] = InMemoryTransport.createLinkedPair();
  const client = new Client({ name: 'errors-63', version: '1' }, { versionNegotiation: { mode: 'legacy' } });
  t.after(async () => { await client.close(); await server.close(); });
  const sent = []; const received = [];
  const originalSend = transport.send.bind(transport);
  transport.send = async (message, options) => { sent.push(structuredClone(message)); await originalSend(message, options); };
  await Promise.all([server.connect(serverTransport), client.connect(transport)]);
  const onmessage = transport.onmessage;
  transport.onmessage = (message, ...rest) => { received.push(structuredClone(message)); onmessage?.(message, ...rest); };
  const invalid = await client.callTool({ name: 'create_task', arguments: { recipientAgentId: 'missing-required-fields' } });
  assert.equal(invalid.isError, true);
  const request = sent.findLast((message) => message.method === 'tools/call');
  const response = received.find((message) => message.id === request.id);
  assert.equal(response.id, request.id); assert.equal(response.result.isError, true);
  assert.ok(response.result.content.some((part) => part.text?.includes('Input validation error')));
  assert.equal(response.result.structuredContent, undefined);
  await assert.rejects(client.callTool({ name: 'missing_tool_63', arguments: {} }));
  const unknown = sent.findLast((message) => message.method === 'tools/call');
  const protocolError = received.find((message) => message.id === unknown.id);
  assert.equal(protocolError.id, unknown.id); assert.equal(protocolError.error.code, -32602);
  assert.equal(protocolError.result, undefined); assert.equal(broker.calls.length, 0);
});

test('output validation failure retains completed call correlation without copying its invalid payload', async (t) => {
  let rpcId;
  const server = createServer({ debug() {}, info() {}, error() {} }, {
    async execute(operation, args, options) {
      rpcId = options.mcpRequestId;
      return { requestId: 'completed-call', requestKey: 'completed-key', data: { taskId: 'stored-task', privateDebug: 'secret-data' }, unexpected: 'secret-extra-output' };
    },
  });
  const [serverTransport, transport] = InMemoryTransport.createLinkedPair();
  const client = new Client({ name: 'output-error', version: '1' }, { versionNegotiation: { mode: 'legacy' } });
  t.after(async () => { await client.close(); await server.close(); });
  await Promise.all([server.connect(serverTransport), client.connect(transport)]);
  const result = await client.callTool({ name: 'get_task_result', arguments: { taskId: 'stored-task' } });
  assert.equal(result.isError, true);
  const error = result.structuredContent.error;
  assert.equal(error.code, 'TOOL_EXECUTION_FAILED'); assert.equal(error.outcome, 'unknown');
  assert.equal(error.mcpRequestId, rpcId); assert.equal(error.requestId, 'completed-call');
  assert.equal(error.requestKey, 'completed-key'); assert.equal(error.taskId, 'stored-task');
  assert.ok(!JSON.stringify(result).includes('secret-'));
});

for (const mode of ['legacy', { pin: '2026-07-28' }]) {
  test(`stdio ${JSON.stringify(mode)} returns safe execution errors with original RPC/request/key/task IDs`, { timeout: 15000 }, async (t) => {
    const broker = await fakeBroker();
    const file = await privateSettings({ ...broker.settings, agentId: 'agent-a' });
    const client = new Client({ name: 'error-stdio', version: '1' }, { versionNegotiation: { mode } });
    const transport = new StdioClientTransport({ command: process.execPath,
      args: [fileURLToPath(new URL('../dist/index.js', import.meta.url)), '--transport', 'stdio', '--request-timeout-ms', '200', '--call-timeout-ms', '1000'],
      env: { ...getDefaultEnvironment(), DEVLINK_BROKER_FILE: file.path }, stderr: 'pipe' });
    let logs = ''; transport.stderr.on('data', (chunk) => { logs += chunk; });
    t.after(async () => { await client.close(); await broker.close(); await file.cleanup(); });
    broker.setHook(({ body, send }) => {
      if (body.operation !== 'get_task_result') return false;
      send({ requestKey: 'preserved-key', error: { scope: 'domain', code: 'TASK_STATE_CONFLICT', taskId: 'known-task', message: broker.settings.localToken, details: 'secret raw data' } }, 409);
      return true;
    });
    await client.connect(transport);
    const result = await client.callTool({ name: 'get_task_result', arguments: { taskId: 'known-task' } });
    assert.equal(result.isError, true);
    const error = result.structuredContent.error;
    const request = broker.calls.findLast((call) => call.body.operation === 'get_task_result');
    assert.equal(error.mcpRequestId, request.body.mcpRequestId); assert.equal(error.requestId, request.body.requestId);
    assert.equal(error.requestKey, 'preserved-key'); assert.equal(error.taskId, 'known-task');
    assert.equal(error.category, 'domain'); assert.deepEqual(JSON.parse(result.content[0].text), result.structuredContent);
    assert.ok(!JSON.stringify(result).includes(broker.settings.localToken)); assert.ok(!logs.includes(broker.settings.localToken));
  });
}
