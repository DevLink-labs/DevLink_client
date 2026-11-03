import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';
import { Client } from '@modelcontextprotocol/client';
import { InMemoryTransport } from '@modelcontextprotocol/server';
import { createServer } from '../dist/server.js';

const examples = JSON.parse(await readFile(new URL('../examples/workflow.json', import.meta.url), 'utf8'));
const guide = await readFile(new URL('../docs/agent-workflow.md', import.meta.url), 'utf8');

test('published workflow JSON blocks match the reusable tool examples', () => {
  const blocks = [...guide.matchAll(/<!-- example:(\w+) -->\s*```json\r?\n([\s\S]*?)\r?\n```/g)];
  assert.equal(blocks.length, 8);
  const names = new Set();
  for (const [, name, json] of blocks) {
    assert.ok(!names.has(name), `Duplicate document example ${name}`); names.add(name);
    assert.ok(examples.calls[name], `Unknown document example ${name}`);
    assert.deepEqual(JSON.parse(json), examples.calls[name].arguments, `Stale document example ${name}`);
  }
});

test('every workflow example is accepted by the actual registered MCP tool schema', async (t) => {
  const observed = [];
  const adapter = { async execute(operation, args, options) {
    observed.push({ operation, args });
    return { requestId: `example-${observed.length}`, mcpRequestId: options.mcpRequestId, data: { operation, arguments: args } };
  } };
  const server = createServer({ debug() {}, info() {}, error() {} }, adapter);
  const [serverTransport, clientTransport] = InMemoryTransport.createLinkedPair();
  const client = new Client({ name: 'workflow-example-schema-test', version: '1' }, { versionNegotiation: { mode: 'legacy' } });
  t.after(async () => { await client.close(); await server.close(); });
  await Promise.all([server.connect(serverTransport), client.connect(clientTransport)]);
  const catalog = (await client.listTools()).tools.map((tool) => tool.name);
  for (const call of Object.values(examples.calls)) {
    assert.ok(catalog.includes(call.name), `Missing registered tool ${call.name}`);
    const response = await client.callTool(call);
    assert.notEqual(response.isError, true, `Invalid ${call.name} example`);
    assert.deepEqual(response.structuredContent.data, { operation: call.name, arguments: call.arguments });
    assert.deepEqual(JSON.parse(response.content[0].text), response.structuredContent);
  }
  assert.equal(observed.length, Object.keys(examples.calls).length);
  assert.equal(new Set(observed.map((call) => call.operation)).size, 7);
});

test('workflow result examples carry all MVP reporting information even when DTO fields are optional', () => {
  const done = examples.calls.complete.arguments;
  assert.ok(done.resultText.includes('검증:')); assert.equal(typeof done.version, 'string');
  assert.ok(Array.isArray(done.taskFiles) && done.taskFiles.length > 0);
  assert.ok(done.taskFiles.every(file => file.relativePath && file.changeType));
  const failed = examples.calls.fail.arguments;
  assert.equal(failed.status, 'FAILED'); assert.ok(failed.failureReason.trim()); assert.equal(typeof failed.partiallyModified, 'boolean');
  assert.equal(examples.calls.accept.arguments.status, 'IN_PROGRESS');
});
