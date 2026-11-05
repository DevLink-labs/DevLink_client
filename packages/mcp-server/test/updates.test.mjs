import assert from 'node:assert/strict';
import test from 'node:test';
import { Client } from '@modelcontextprotocol/client';
import { InMemoryTransport } from '@modelcontextprotocol/server';
import { BrokerClient } from '../dist/broker/client.js';
import { operationRoutes } from '../dist/broker/contracts.js';
import { createServer } from '../dist/server.js';
import { fakeBroker } from './helpers/fake-broker.mjs';

const silent = { debug() {}, info() {}, error() {} };
const cursor = (r, s, m) => `v2.${r}.${s}.${m}.${'a'.repeat(32)}`;
const feed = {
  updates: [
    { eventId: 'created-1', kind: 'TASK_CREATED', direction: 'received', taskId: '101', requesterAgentId: '11' },
    { eventId: 'status-1', kind: 'TASK_STATUS_CHANGED', direction: 'sent', taskId: '201', taskVersion: '1', status: 'REJECTED' },
    { eventId: 'done-1', kind: 'TASK_COMPLETED', direction: 'sent', taskId: '201', taskVersion: '2', status: 'COMPLETED' },
    { eventId: '502', kind: 'TASK_QUESTION_CREATED', direction: 'received', taskId: '300', sequence: '13' },
    { eventId: '503', kind: 'TASK_ANSWER_CREATED', direction: 'received', taskId: '300', sequence: '14' },
  ],
  nextCursor: cursor(1, 2, 2), hasMore: false, cursorReset: false, coverage: 'local',
};

async function connected(t) {
  const broker = await fakeBroker();
  const adapter = new BrokerClient({ settings: broker.settings });
  const server = createServer(silent, adapter);
  const [serverTransport, clientTransport] = InMemoryTransport.createLinkedPair();
  const client = new Client({ name: 'updates-test', version: '1' }, { versionNegotiation: { mode: 'legacy' } });
  t.after(async () => { await client.close(); await adapter.close(); await server.close(); await broker.close(); });
  await Promise.all([server.connect(serverTransport), client.connect(clientTransport)]);
  assert.equal((await client.callTool({ name: 'register_agent', arguments: {} })).isError, undefined);
  return { broker, client };
}

test('get_updates is published read-only and forwards only cursor and limit', async (t) => {
  const { broker, client } = await connected(t);
  const tool = (await client.listTools()).tools.find(({ name }) => name === 'get_updates');
  assert.equal(tool.annotations.readOnlyHint, true);
  assert.equal(tool.annotations.destructiveHint, false);
  assert.match(tool.description, /never accepts, acknowledges, answers, marks read or changes a task/);
  assert.match(tool.description, /not the user's approval to start work or to answer a question/);
  broker.setBusinessHook(({ path, send }) => path === operationRoutes.get_updates && (send({ data: feed }), true));
  const first = await client.callTool({ name: 'get_updates', arguments: {} });
  assert.equal(first.isError, undefined);
  assert.deepEqual(first.structuredContent.data, feed);
  await client.callTool({ name: 'get_updates', arguments: { cursor: feed.nextCursor, limit: 5 } });
  const calls = broker.calls.filter(({ path }) => path === '/v1/updates').map(({ body }) => body);
  assert.deepEqual(calls.map(({ operation, arguments: args }) => ({ operation, args })), [
    { operation: 'get_updates', args: { limit: 20 } },
    { operation: 'get_updates', args: { cursor: feed.nextCursor, limit: 5 } },
  ]);
  for (const body of calls) assert.equal(body.context.agentId, 'registered-agent');
});

test('provider feeds that leak bodies or break the contract become protocol errors', async (t) => {
  const { broker, client } = await connected(t);
  const leaked = structuredClone(feed); leaked.updates[0].prompt = 'secret prompt text';
  const wrongState = structuredClone(feed); wrongState.updates[2].status = 'FAILED';
  const notLocal = { ...feed, coverage: 'complete' };
  const badCursor = { ...feed, nextCursor: 'v1.0.0.short' };
  const leakedQuestion = structuredClone(feed); leakedQuestion.updates[3].message = 'secret prompt text';
  const sentQuestion = structuredClone(feed); sentQuestion.updates[3].direction = 'sent';
  const badSequence = structuredClone(feed); badSequence.updates[4].sequence = '0';
  const v2WithoutMessages = { ...feed, nextCursor: `v2.1.2.${'a'.repeat(32)}` };
  for (const data of [leaked, wrongState, notLocal, badCursor, leakedQuestion, sentQuestion, badSequence, v2WithoutMessages]) {
    broker.setBusinessHook(({ send }) => (send({ data }), true));
    const result = await client.callTool({ name: 'get_updates', arguments: {} });
    assert.equal(result.isError, true);
    assert.equal(result.structuredContent.error.code, 'BROKER_PROTOCOL_ERROR');
    assert.ok(!JSON.stringify(result).includes('secret prompt text'));
  }
});

test('an unreadable local store maps to its own rejected domain fault', async (t) => {
  const { broker, client } = await connected(t);
  broker.setBusinessHook(({ send }) => (send({ error: { code: 'LOCAL_UPDATES_UNAVAILABLE', scope: 'domain', outcome: 'rejected' } }, 503), true));
  const result = await client.callTool({ name: 'get_updates', arguments: {} });
  assert.equal(result.isError, true);
  assert.equal(result.structuredContent.error.code, 'LOCAL_UPDATES_UNAVAILABLE');
  assert.equal(result.structuredContent.error.outcome, 'rejected');
  assert.match(result.structuredContent.error.message, /get_tasks or get_task_result/);
});

test('a v1 cursor from an older broker is still a valid page cursor', async (t) => {
  const { broker, client } = await connected(t);
  const legacy = { ...feed, updates: feed.updates.slice(0, 3), nextCursor: `v1.1.2.${'b'.repeat(32)}` };
  broker.setBusinessHook(({ send }) => (send({ data: legacy }), true));
  const result = await client.callTool({ name: 'get_updates', arguments: { cursor: legacy.nextCursor } });
  assert.equal(result.isError, undefined);
  assert.deepEqual(result.structuredContent.data, legacy);
});
