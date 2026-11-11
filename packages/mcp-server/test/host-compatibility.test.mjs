import assert from 'node:assert/strict';
import test from 'node:test';
import { readHostTranscript, parseHostAnswer, hostConfiguration, hostTools } from './helpers/host-transcript.mjs';
import { verifyCalls, verifyWire } from '../scripts/verify-hosts.mjs';
import { captureHost } from './helpers/host-process.mjs';
import { fakeBroker } from './helpers/fake-broker.mjs';
import { BrokerHttp } from '../dist/broker/http.js';
import { hostFixture } from './helpers/host-fixture.mjs';

const jsonl = (events) => events.map((event) => JSON.stringify(event)).join('\n');
const success = { requestId: 'http-call', mcpRequestId: 0, requestKey: 'broker-key', data: { taskId: 'task', status: 'PENDING', delivery: { status: 'UNKNOWN' } } };
const toolResult = (value, snake = false) => ({ content: [{ type: 'text', text: JSON.stringify(value) }], [snake ? 'structured_content' : 'structuredContent']: value });
const steps = [{ tool: 'create_task', args: { recipientAgentId: 'recipient' }, expect: 'PENDING' }];

test('Host rotation fixture clears volatile ownership and preserves the Agent under recover', async (t) => {
  const fixture = await hostFixture();const old = fixture.profile('codex');
  const settings = profile => ({ schemaVersion: 1, brokerUrl: fixture.broker.settings.brokerUrl, localToken: profile.localToken });
  const before = new BrokerHttp(settings(old));let after;
  t.after(async () => { before.close(); after?.close(); await fixture.broker.close(); });
  await before.request('/v1/sessions', { instanceId: 'old-host', mode: 'recover', agentId: old.agentId });
  const rotated = fixture.rotate('codex');assert.equal(rotated.agentId, old.agentId);
  after = new BrokerHttp(settings(rotated));
  const recovered = await after.request('/v1/sessions', { instanceId: 'new-host', mode: 'recover', agentId: rotated.agentId });
  assert.equal(recovered.principal.agentId, old.agentId);assert.equal(fixture.broker.owners.size, 1);
});

test('Codex JSONL uses actual completed MCP events and snake-case structured results', () => {
  const transcript = readHostTranscript('codex', jsonl([
    { type: 'item.started', item: { id: '1', type: 'mcp_tool_call', server: 'devlink', tool: 'create_task', arguments: { recipientAgentId: 'recipient' } } },
    { type: 'item.completed', item: { id: '1', type: 'mcp_tool_call', server: 'devlink', tool: 'create_task', arguments: { recipientAgentId: 'recipient' }, result: toolResult(success, true) } },
    { type: 'item.completed', item: { type: 'agent_message', text: '{"ok":true}' } }, { type: 'turn.completed' },
  ]));
  assert.equal(transcript.calls.length, 1); assert.deepEqual(transcript.calls[0].value, success);
  assert.equal(verifyCalls(transcript, steps)[0].passed, true);
  assert.deepEqual(parseHostAnswer(transcript.finalText), { ok: true });
});

test('Claude tool-use IDs correlate real results, catalog and safe fault metadata', () => {
  const fault = { error: { code: 'BROKER_TIMEOUT', outcome: 'unknown', requestId: 'http-call', mcpRequestId: 'host-id', taskId: 'task' } };
  const transcript = readHostTranscript('claude', jsonl([
    { type: 'system', subtype: 'init', model: 'claude-opus-5-5', tools: hostTools.map((name) => `mcp__devlink__${name}`) },
    { type: 'assistant', message: { content: [{ type: 'tool_use', id: 'use-1', name: 'mcp__devlink__get_task_result', input: { taskId: 'task' } }] } },
    { type: 'user', message: { content: [{ type: 'tool_result', tool_use_id: 'use-1', content: JSON.stringify(fault), is_error: true }] }, tool_use_result: toolResult(fault) },
    { type: 'result', is_error: false, result: '{"ok":true}' },
  ]));
  assert.deepEqual(transcript.calls[0].value, fault); assert.deepEqual(transcript.catalog, hostTools);
  assert.equal(transcript.model, 'claude-opus-5-5');
});

test('model prose, wrong recipients, incomplete calls and foreign tools cannot pass compatibility checks', () => {
  const prose = readHostTranscript('codex', jsonl([{ type: 'item.completed', item: { type: 'agent_message', text: 'All tools passed' } }, { type: 'turn.completed' }]));
  assert.throws(() => verifyCalls(prose, steps));
  assert.throws(() => verifyCalls({ calls: [{ tool: 'create_task', args: { recipientAgentId: 'wrong' }, value: success }] }, steps));
  assert.throws(() => readHostTranscript('codex', jsonl([{ type: 'item.started', item: { id: '1', type: 'mcp_tool_call', server: 'devlink', tool: 'get_tasks' } }, { type: 'turn.completed' }])));
  assert.throws(() => readHostTranscript('codex', jsonl([{ type: 'item.completed', item: { type: 'command_execution' } }, { type: 'turn.completed' }])));
  assert.throws(() => readHostTranscript('claude', jsonl([{ type: 'assistant', message: { content: [{ type: 'tool_use', id: '1', name: 'Read', input: {} }] } }, { type: 'result', is_error: false }])));
});

test('a task version is compared by value across its string and integer forms, and nothing else is', () => {
  const versioned = [{ tool: 'update_task_status', args: { taskId: 'task', version: '0', status: 'IN_PROGRESS' }, expect: 'success' }];
  const sent = (args) => ({ calls: [{ tool: 'update_task_status', args, value: success }] });
  assert.deepEqual(verifyCalls(sent({ taskId: 'task', version: 0, status: 'IN_PROGRESS' }), versioned).map((check) => check.versionType), ['number']);
  assert.deepEqual(verifyCalls(sent({ taskId: 'task', version: '0', status: 'IN_PROGRESS' }), versioned).map((check) => check.versionType), ['string']);
  assert.equal('versionType' in verifyCalls({ calls: [{ tool: 'create_task', args: { recipientAgentId: 'recipient' }, value: success }] }, steps)[0], false);
  for (const version of [1, '1', '00', '0.0', 0.5, -1, true, null, [0], Number.MAX_SAFE_INTEGER + 1]) {
    assert.throws(() => verifyCalls(sent({ taskId: 'task', version, status: 'IN_PROGRESS' }), versioned), /Wrong arguments at step 1/, String(version));
  }
  // Numbers outside what the server accepts stay as they are, even when their text equals the instruction.
  for (const [instructed, received] of [['-1', -1], ['9007199254740992', Number.MAX_SAFE_INTEGER + 1], ['0.5', 0.5]]) {
    const step = [{ ...versioned[0], args: { ...versioned[0].args, version: instructed } }];
    assert.throws(() => verifyCalls(sent({ taskId: 'task', version: received, status: 'IN_PROGRESS' }), step), /Wrong arguments at step 1/, instructed);
  }
  assert.throws(() => verifyCalls(sent({ taskId: 'task', status: 'IN_PROGRESS' }), versioned), /Wrong arguments at step 1/);
  assert.throws(() => verifyCalls(sent({ taskId: 0, version: '0', status: 'IN_PROGRESS' }), [{ ...versioned[0], args: { ...versioned[0].args, taskId: '0' } }]), /Wrong arguments at step 1/);
});

test('mismatched text/structured results and orphaned Claude results are rejected', () => {
  assert.throws(() => readHostTranscript('codex', jsonl([{ type: 'item.completed', item: { id: '1', type: 'mcp_tool_call', server: 'devlink', tool: 'get_tasks', result: { content: [{ text: '{}' }], structured_content: success } } }, { type: 'turn.completed' }])));
  assert.throws(() => readHostTranscript('claude', jsonl([{ type: 'user', message: { content: [{ type: 'tool_result', tool_use_id: 'missing', content: '{}' }] } }, { type: 'result', is_error: false }])));
});

test('SDK input validation is distinct from structured execution faults in both Host encodings', () => {
  for (const host of ['codex', 'claude']) {
    const native = { isError: true, content: [{ type: 'text', text: 'Input validation error: failure reason is required.' }] };
    const events = host === 'codex' ? [
      { type: 'item.completed', item: { id: '1', type: 'mcp_tool_call', server: 'devlink', tool: 'update_task_status', arguments: { taskId: 'task', status: 'FAILED' }, result: native } }, { type: 'turn.completed' },
    ] : [
      { type: 'assistant', message: { content: [{ type: 'tool_use', id: '1', name: 'mcp__devlink__update_task_status', input: { taskId: 'task', status: 'FAILED' } }] } },
      { type: 'user', message: { content: [{ type: 'tool_result', tool_use_id: '1', content: native.content[0].text, is_error: true }] }, tool_use_result: native.content[0].text }, { type: 'result', is_error: false },
    ];
    const parsed = readHostTranscript(host, jsonl(events));
    assert.equal(parsed.calls[0].inputError, true); assert.equal(parsed.calls[0].value, undefined);
  }
});

test('observed Host error wrappers retain native input errors and model-visible JSON faults', () => {
  const codex = readHostTranscript('codex', jsonl([
    { type: 'item.completed', item: { id: '1', type: 'mcp_tool_call', server: 'devlink', tool: 'update_task_status', status: 'failed', error: null,
      result: { content: [{ type: 'text', text: 'Input validation error: reason required.' }], structured_content: null } } }, { type: 'turn.completed' },
  ]));
  assert.equal(codex.calls[0].inputError, true); assert.equal(codex.calls[0].value, undefined);
  assert.equal(codex.calls[0].representation, 'native-error');
  const fault = { error: { code: 'BROKER_FORBIDDEN', outcome: 'rejected' } };
  const claude = readHostTranscript('claude', jsonl([
    { type: 'assistant', message: { content: [{ type: 'tool_use', id: '1', name: 'mcp__devlink__get_tasks', input: {} }] } },
    { type: 'user', message: { content: [{ type: 'tool_result', tool_use_id: '1', content: JSON.stringify(fault), is_error: true }] },
      tool_use_result: { content: `Error: ${JSON.stringify(fault)}` } }, { type: 'result', is_error: false },
  ]));
  assert.deepEqual(claude.calls[0].value, fault); assert.equal(claude.calls[0].representation, 'json-text');
});

test('tool event overlap is rejected, while final fenced JSON can follow Host commentary', () => {
  assert.throws(() => readHostTranscript('codex', jsonl([
    { type: 'item.started', item: { id: '1', type: 'mcp_tool_call', server: 'devlink', tool: 'get_tasks', arguments: {} } },
    { type: 'item.started', item: { id: '2', type: 'mcp_tool_call', server: 'devlink', tool: 'get_tasks', arguments: {} } },
  ])), /overlapped/);
  assert.throws(() => readHostTranscript('claude', jsonl([
    { type: 'assistant', message: { content: ['1', '2'].map((id) => ({ type: 'tool_use', id, name: 'mcp__devlink__get_tasks', input: {} })) } },
  ])), /overlapped/);
  assert.deepEqual(parseHostAnswer('Host commentary\n```json\n{"ok":true}\n```'), { ok: true });
  assert.throws(() => parseHostAnswer('All tests passed'));
});

test('one-shot Host configurations preserve user settings and pass only a credential file path', () => {
  const configuration = { directory: 'C:/private fixture', nodePath: 'C:/Program Files/nodejs/node.exe', entryPath: 'C:/workspace/dist/index.js', credentialPath: 'C:/private fixture/broker.json', marker: 'devlink-host64-00000000-0000-0000-0000-000000000000' };
  const codex = hostConfiguration('codex', configuration);
  assert.ok(codex.args.includes('--ignore-user-config')); assert.ok(codex.args.includes('--no-daemon')); assert.ok(codex.args.includes('--ephemeral'));
  assert.ok(!codex.args.includes('add')); assert.ok(!codex.args.includes('--dangerously-bypass-approvals-and-sandbox'));
  const claude = hostConfiguration('claude', configuration);
  assert.deepEqual(Object.keys(claude.mcp.mcpServers), ['devlink']);
  assert.deepEqual(claude.mcp.mcpServers.devlink.env, { DEVLINK_BROKER_FILE: configuration.credentialPath });
  assert.ok(claude.args.includes('--strict-mcp-config')); assert.ok(claude.args.includes('--restricted'));
});

test('the Host fixture extension runs only after token/session/principal validation', async (t) => {
  const broker = await fakeBroker(); const http = new BrokerHttp(broker.settings); let invoked = 0;
  t.after(async () => { http.close(); await broker.close(); });
  broker.setBusinessHook(({ send }) => { invoked++; send({ data: {} }); return true; });
  const session = await http.request('/v1/sessions', { requestId: 'claim', instanceId: 'instance', mode: 'fresh', agentId: 'agent' });
  await assert.rejects(http.request('/v1/tasks/list', { requestId: 'bad', operation: 'get_tasks', arguments: {}, context: { ...session.principal, sessionId: session.sessionId, teamId: 'forged' } }));
  assert.equal(invoked, 0);
  await http.request('/v1/tasks/list', { requestId: 'good', operation: 'get_tasks', arguments: {}, context: { ...session.principal, sessionId: session.sessionId } });
  assert.equal(invoked, 1);
});

test('wire correlation requires present unique identifiers, including a valid numeric zero', () => {
  const profile = { userId: 'user', teamId: 'team', agentId: 'agent', toolType: 'codex' };
  const call = { body: { operation: 'create_task', requestId: 'http-call', mcpRequestId: 0, context: profile } };
  const transcript = { calls: [{ value: success }] };
  verifyWire([call], transcript, profile);
  assert.throws(() => verifyWire([{ body: { ...call.body, requestId: undefined } }], transcript, profile), /Missing HTTP/);
  assert.throws(() => verifyWire([{ body: { ...call.body, mcpRequestId: undefined } }], transcript, profile), /Missing or invalid/);
  assert.throws(() => verifyWire([call, call], transcript, profile), /Repeated/);
  assert.throws(() => verifyWire([], transcript, profile), /No business/);
});

test('test broker records arrivals even if an empty DELETE body cannot be parsed', async (t) => {
  const broker = await fakeBroker(); t.after(() => broker.close());
  const response = await fetch(`${broker.settings.brokerUrl}/v1/sessions/synthetic`, { method: 'DELETE' });
  await response.arrayBuffer(); await broker.drain();
  assert.equal(broker.calls.length, 0);
  assert.deepEqual(broker.arrivals, [{ method: 'DELETE', path: '/v1/sessions/synthetic' }]);
});

test('Host capture preserves split UTF-8 and reports spawn failures', async () => {
  const result = await captureHost(process.execPath, ['-e', "process.stdout.write(Buffer.from([0xed])); setTimeout(()=>process.stdout.write(Buffer.from([0x95,0x9c])),30)"], { timeoutMs: 2000 });
  assert.equal(result.code, 0); assert.equal(result.stdout, '한');
  const missing = await captureHost('devlink-host-executable-that-does-not-exist.exe', [], { timeoutMs: 1000 });
  assert.equal(missing.spawnError, true);
});

test('Windows Host deadline terminates the owned process tree within a bounded wait', { skip: process.platform !== 'win32', timeout: 12000 }, async () => {
  const code = "const {spawn}=require('node:child_process'); const child=spawn(process.execPath,['-e','setInterval(()=>{},1000)'],{stdio:'inherit'}); console.log(child.pid); setInterval(()=>{},1000)";
  const started = Date.now();
  const result = await captureHost(process.execPath, ['-e', code], { timeoutMs: 1200 });
  assert.equal(result.timedOut, true); assert.ok(Date.now() - started < 10000);
  const descendant = Number(result.stdout.trim()); assert.ok(Number.isSafeInteger(descendant) && descendant > 0);
  assert.throws(() => process.kill(descendant, 0), /ESRCH|no such process/i);
});

test('Host capture honors runner abort', { timeout: 12000 }, async () => {
  const interruption = new AbortController();
  const timer = setTimeout(() => interruption.abort(), 300);
  try {
    const result = await captureHost(process.execPath, ['-e', 'setInterval(()=>{},1000)'], { timeoutMs: 5000, signal: interruption.signal });
    assert.equal(result.interrupted, true);
  } finally { clearTimeout(timer); }
});
