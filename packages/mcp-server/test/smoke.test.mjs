import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { PassThrough } from 'node:stream';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { Client } from '@modelcontextprotocol/client';
import { getDefaultEnvironment, StdioClientTransport } from '@modelcontextprotocol/client/stdio';
import { createLogger } from '../dist/logger.js';
import { startRuntime } from '../dist/runtime.js';

const entry = fileURLToPath(new URL('../dist/index.js', import.meta.url));
const baseEnv = getDefaultEnvironment();
const configEnv = { ...baseEnv, DEVLINK_MCP_TRANSPORT: 'stdio', DEVLINK_MCP_LOG_LEVEL: 'debug' };
const deadline = (promise, ms = 5000) => {
  let timer;
  return Promise.race([
    promise,
    new Promise((_, reject) => { timer = setTimeout(() => reject(new Error('test deadline exceeded')), ms); }),
  ]).finally(() => clearTimeout(timer));
};

function startChild(t, { env = configEnv, args = [] } = {}) {
  const child = spawn(process.execPath, [entry, ...args], { env, windowsHide: true });
  let stdout = '';
  let stderr = '';
  child.stdout.setEncoding('utf8').on('data', (data) => { stdout += data; });
  child.stderr.setEncoding('utf8').on('data', (data) => { stderr += data; });
  const exited = once(child, 'exit');
  const finished = once(child, 'close');
  t.after(async () => {
    if (child.exitCode === null && child.signalCode === null) child.kill();
    await deadline(finished);
  });
  return { child, exited, finished, stdout: () => stdout, stderr: () => stderr };
}

function rawClient(session) {
  let buffer = '';
  let nextId = 0;
  const waiting = new Map();
  session.child.stdout.on('data', (chunk) => {
    buffer += chunk;
    let newline;
    while ((newline = buffer.indexOf('\n')) !== -1) {
      const line = buffer.slice(0, newline);
      buffer = buffer.slice(newline + 1);
      try {
        const message = JSON.parse(line);
        assert.equal(message.jsonrpc, '2.0');
        if (waiting.has(message.id)) {
          waiting.get(message.id).resolve(message);
          waiting.delete(message.id);
        }
      } catch (error) {
        for (const waiter of waiting.values()) waiter.reject(error);
        waiting.clear();
      }
    }
  });
  return {
    request(method, params = {}) {
      const id = ++nextId;
      const response = new Promise((resolve, reject) => waiting.set(id, { resolve, reject }));
      session.child.stdin.write(`${JSON.stringify({ jsonrpc: '2.0', id, method, params })}\n`);
      return deadline(response);
    },
    notify(method) {
      session.child.stdin.write(`${JSON.stringify({ jsonrpc: '2.0', method })}\n`);
    },
  };
}

for (const versionNegotiation of [{ mode: 'legacy' }, { mode: { pin: '2026-07-28' } }]) {
  test(`SDK client lists and calls the tool, then restarts: ${JSON.stringify(versionNegotiation.mode)}`, { timeout: 15000 }, async (t) => {
    for (let run = 0; run < 2; run++) {
      const transport = new StdioClientTransport({
        command: process.execPath, args: [entry], env: configEnv, stderr: 'pipe',
      });
      let stderr = '';
      transport.stderr.on('data', (data) => { stderr += data; });
      const client = new Client({ name: 'devlink-smoke', version: '1.0.0' }, { versionNegotiation });
      const errors = [];
      client.onerror = (error) => errors.push(error);
      t.after(() => client.close());
      await deadline(client.connect(transport));
      assert.deepEqual(client.getServerVersion(), { name: 'devlink-mcp', version: '0.1.3' });
      const { tools } = await client.listTools();
      assert.deepEqual(tools.map(({ name }) => name), ['devlink_ping', 'register_agent', 'list_agents', 'update_agent', 'find_agent', 'create_task', 'update_task_status', 'complete_task', 'get_tasks', 'get_task_result', 'get_team_context', 'ask_question', 'answer_question', 'get_updates', 'get_my_context', 'get_connection_status']);
      assert.equal(tools[0].annotations.readOnlyHint, true);
      const result = await client.callTool({ name: 'devlink_ping', arguments: {} });
      assert.deepEqual(result.structuredContent, { status: 'ok', version: '0.1.3' });
      assert.deepEqual(JSON.parse(result.content[0].text), result.structuredContent);
      await client.close();
      assert.deepEqual(errors, []);
      assert.ok(stderr.includes('tool.ping'));
      assert.ok(stderr.includes('runtime.closed'));
    }
  });
}

test('missing configuration exits nonzero with no protocol output', { timeout: 10000 }, async (t) => {
  const session = startChild(t, { env: baseEnv });
  const [code, signal] = await deadline(session.exited);
  await session.finished;
  assert.equal(code, 1);
  assert.equal(signal, null);
  assert.equal(session.stdout(), '');
  const entry = JSON.parse(session.stderr().trim());
  assert.equal(entry.code, 'CONFIG_INVALID');
  assert.deepEqual(entry.fields, ['transport']);
});

test('invalid settings and unknown flags never echo values or credentials', { timeout: 10000 }, async (t) => {
  const secret = 'do-not-log-this-token';
  for (const settings of [
    { env: { ...configEnv, DEVLINK_MCP_LOG_LEVEL: secret, ACCESS_TOKEN: secret } },
    { env: configEnv, args: [`--${secret}`] },
  ]) {
    const session = startChild(t, settings);
    assert.equal((await deadline(session.exited))[0], 1);
    await session.finished;
    assert.equal(session.stdout(), '');
    assert.ok(!session.stderr().includes(secret));
    assert.equal(JSON.parse(session.stderr().trim()).code, 'CONFIG_INVALID');
  }
});

test('logs stay on stderr and EOF exits normally without a forced kill', { timeout: 15000 }, async (t) => {
  const secret = 'private-test-identity';
  const session = startChild(t, { env: { ...configEnv, ACCESS_TOKEN: secret, DEVLINK_AGENT_ID: secret } });
  const client = rawClient(session);
  const initialized = await client.request('initialize', {
    protocolVersion: '2025-11-25', capabilities: {}, clientInfo: { name: 'wire-audit', version: '1.0.0' },
  });
  assert.ok(initialized.result.capabilities.tools);
  client.notify('notifications/initialized');
  const listed = await client.request('tools/list');
  assert.equal(listed.result.tools[0].name, 'devlink_ping');
  for (let i = 0; i < 30; i++) {
    const called = await client.request('tools/call', { name: 'devlink_ping', arguments: {} });
    assert.equal(called.result.structuredContent.status, 'ok');
  }
  session.child.stdin.end();
  const [code, signal] = await deadline(session.exited);
  await session.finished;
  assert.equal(code, 0);
  assert.equal(signal, null);
  const messages = session.stdout().trim().split('\n').map((line) => JSON.parse(line));
  assert.equal(messages.length, 32);
  assert.ok(messages.every((message) => message.jsonrpc === '2.0'));
  const logs = session.stderr().trim().split('\n').map((line) => JSON.parse(line));
  assert.equal(logs.filter(({ event }) => event === 'tool.ping').length, 30);
  assert.equal(logs.at(-1).event, 'runtime.closed');
  assert.ok(!(session.stdout() + session.stderr()).includes(secret));
});

test('EOF before initialization also releases the unused server and allows restart', { timeout: 10000 }, async (t) => {
  for (let run = 0; run < 2; run++) {
    const session = startChild(t);
    session.child.stdin.end();
    assert.deepEqual(await deadline(session.exited), [0, null]);
    await session.finished;
    assert.equal(session.stdout(), '');
    assert.ok(session.stderr().includes('runtime.closed'));
  }
});

test('explicit close is idempotent and removes owned stdin listeners', { timeout: 10000 }, async () => {
  const input = new PassThrough();
  const output = new PassThrough();
  const stderr = new PassThrough();
  let logs = '';
  stderr.on('data', (data) => { logs += data; });
  const listeners = ['data', 'end', 'close', 'error'].map((event) => input.listenerCount(event));
  const runtime = startRuntime({ transport: 'stdio', logLevel: 'debug' }, {
    input, output, logger: createLogger(stderr, 'debug'),
  });
  await Promise.all([runtime.close(), runtime.close(), runtime.closed]);
  assert.deepEqual(['data', 'end', 'close', 'error'].map((event) => input.listenerCount(event)), listeners);
  assert.equal(logs.split('\n').filter((line) => line.includes('runtime.closed')).length, 1);
  input.destroy();
  output.destroy();
  stderr.destroy();
});
