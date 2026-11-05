import assert from 'node:assert/strict';
import test from 'node:test';
import { Client } from '@modelcontextprotocol/client';
import { InMemoryTransport } from '@modelcontextprotocol/server';
import { createServer } from '../dist/server.js';
import { BrokerClient } from '../dist/broker/client.js';
import { fakeBroker } from './helpers/fake-broker.mjs';

async function connect(t) {
  const broker = await fakeBroker();
  const adapter = new BrokerClient({ settings: { ...broker.settings, agentId: 'registered-agent' } });
  const server = createServer({ debug() {}, info() {}, error() {} }, adapter);
  const client = new Client({ name: 'central-contract', version: '1' }, { versionNegotiation: { mode: 'legacy' } });
  const [a, b] = InMemoryTransport.createLinkedPair();
  t.after(async () => { await client.close(); await adapter.close(); await server.close(); await broker.close(); });
  await Promise.all([server.connect(a), client.connect(b)]);
  return { broker, client };
}

test('published correlation key schema advertises the actual 64-unit limit', async (t) => {
  const { client, broker } = await connect(t);
  const tools = (await client.listTools()).tools;
  for (const name of ['create_task', 'update_task_status', 'complete_task', 'get_task_result']) {
    assert.equal(tools.find(tool => tool.name === name).inputSchema.properties.requestKey.maxLength, 64);
  }
  assert.equal(broker.calls.length, 0);
});

test('SDK preserves Long versions, completion text, verification notes, file types and diff text on the wire', async (t) => {
  const { client, broker } = await connect(t);
  const args = { taskId: '9223372036854775807', version: '9007199254740993', requestKey: 'creation-key',
    resultText: '  요약\n검증: npm test 통과\n미실행: 운영 배포  ', taskFiles: [
      { relativePath: 'src/한글.ts', changeType: 'MODIFIED', diffText: '  - old\n+ 변경\n' },
      { relativePath: 'deleted.ts', changeType: 'DELETED', diffText: null },
    ] };
  const result = await client.callTool({ name: 'complete_task', arguments: args });
  assert.notEqual(result.isError, true);
  assert.deepEqual(broker.calls.findLast(c => c.body.operation === 'complete_task').body.arguments, args);
  const failed = { taskId: '101', version: '1', status: 'FAILED', failureReason: '  실패 원문\n  ', partiallyModified: false };
  assert.notEqual((await client.callTool({ name: 'update_task_status', arguments: failed })).isError, true);
  assert.deepEqual(broker.calls.at(-1).body.arguments, failed);
  assert.notEqual((await client.callTool({ name: 'get_tasks', arguments: { status: 'REJECTED' } })).isError, true);
  assert.equal(broker.calls.at(-1).body.arguments.status, 'REJECTED');
  const metadata = { name: '  CLI Agent  ', toolType: 'codex', role: '  Exact role\n  ' };
  assert.notEqual((await client.callTool({ name: 'register_agent', arguments: metadata })).isError, true);
  assert.deepEqual(broker.calls.findLast(c => c.body.operation === 'register_agent').body.arguments, metadata);
});

test('SDK rejects legacy, missing, unsafe or status-inconsistent mutation inputs before broker I/O', async (t) => {
  const { client, broker } = await connect(t);
  const status = { taskId: '101', version: '0', status: 'IN_PROGRESS' };
  const complete = { taskId: '101', version: '1', resultText: 'Done' };
  for (const [name, args] of [
    ['update_task_status', { taskId: '101', status: 'IN_PROGRESS' }],
    ...['01', '-1', '1e2', 'invalid', '9223372036854775808', Number.MAX_SAFE_INTEGER + 1].map(version => ['update_task_status', { ...status, version }]),
    ['update_task_status', { ...status, status: 'FAILED', failureReason: 'failed' }],
    ['update_task_status', { ...status, status: 'FAILED', partiallyModified: false }],
    ['update_task_status', { ...status, partiallyModified: false }],
    ['complete_task', { taskId: '101', result: { summary: 'legacy', changedFiles: ['unknown-type.ts'], verification: 'must not be lost' } }],
    ['complete_task', { ...complete, verification: 'unsupported separate field' }],
    ['complete_task', { ...complete, taskFiles: [{ relativePath: 'src/a.ts' }] }],
    ['complete_task', { ...complete, resultText: '  \n ' }],
    ...['accept 101', 'tab\tkey', 'key\u00a0value', 'x'.repeat(65)].map(requestKey => ['update_task_status', { ...status, requestKey }]),
  ]) {
    try { assert.equal((await client.callTool({ name, arguments: args })).isError, true, name); }
    catch (error) { assert.equal(error.code, -32602); }
  }
  assert.equal(broker.calls.length, 0);
});

test('central version conflicts and protocol uncertainty retain codes, IDs and keys without retry', async (t) => {
  const { client, broker } = await connect(t);
  for (const [code, scope, status, outcome] of [
    ['TASK_VERSION_CONFLICT', 'domain', 409, 'rejected'],
    ['RESOURCE_NOT_FOUND', 'domain', 404, 'rejected'],
    ['REQUEST_KEY_CONFLICT', 'domain', 409, 'unknown'],
    ['BROKER_STORAGE_FAILED', 'domain', 503, 'rejected'],
    ['CENTRAL_PROTOCOL_ERROR', 'transport', 502, 'unknown'],
    ['CENTRAL_RESULT_UNKNOWN', 'transport', 503, 'unknown'],
  ]) {
    broker.setHook(({ body, send }) => {
      if (body.operation !== 'update_task_status') return false;
      send({ requestKey: 'known-key', error: { code, scope, outcome, taskId: '101', message: 'secret provider detail' } }, status);
      return true;
    });
    const count = broker.calls.filter(c => c.body.operation === 'update_task_status').length;
    const result = await client.callTool({ name: 'update_task_status', arguments: { taskId: '101', version: '0', status: 'IN_PROGRESS', requestKey: 'known-key' } });
    assert.equal(result.isError, true);
    assert.equal(result.structuredContent.error.code, code);
    assert.equal(result.structuredContent.error.outcome, outcome);
    assert.equal(result.structuredContent.error.taskId, '101');
    assert.equal(result.structuredContent.error.requestKey, 'known-key');
    assert.ok(!JSON.stringify(result).includes('secret provider detail'));
    assert.equal(broker.calls.filter(c => c.body.operation === 'update_task_status').length, count + 1);
  }
});
