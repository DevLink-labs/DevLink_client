import assert from 'node:assert/strict';
import { resolve, relative } from 'node:path';
import { readHostTranscript } from './host-transcript.mjs';

/** Keep local coding tools as evidence, while decoding MCP results with the normal Host parser. */
export function centralHostTranscript(host, stdout) {
  const events = stdout.split(/\r?\n/).filter(Boolean).map((line) => JSON.parse(line));
  if (host === 'codex') {
    const filtered = events.filter((e) => !e.item || !['command_execution', 'file_change', 'todo_list'].includes(e.item.type));
    return { events, parsed: readHostTranscript(host, filtered.map((e) => JSON.stringify(e)).join('\n')) };
  }
  assert.equal(host, 'claude');
  const ids = new Set();
  for (const e of events) for (const part of Array.isArray(e.message?.content) ? e.message.content : []) {
    if (part.type !== 'tool_use') continue;
    if (part.name.startsWith('mcp__')) {
      assert(part.name.startsWith('mcp__devlink__'), 'Host used another MCP server');
      ids.add(part.id);
    } else assert(['Read', 'Edit', 'Write', 'Bash'].includes(part.name), 'Host used an unexpected local tool');
  }
  const filtered = events.map((e) => e.message ? { ...e, message: { ...e.message,
    content: (Array.isArray(e.message.content) ? e.message.content : []).filter((p) => p.type === 'tool_use' ? ids.has(p.id)
      : p.type === 'tool_result' ? ids.has(p.tool_use_id) : true) } } : e);
  return { events, parsed: readHostTranscript(host, filtered.map((e) => JSON.stringify(e)).join('\n')) };
}

function successful(call) {
  return call.completed && !call.hostError && !call.inputError && call.value?.data && !call.value.error;
}
function taskId(value) {
  assert(typeof value === 'string' && /^[1-9][0-9]*$/.test(value), 'Host task ID must be a canonical string');
  return value;
}

/** Prose and exit 0 alone never prove that a request completed or that its requester read it. */
export function verifyIncomingFlow(calls, { agentId, counterpartId, sourcePath, incomingNonce }) {
  const good = calls.filter(successful);
  const done = good.filter((c) => c.tool === 'complete_task');
  assert.equal(done.length, 1, 'Host did not complete its incoming task');
  const completed = done[0], incomingId = taskId(completed.args.taskId);
  assert.equal(completed.value.data.taskId, incomingId);
  assert.equal(completed.value.data.status, 'COMPLETED');
  assert.equal(completed.args.taskFiles?.length, 1);
  assert.equal(completed.args.taskFiles[0].relativePath, sourcePath);
  assert.equal(completed.args.taskFiles[0].changeType, 'MODIFIED');
  const accepted = good.find((c) => c.tool === 'update_task_status' && c.args.taskId === incomingId
    && c.args.status === 'IN_PROGRESS' && c.value.data.status === 'IN_PROGRESS');
  assert(accepted, 'Host did not explicitly accept the incoming task');
  const pending = good.find((c) => c.tool === 'get_task_result' && c.args.taskId === incomingId
    && c.value.data.status === 'PENDING' && calls.indexOf(c) < calls.indexOf(accepted));
  assert(pending, 'Host did not inspect PENDING before accepting');
  assert.equal(pending.value.data.taskId, incomingId);
  assert.equal(accepted.value.data.taskId, incomingId);
  assert(pending.value.data.prompt?.includes(incomingNonce), 'Incoming task was not the explicitly approved request');
  assert.equal(pending.value.data.receiverAgent?.agentId, String(agentId));
  assert.equal(pending.value.data.requesterAgent?.agentId, String(counterpartId));
  assert(calls.indexOf(accepted) < calls.indexOf(completed), 'Host completed before accepting');
  return { completed, accepted, incomingId };
}
export function verifyOutgoingFlow(calls, { agentId, counterpartId, counterpartPath, outgoingNonce }) {
  const good = calls.filter(successful);
  const made = good.filter((c) => c.tool === 'create_task');
  assert.equal(made.length, 1, 'Host must create exactly one outgoing task');
  const created = made[0], outgoingId = taskId(created.value.data.taskId);
  assert.equal(created.args.recipientAgentId, String(counterpartId));
  assert.equal(created.value.data.status, 'PENDING');
  assert(created.args.taskContext?.includes(outgoingNonce), 'Outgoing context lost its nonce');
  const observed = good.find((c) => c.tool === 'get_task_result' && c.args.taskId === outgoingId
    && c.value.data.status === 'COMPLETED' && calls.indexOf(c) > calls.indexOf(created));
  assert(observed, 'Requester did not read the completed outgoing result');
  assert.equal(observed.value.data.taskId, outgoingId);
  assert.equal(observed.value.data.requesterAgent?.agentId, String(agentId));
  assert.equal(observed.value.data.receiverAgent?.agentId, String(counterpartId));
  assert(observed.value.data.resultText?.includes('node --test'), 'Requester did not receive verification details');
  assert(observed.value.data.taskFiles?.some((f) => f.relativePath === counterpartPath && f.diffText?.trim()),
    'Requester did not receive the actual changed-file diff');
  return { created, observed, outgoingId };
}
export function verifyHostFlow(calls, options) {
  const incoming = verifyIncomingFlow(calls, options), outgoing = verifyOutgoingFlow(calls, options);
  assert.notEqual(incoming.incomingId, outgoing.outgoingId, 'Incoming and outgoing tasks must be different');
  return { ...incoming, ...outgoing };
}

function passingTests(text) { return /\bpass\s+2\b/.test(text) && /\bfail\s+0\b/.test(text); }
function parts(e) { return Array.isArray(e.message?.content) ? e.message.content : []; }
function contentText(content) { return typeof content === 'string' ? content : Array.isArray(content)
  ? content.filter((p) => p.type === 'text').map((p) => p.text ?? '').join('\n') : ''; }
export function hostRanPassingTests(host, events) {
  if (host === 'codex') return events.some((e) => e.type === 'item.completed'
    && e.item?.type === 'command_execution' && e.item.command?.includes('node --test')
    && e.item.exit_code === 0 && passingTests(e.item.aggregated_output ?? ''));
  const ids = new Set(events.flatMap((e) => parts(e)
    .filter((p) => p.type === 'tool_use' && p.name === 'Bash' && p.input?.command?.includes('node --test')).map((p) => p.id)));
  return events.some((e) => parts(e).some((p) => p.type === 'tool_result'
    && ids.has(p.tool_use_id) && !p.is_error && passingTests(contentText(p.content))));
}

/** Normalize only shell transport line endings; preserve spaces and blank diff lines. */
export function normalizeDiff(text) {
  assert.equal(typeof text, 'string');
  assert(text.trim(), 'Changed-file diff must not be empty');
  return text.replaceAll('\r\n', '\n').replace(/\n$/, '');
}
function samePath(project, candidate, target) {
  if (typeof candidate !== 'string') return false;
  const canonical = (path) => process.platform === 'win32' ? resolve(path).toLowerCase() : resolve(path);
  return canonical(resolve(project, candidate)) === canonical(resolve(project, target));
}
export function hostEditedSource(host, events, project, sourcePath) {
  if (host === 'codex') return events.some((e) => e.type === 'item.completed' && e.item?.type === 'file_change'
    && e.item.status === 'completed' && e.item.changes?.some((c) => samePath(project, c.path, sourcePath)));
  const ids = new Set(events.flatMap((e) => parts(e).filter((p) => p.type === 'tool_use'
    && ['Edit', 'Write'].includes(p.name) && samePath(project, p.input?.file_path, sourcePath)).map((p) => p.id)));
  return events.some((e) => parts(e).some((p) => p.type === 'tool_result' && ids.has(p.tool_use_id) && !p.is_error));
}
export function rejectCounterpartAccess(host, events, project, counterpart) {
  const normal = (s) => s.replaceAll('\\', '/').toLowerCase();
  const forbidden = [normal(resolve(counterpart)), normal(relative(project, counterpart))];
  const inspect = (value) => {
    if (typeof value === 'string') assert(!forbidden.some((path) => normal(value).includes(path)), 'Host accessed its counterpart project');
    else if (value && typeof value === 'object') Object.values(value).forEach(inspect);
  };
  for (const e of events) {
    if (host === 'codex' && ['command_execution', 'file_change'].includes(e.item?.type)) inspect(e.item);
    if (host === 'claude') for (const p of parts(e)) if (p.type === 'tool_use' && !p.name.startsWith('mcp__')) inspect(p.input);
  }
}
export function unexpectedProjectFiles(files) {
  return files.filter((file) => !['config.json', 'mcp.json'].includes(file) && !file.startsWith('.devlink-runtime/'));
}
export function verifyTwoHostExchange(proofs) {
  assert.equal(proofs.length, 2);
  const a = proofs.find((p) => p.host === 'codex'), b = proofs.find((p) => p.host === 'claude');
  assert(a && b, 'Both distinct Hosts are required');
  const links = [];
  for (const [sender, receiver] of [[a, b], [b, a]]) {
    assert.equal(sender.outgoingId, receiver.incomingId, 'Host task IDs do not cross-match');
    const files = sender.observed.value.data.taskFiles;
    assert.equal(files.length, 1);
    assert.equal(files[0].relativePath, receiver.sourcePath);
    assert.equal(normalizeDiff(files[0].diffText), normalizeDiff(receiver.diff), 'Requester diff differs from actual receiver edit');
    links.push({ requester: sender.host, receiver: receiver.host, taskId: receiver.incomingId });
  }
  return links;
}
