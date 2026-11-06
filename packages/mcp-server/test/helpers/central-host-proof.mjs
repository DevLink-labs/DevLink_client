import assert from 'node:assert/strict';
import { resolve, relative, isAbsolute } from 'node:path';
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

function insideProject(project, path) {
  if (typeof path !== 'string') return false;
  const rel = relative(resolve(project), resolve(project, path));
  return !rel.startsWith('..') && !isAbsolute(rel);
}
/** Transcript position where a tool call was issued: Claude's tool_use, or Codex's first event for the item. */
export function callPosition(host, events, call) {
  const index = host === 'codex'
    ? events.findIndex((e) => e.item?.id === call.id)
    : events.findIndex((e) => parts(e).some((p) => p.type === 'tool_use' && p.id === call.id));
  assert(index >= 0, 'Tool call is missing from the transcript');
  return index;
}
/** Transcript position where a tool call's result was returned to the model. */
export function resultPosition(host, events, call) {
  const index = host === 'codex'
    ? events.findIndex((e) => e.type === 'item.completed' && e.item?.id === call.id)
    : events.findIndex((e) => parts(e).some((p) => p.type === 'tool_result' && p.tool_use_id === call.id));
  assert(index >= 0, 'Tool result is missing from the transcript');
  return index;
}
/**
 * Positions where a successful edit inside the project was issued. An edit changes the file
 * when it is issued, before its result is recorded, so the issue position is what counts.
 * Shell writes are not visible here; runners pair this with a file snapshot watcher.
 */
export function projectEditPositions(host, events, project) {
  if (host === 'codex') {
    const done = events.filter((e) => e.type === 'item.completed' && e.item?.type === 'file_change'
      && e.item.status === 'completed' && e.item.changes?.some((c) => insideProject(project, c.path)));
    return done.map((e) => events.findIndex((x) => x.item?.id === e.item.id));
  }
  const uses = events.flatMap((e, index) => parts(e).filter((p) => p.type === 'tool_use'
    && ['Edit', 'Write'].includes(p.name) && insideProject(project, p.input?.file_path)).map((p) => ({ id: p.id, index })));
  return uses.filter((use) => events.some((e) => parts(e).some((p) => p.type === 'tool_result'
    && p.tool_use_id === use.id && !p.is_error))).map((use) => use.index);
}
/** AC3: the receiving Host issued no successful project edit before its explicit acceptance returned. */
export function assertEditsFollowAcceptance(host, events, project, accepted) {
  const edits = projectEditPositions(host, events, project);
  assert(edits.length > 0, 'Receiving Host made no successful project edit');
  const accepting = resultPosition(host, events, accepted);
  assert(edits.every((index) => index > accepting), 'Receiving Host edited its project before the explicit acceptance');
  return { firstEdit: Math.min(...edits), lastEdit: Math.max(...edits), accepting };
}
/** AC2 receiver: inspect PENDING, explicitly accept, then register FAILED with a reason and partial modification. */
export function verifyFailedIncomingFlow(calls, { agentId, counterpartId, incomingNonce }) {
  const good = calls.filter(successful);
  assert.equal(good.filter((c) => c.tool === 'complete_task').length, 0, 'A failed task must not also be completed');
  const failed = good.filter((c) => c.tool === 'update_task_status' && c.args.status === 'FAILED');
  assert.equal(failed.length, 1, 'Host did not register exactly one FAILED status');
  const failure = failed[0], incomingId = taskId(failure.args.taskId);
  assert.equal(failure.value.data.taskId, incomingId);
  assert.equal(failure.value.data.status, 'FAILED');
  assert(typeof failure.args.failureReason === 'string' && failure.args.failureReason.trim(), 'FAILED needs a failure reason');
  assert.equal(failure.args.partiallyModified, true, 'The partial source change must be reported');
  const accepted = good.find((c) => c.tool === 'update_task_status' && c.args.taskId === incomingId
    && c.args.status === 'IN_PROGRESS' && c.value.data.status === 'IN_PROGRESS');
  assert(accepted, 'Host did not explicitly accept the incoming task');
  assert.equal(accepted.value.data.taskId, incomingId);
  const pending = good.find((c) => c.tool === 'get_task_result' && c.args.taskId === incomingId
    && c.value.data.status === 'PENDING' && calls.indexOf(c) < calls.indexOf(accepted));
  assert(pending, 'Host did not inspect PENDING before accepting');
  assert.equal(pending.value.data.taskId, incomingId);
  assert(pending.value.data.prompt?.includes(incomingNonce), 'Incoming task was not the explicitly approved request');
  assert.equal(pending.value.data.receiverAgent?.agentId, String(agentId));
  assert.equal(pending.value.data.requesterAgent?.agentId, String(counterpartId));
  assert(calls.indexOf(accepted) < calls.indexOf(failure), 'Host failed the task before accepting it');
  return { failure, accepted, incomingId };
}
/** AC2 requester: create once, then read the FAILED result with its reason and partial-modification flag. */
export function verifyOutgoingFailure(calls, { agentId, counterpartId, outgoingNonce }) {
  const good = calls.filter(successful);
  const made = good.filter((c) => c.tool === 'create_task');
  assert.equal(made.length, 1, 'Host must create exactly one outgoing task');
  const created = made[0], outgoingId = taskId(created.value.data.taskId);
  assert.equal(created.args.recipientAgentId, String(counterpartId));
  assert.equal(created.value.data.status, 'PENDING');
  assert(created.args.taskContext?.includes(outgoingNonce), 'Outgoing context lost its nonce');
  const observed = good.find((c) => c.tool === 'get_task_result' && c.args.taskId === outgoingId
    && c.value.data.status === 'FAILED' && calls.indexOf(c) > calls.indexOf(created));
  assert(observed, 'Requester did not read the FAILED outgoing result');
  assert.equal(observed.value.data.taskId, outgoingId);
  assert.equal(observed.value.data.requesterAgent?.agentId, String(agentId));
  assert.equal(observed.value.data.receiverAgent?.agentId, String(counterpartId));
  assert(observed.value.data.failureReason?.trim(), 'Requester did not receive the failure reason');
  assert.equal(observed.value.data.partiallyModified, true, 'Requester did not receive the partial-modification flag');
  return { created, observed, outgoingId };
}
const testCommand = /(^|[;&|(]\s*|\s)node(\.exe)?\s+--test\b/;
function testCommandIssued(command) { return typeof command === 'string' && testCommand.test(command) && !/^\s*echo\b/.test(command); }
/**
 * Positions of local test runs whose output matches `expected` (for example pass 1 / fail 1).
 * Claude: the Bash tool_use position, judged by its correlated result text. Codex: a completed
 * command with a numeric exit code. A denied or malformed command never counts as a test result.
 */
export function testRunPositions(host, events, expected) {
  if (host === 'codex') return events.flatMap((e) => e.type === 'item.completed' && e.item?.type === 'command_execution'
    && testCommandIssued(e.item.command) && typeof e.item.exit_code === 'number'
    && expected(e.item.aggregated_output ?? '', e.item.exit_code) ? [events.findIndex((x) => x.item?.id === e.item.id)] : []);
  const uses = events.flatMap((e, index) => parts(e).filter((p) => p.type === 'tool_use' && p.name === 'Bash'
    && testCommandIssued(p.input?.command)).map((p) => ({ id: p.id, index })));
  return uses.filter((use) => events.some((e) => parts(e).some((p) => p.type === 'tool_result' && p.tool_use_id === use.id
    && expected(contentText(p.content), p.is_error ? 1 : 0)))).map((use) => use.index);
}
/**
 * The partial fix result: the node test runner's own summary lines report exactly one passing
 * and one failing test. The tool's exit status is not used, because a Host may chain commands
 * (`node --test; echo exit=$?; git diff`) so the last command decides it; a denied or malformed
 * command never prints these summary lines.
 */
export function partialFailure(output) {
  return /^\s*(?:ℹ|#)\s*pass\s+1\s*$/m.test(output) && /^\s*(?:ℹ|#)\s*fail\s+1\s*$/m.test(output);
}

/** Commands a Host may run before accepting: reading, waiting and testing, never writing. */
// `timeout` is allowed only as a bare wait (`timeout 5`, `timeout /t 5`), never as a wrapper that runs another command.
const readOnlyStarts = /^(?:sleep|start-sleep|timeout(?:\s+\/t)?\s+\d+(?:\s+\/nobreak)?$|cat|type|get-content|gc|ls|dir|get-childitem|gci|pwd|cd|set-location|echo|write-output|head|tail|wc|find|findstr|grep|rg|select-string|stat|test-path|node\s+--test|git\s+(?:diff|status|log|show|ls-files|rev-parse))\b/i;
const writeMarkers = /\b(?:sed\s+-i|tee|rm|del|mv|cp|copy|move|touch|mkdir|rmdir|set-content|add-content|out-file|new-item|remove-item|copy-item|move-item|rename-item|git\s+(?:apply|checkout|reset|restore|stash|commit|add|rm|mv|clean))\b/i;
/** Constructs that run or write something inside an otherwise read-only command: substitution, `find -delete/-exec`, `git diff --output`. */
const hiddenEffects = /\$\(|`|<\(|\s-(?:delete|exec|execdir|ok|okdir|fprint\w*)\b|--output\b/i;
function readOnlyCommand(command) {
  if (typeof command !== 'string') return false;
  // Unwrap a PowerShell/cmd invocation such as `pwsh -Command "..."` or `cmd /c ...`.
  // The executable may be a quoted path with spaces, e.g. "C:\Program Files\PowerShell\7\pwsh.exe".
  const body = command.replace(/^\s*(?:"[^"]*?(?:pwsh|powershell)(?:\.exe)?"|\S*?(?:pwsh|powershell)(?:\.exe)?)\s+(?:-\w+\s+)*?-Command\s+/i, '')
    .replace(/^\s*cmd(?:\.exe)?\s+\/c\s+/i, '').replace(/^['"]|['"]\s*$/g, '');
  const cleaned = body.replace(/\d?>&\d/g, '').replace(/\d?>\s*(?:\/dev\/null|nul|\$null)\b/gi, '');
  if (/>|\btee\b/i.test(cleaned) || writeMarkers.test(cleaned) || hiddenEffects.test(cleaned)) return false;
  return cleaned.split(/&&|\|\||[;|\n]/).map((s) => s.trim()).filter(Boolean).every((segment) => readOnlyStarts.test(segment));
}
/**
 * AC3 complement to the file watcher: every shell command issued before the acceptance result
 * must be read-only (reading, waiting, testing). This closes the gap of a write issued in the
 * same turn as the acceptance, between two watcher samples.
 */
export function assertReadOnlyBeforeAcceptance(host, events, accepted) {
  const accepting = resultPosition(host, events, accepted);
  const commands = host === 'codex'
    ? events.flatMap((e, index) => e.item?.type === 'command_execution' && events.findIndex((x) => x.item?.id === e.item.id) === index ? [{ index, command: e.item.command }] : [])
    : events.flatMap((e, index) => parts(e).filter((p) => p.type === 'tool_use' && p.name === 'Bash').map((p) => ({ index, command: p.input?.command })));
  const early = commands.filter((c) => c.index < accepting);
  const offending = early.filter((c) => !readOnlyCommand(c.command));
  assert.equal(offending.length, 0, 'Host ran a non-read-only shell command before the explicit acceptance: ' + offending.map((c) => String(c.command).slice(0, 80)).join(' | '));
  return early.length;
}
export { readOnlyCommand };
