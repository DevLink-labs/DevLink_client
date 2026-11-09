import assert from 'node:assert/strict';
import test from 'node:test';
import { PassThrough } from 'node:stream';
import { fileURLToPath } from 'node:url';
import { z } from 'zod';
import { Client } from '@modelcontextprotocol/client';
import { getDefaultEnvironment, StdioClientTransport } from '@modelcontextprotocol/client/stdio';
import { InMemoryTransport, McpServer } from '@modelcontextprotocol/server';
import { BrokerClient } from '../dist/broker/client.js';
import { BrokerFault, operationRoutes } from '../dist/broker/contracts.js';
import { createServer } from '../dist/server.js';
import { createLogger } from '../dist/logger.js';
import { defineTool, registerTools } from '../dist/tools/registry.js';
import { echoExample } from '../dist/examples/echo.js';
import { startRuntime } from '../dist/runtime.js';
import { fakeBroker } from './helpers/fake-broker.mjs';
import { privateSettings } from './helpers/private-settings.mjs';

const silent = { debug() {}, info() {}, error() {} };
const samples = {
  register_agent: { name: 'worker', toolType: 'codex', role: 'MCP implementation' },
  list_agents: {},
  update_agent: { projectName: 'DevLink client', roleDescription: 'MCP 담당' },
  create_task: { recipientAgentId: 'agent-b', title: 'Check interface', description: 'Review draft contract' },
  update_task_status: { taskId: 'task-1', version: '0', status: 'IN_PROGRESS' },
  complete_task: { taskId: 'task-1', version: '1', resultText: 'Done\nVerification: npm test passed', taskFiles: [{ relativePath: 'src/example.ts', changeType: 'MODIFIED', diffText: '- old\n+ new' }] },
  get_tasks: {}, get_task_result: { taskId: 'task-1' }, get_updates: {},
  ask_question: { taskId: 'task-1', message: 'Which API version applies?', requestKey: 'question-1' },
  answer_question: { taskId: 'task-1', questionEventId: '501', message: 'Use v2.', requestKey: 'answer-1' },
};
async function connected(t, { brokerClient, extensions } = {}) {
  const broker = await fakeBroker();
  const adapter = brokerClient ?? new BrokerClient({ settings: broker.settings });
  const server = createServer(silent, adapter, extensions);
  const [serverTransport, clientTransport] = InMemoryTransport.createLinkedPair();
  const client = new Client({ name: 'tool-contract-test', version: '1' }, { versionNegotiation: { mode: 'legacy' } });
  t.after(async () => { await client.close(); await adapter.close?.(); await server.close(); await broker.close(); });
  await Promise.all([server.connect(serverTransport), client.connect(clientTransport)]);
  return { broker, adapter, client };
}
async function invalid(client, name, args) {
  try {
    const result = await client.callTool({ name, arguments: args });
    assert.equal(result.isError, true);
  } catch (error) { assert.equal(error.code, -32602); }
}

test('all eleven business tools use the common invocation/result path and trusted broker context', async (t) => {
  const { client, broker } = await connected(t);
  const listed = await client.listTools();
  assert.equal(listed.tools.length, 14);
  assert.equal(broker.calls.length, 0);
  for (const [name, args] of Object.entries(samples)) {
    const result = await client.callTool({ name, arguments: args });
    assert.equal(result.isError, undefined);
    assert.deepEqual(JSON.parse(result.content[0].text), result.structuredContent);
    assert.equal(typeof result.structuredContent.requestId, 'string');
    const call = broker.calls.findLast((c) => c.path === operationRoutes[name]);
    assert.equal(call.body.context.userId, 'trusted-user');
    assert.equal(call.body.context.teamId, 'trusted-team');
    if (name !== 'register_agent') assert.equal(call.body.context.agentId, 'registered-agent');
    assert.ok(call.body.mcpRequestId !== undefined);
  }
  const getTasks = broker.calls.find((c) => c.path === operationRoutes.get_tasks);
  assert.deepEqual(getTasks.body.arguments, { direction: 'received', limit: 20 });
  const reject = { taskId: 'task-2', version: '0', status: 'REJECTED', rejectionReason: '  out of scope\n범위 밖 ' };
  const rejected = await client.callTool({ name: 'update_task_status', arguments: reject });
  assert.equal(rejected.isError, undefined);
  const forwarded = broker.calls.findLast((c) => c.path === operationRoutes.update_task_status);
  assert.deepEqual(forwarded.body.arguments, reject);
  const question = { taskId: 'task-1', message: '  같은 Task 질문\n원문  ' };
  assert.equal((await client.callTool({ name: 'ask_question', arguments: question })).isError, undefined);
  const asked = broker.calls.findLast((c) => c.path === operationRoutes.ask_question).body.arguments;
  assert.equal(asked.message, question.message);
  assert.match(asked.requestKey, /^[0-9a-f-]{36}$/, 'MCP issues a key so a lost response can be retried');
  const answerArgs = { ...samples.answer_question, message: '가'.repeat(8_000) };
  assert.equal((await client.callTool({ name: 'answer_question', arguments: answerArgs })).isError, undefined);
  assert.deepEqual(broker.calls.findLast((c) => c.path === operationRoutes.answer_question).body.arguments, answerArgs);
  const retry = { ...samples.create_task, requestKey: 'agent-chosen-key' };
  assert.equal((await client.callTool({ name: 'create_task', arguments: retry })).isError, undefined);
  assert.deepEqual(broker.calls.findLast((c) => c.path === operationRoutes.create_task).body.arguments, retry);
});

test('invalid inputs and actor overrides are rejected before any session or broker request', async (t) => {
  const { client, broker } = await connected(t);
  for (const [name, args] of Object.entries(samples)) {
    await invalid(client, name, { ...args, agentId: 'forged-actor' });
    await invalid(client, name, { ...args, userId: 'forged-user' });
    await invalid(client, name, { ...args, context: { teamId: 'forged-team' } });
  }
  for (const [name, args] of [
    ['create_task', { recipientAgentId: '', title: 'x', description: 'x' }],
    ...['', ' key', 'a b', 'k'.repeat(65), 7].map(requestKey => ['create_task', { ...samples.create_task, requestKey }]),
    ['register_agent', { name: 'x', toolType: 'unknown' }],
    ['update_task_status', { taskId: 'task', status: 'COMPLETED' }],
    ['update_task_status', { taskId: 'task', status: 'FAILED' }],
    ['update_task_status', { taskId: 'task', version: '0', status: 'REJECTED' }],
    ['update_task_status', { taskId: 'task', version: '0', status: 'REJECTED', rejectionReason: '   ' }],
    ['update_task_status', { taskId: 'task', version: '0', status: 'REJECTED', rejectionReason: 'no', partiallyModified: false }],
    ['update_task_status', { taskId: 'task', version: '0', status: 'IN_PROGRESS', rejectionReason: 'no' }],
    ['update_task_status', { taskId: 'task', version: '0', status: 'FAILED', failureReason: 'x', partiallyModified: false, rejectionReason: 'no' }],
    ['get_tasks', { limit: 101 }],
    ['get_task_result', { taskId: 1 }],
    ['get_updates', { limit: 51 }],
    ['get_updates', { cursor: '' }],
    ['update_agent', {}],
    ['update_agent', { projectName: '   ' }],
    ['update_agent', { projectName: 'p'.repeat(121) }],
    ['update_agent', { roleDescription: 'r'.repeat(256) }],
    ['update_agent', { roleDescription: '' }],
    ['update_agent', { roleDescription: '' }],
    ['update_agent', { projectName: 'DevLink' }],
    ['update_agent', { roleDescription: 'C1  control' }],
    ['update_agent', { projectName: 'DevLink', name: 'renamed' }],
    ['update_agent', { projectName: 'DevLink', agentId: 'other-agent' }],
    ...['', '   ', 'x'.repeat(8_001), 'nul\0inside'].map(message => ['ask_question', { ...samples.ask_question, message }]),
    ...['0', '01', 'q', 501, '9223372036854775808'].map(questionEventId => ['answer_question', { ...samples.answer_question, questionEventId }]),
    ['answer_question', { taskId: 'task-1', message: 'missing question' }],
    ['ask_question', { ...samples.ask_question, requestKey: 'a b' }],
    ['ask_question', { ...samples.ask_question, questionEventId: '501' }],
    ['complete_task', { taskId: 'task', result: { summary: '' } }],
  ]) await invalid(client, name, args);
  assert.equal(broker.calls.length, 0);
});

test('all business errors use the same safe result envelope and preserve a known task ID', async (t) => {
  let originalId;
  const adapter = { async execute(operation, args, options) { originalId = options.mcpRequestId; throw new BrokerFault('BROKER_REJECTED', 'correlated-request', 'known-task'); } };
  const { client } = await connected(t, { brokerClient: adapter });
  for (const [name, args] of Object.entries(samples)) {
    const result = await client.callTool({ name, arguments: args });
    assert.equal(result.isError, true);
    assert.deepEqual(result.structuredContent.error, { code: 'BROKER_REJECTED', category: 'domain', message: 'The broker rejected the operation.', mcpRequestId: originalId, requestId: 'correlated-request', outcome: 'rejected', taskId: 'known-task', ...(name === 'register_agent' ? { stage: 'protocol', nextActions: ['check_provider_contract'] } : {}) });
    assert.deepEqual(JSON.parse(result.content[0].text), result.structuredContent);
  }
});

test('a functional module extends registration without changing the registry; unsafe failures are hidden', async (t) => {
  const outputSchema = z.strictObject({ message: z.string() });
  const annotations = { readOnlyHint: true };
  const examples = [echoExample, ...['throws_secret', 'invalid_output'].map((name) => defineTool({
    name, description: 'Error fixture', inputSchema: z.strictObject({}), outputSchema, annotations,
    handler: async () => { if (name === 'throws_secret') throw new Error('secret raw internal detail'); return { privateToken: 'secret raw internal detail' }; },
  }))];
  const { client, broker } = await connected(t, { extensions: examples });
  assert.equal((await client.listTools()).tools.length, 17);
  assert.deepEqual((await client.callTool({ name: 'example_echo', arguments: { message: 'hello' } })).structuredContent, { message: 'hello' });
  for (const name of ['throws_secret', 'invalid_output']) {
    const result = await client.callTool({ name, arguments: {} });
    assert.equal(result.structuredContent.error.code, 'TOOL_EXECUTION_FAILED');
    assert.ok(!JSON.stringify(result).includes('secret raw'));
  }
  assert.equal(broker.calls.length, 0);
  const server = new McpServer({ name: 'duplicates', version: '1' });
  assert.throws(() => registerTools(server, [echoExample, echoExample]), /duplicate/);
  await server.close();
});

test('missing broker settings produce a tool error while diagnostics remain usable', async (t) => {
  const { client, broker } = await connected(t, { brokerClient: new BrokerClient() });
  const result = await client.callTool({ name: 'register_agent', arguments: samples.register_agent });
  assert.equal(result.structuredContent.error.code, 'CREDENTIAL_FILE_INVALID');
  assert.equal((await client.callTool({ name: 'devlink_ping', arguments: {} })).structuredContent.status, 'ok');
  assert.equal((await client.listTools()).tools.length, 14);
  assert.equal(broker.calls.length, 0);
});

test('registration faults retain scope/outcome/IDs and provide advice without executing recovery', async (t) => {
  let calls = 0;
  const adapter = { async execute() { calls++; throw new BrokerFault('CENTRAL_RESULT_UNKNOWN', 'original-request', undefined, { scope: 'transport', outcome: 'unknown' }); } };
  const { client } = await connected(t, { brokerClient: adapter });
  const result = await client.callTool({ name: 'register_agent', arguments: samples.register_agent });
  const error = result.structuredContent.error;
  assert.equal(error.requestId, 'original-request'); assert.equal(error.code, 'CENTRAL_RESULT_UNKNOWN');
  assert.equal(error.scope, 'transport'); assert.equal(error.outcome, 'unknown'); assert.equal(error.stage, 'registration');
  assert.deepEqual(error.nextActions, ['inspect_owned_agents_then_use']); assert.equal(calls, 1);
});

const entry = fileURLToPath(new URL('../dist/index.js', import.meta.url));
for (const mode of ['legacy', { pin: '2026-07-28' }]) {
  test(`stdio SDK → private descriptor → fake broker, protocol ${JSON.stringify(mode)}`, { timeout: 15000 }, async (t) => {
    const broker = await fakeBroker();
    const file = await privateSettings(broker.settings);
    const client = new Client({ name: 'stdio-broker-test', version: '1' }, { versionNegotiation: { mode } });
    const transport = new StdioClientTransport({ command: process.execPath, args: [entry, '--transport', 'stdio'],
      env: { ...getDefaultEnvironment(), DEVLINK_BROKER_FILE: file.path }, stderr: 'pipe' });
    let logs = '';
    transport.stderr.on('data', (chunk) => { logs += chunk; });
    t.after(async () => { await client.close(); try { await file.cleanup(); } finally { await broker.close(); } });
    await client.connect(transport);
    assert.equal((await client.listTools()).tools.length, 14);
    await invalid(client, 'create_task', { ...samples.create_task, teamId: 'forged' });
    await client.callTool({ name: 'devlink_ping', arguments: {} });
    assert.equal(broker.calls.length, 0);
    for (const [name, args] of Object.entries(samples)) {
      const result = await client.callTool({ name, arguments: args });
      assert.equal(result.isError, undefined);
      assert.ok(result.structuredContent.requestId);
    }
    await client.close();
    assert.ok(!logs.includes(broker.settings.localToken));
    assert.ok(!logs.includes('trusted-user'));
    assert.ok(!logs.includes('Review draft contract'));
  });
}

test('stdio EOF cancels an in-flight tool and releases the shared broker within the shutdown deadline', { timeout: 5000 }, async (t) => {
  const broker = await fakeBroker();
  const input = new PassThrough();
  const output = new PassThrough();
  const logs = new PassThrough();
  logs.resume();
  const runtime = startRuntime({ transport: 'stdio', logLevel: 'error' }, { input, output,
    logger: createLogger(logs, 'error'), brokerOptions: { settings: { ...broker.settings, agentId: 'a' }, shutdownTimeoutMs: 50 } });
  t.after(async () => { await runtime.close(); input.destroy(); output.destroy(); logs.destroy(); await broker.close(); });
  let buffer = '';
  const responses = new Map();
  output.on('data', (chunk) => {
    buffer += chunk;
    let newline;
    while ((newline = buffer.indexOf('\n')) >= 0) {
      const message = JSON.parse(buffer.slice(0, newline));
      buffer = buffer.slice(newline + 1);
      responses.get(message.id)?.(message);
    }
  });
  let nextId = 0;
  const request = (method, params) => {
    const id = ++nextId;
    const result = new Promise((resolve) => responses.set(id, resolve));
    input.write(`${JSON.stringify({ jsonrpc: '2.0', id, method, params })}\n`);
    return result;
  };
  await request('initialize', { protocolVersion: '2025-11-25', capabilities: {}, clientInfo: { name: 'close-test', version: '1' } });
  input.write(`${JSON.stringify({ jsonrpc: '2.0', method: 'notifications/initialized' })}\n`);
  await request('tools/call', { name: 'get_tasks', arguments: {} });
  let reached;
  const received = new Promise((resolve) => { reached = resolve; });
  broker.setHook(({ path, req }) => {
    if (path === operationRoutes.create_task) { reached(); return true; }
    return req.method === 'DELETE';
  });
  void request('tools/call', { name: 'create_task', arguments: samples.create_task });
  await received;
  const started = Date.now();
  input.end();
  await runtime.closed;
  assert.ok(Date.now() - started < 1000);
});
