import assert from 'node:assert/strict';
import { test } from 'node:test';
import { resolve, join } from 'node:path';
import { centralHostTranscript, hostRanPassingTests, verifyHostFlow, verifyIncomingFlow,normalizeDiff,hostEditedSource,rejectCounterpartAccess,unexpectedProjectFiles,verifyTwoHostExchange,assertEditsFollowAcceptance,verifyFailedIncomingFlow,verifyOutgoingFailure,testRunPositions,partialFailure,assertReadOnlyBeforeAcceptance,readOnlyCommand } from './helpers/central-host-proof.mjs';
import { projectSnapshot, watchUnchangedWhilePending, assertWatchEvidence } from './helpers/pending-watch.mjs';

function fixture() {
  const good = (tool, args, data) => ({ tool, args, completed: true, value: { data } });
  const options = { agentId: '11', counterpartId: '12', sourcePath: 'src/slug.mjs', counterpartPath: 'src/invoice.mjs', incomingNonce: 'incoming', outgoingNonce: 'outgoing' };
  const calls = [
    good('create_task', { recipientAgentId: '12', taskContext: 'approved outgoing' }, { taskId: '21', status: 'PENDING' }),
    good('get_task_result', { taskId: '21' }, { taskId:'21',status: 'COMPLETED', requesterAgent: { agentId: '11' }, receiverAgent: { agentId: '12' }, resultText: 'node --test: 2 passed', taskFiles: [{ relativePath: 'src/invoice.mjs', diffText: '+ corrected' }] }),
    good('get_task_result', { taskId: '22' }, { taskId:'22',status: 'PENDING', prompt: 'approved incoming', requesterAgent: { agentId: '12' }, receiverAgent: { agentId: '11' } }),
    good('update_task_status', { taskId: '22', status: 'IN_PROGRESS' }, { taskId:'22',status: 'IN_PROGRESS' }),
    good('complete_task', { taskId: '22', taskFiles: [{ relativePath: 'src/slug.mjs', changeType: 'MODIFIED', diffText: '+ corrected' }] }, { taskId: '22', status: 'COMPLETED' }),
  ];
  return { options, calls };
}
test('only an explicit accepted request with a requester result read proves the two roles', () => {
  const { options, calls } = fixture();
  assert.equal(verifyHostFlow(calls, options).outgoingId, '21');
  assert.throws(() => verifyHostFlow(calls.filter((c) => c.tool !== 'update_task_status'), options), /explicitly accept/);
  assert.throws(() => verifyHostFlow(calls.filter((c) => c.args.taskId !== '21'), options), /Requester did not read/);
  calls[2].value.data.prompt = 'an unrelated unapproved request';
  assert.throws(() => verifyHostFlow(calls, options), /explicitly approved/);
});
test('a successful CLI turn or a FAILED task cannot substitute for source completion', () => {
  const { options, calls } = fixture();
  calls[4] = { tool: 'update_task_status', args: { taskId: '22', status: 'FAILED' }, completed: true,
    value: { data: { taskId: '22', status: 'FAILED', failureReason: 'execution policy blocked', partiallyModified: false } } };
  assert.throws(() => verifyHostFlow(calls, options), /did not complete/);
  assert.equal(hostRanPassingTests('codex', [{ type: 'item.completed', item: { type: 'agent_message', text: 'All tests passed' } }]), false);
});
test('a verified incoming completion remains inspectable when the outgoing task fails', () => {
  const { options, calls } = fixture();
  calls[1].value.data.status = 'FAILED';
  assert.equal(verifyIncomingFlow(calls, options).incomingId, '22');
  assert.throws(() => verifyHostFlow(calls, options), /Requester did not read/);
});
test('the actual local test invocation and successful output must be correlated', () => {
  const use = { message: { content: [{ type: 'tool_use', id: 'test', name: 'Bash', input: { command: 'node --test' } }] } };
  const result = { message: { content: [{ type: 'tool_result', tool_use_id: 'test', content: 'pass 2\nfail 0', is_error: false }] } };
  assert.equal(hostRanPassingTests('claude', [use]), false);
  assert.equal(hostRanPassingTests('claude', [use, result]), true);
  result.message.content[0].is_error = true;
  assert.equal(hostRanPassingTests('claude', [use, result]), false);
  result.message.content[0].is_error = false;
  result.message.content[0].content = [{type:'text',text:'pass 2\nfail 0'}];
  assert.equal(hostRanPassingTests('claude', [use, result]), true);
  assert.equal(hostRanPassingTests('codex', [{ type: 'item.completed', item: { type: 'command_execution', command: 'node --test', exit_code: 1, aggregated_output: 'pass 2\nfail 0' } }]), false);
});
test('the coding-tool projection still rejects another MCP server', () => {
  const e = { message: { content: [{ type: 'tool_use', id: 'foreign', name: 'mcp__other__run', input: {} }] } };
  assert.throws(() => centralHostTranscript('claude', JSON.stringify(e)), /another MCP server/);
});
test('diff normalization preserves source whitespace and removes only shell line-ending framing',()=>{
  assert.equal(normalizeDiff('+  content \r\n'),normalizeDiff('+  content '));
  assert.notEqual(normalizeDiff('+  content \n'),normalizeDiff('+content\n'));
  assert.notEqual(normalizeDiff('+content\n\n'),normalizeDiff('+content\n'));
  assert.throws(()=>normalizeDiff(' \n'),/must not be empty/);
});
test('an edit must be attributed to the receiving Host and counterpart access is rejected',()=>{
  const project=resolve('fixtures/a'),counterpart=resolve('fixtures/b');
  const codex=[{type:'item.completed',item:{type:'file_change',status:'completed',changes:[{path:'src/slug.mjs',kind:'update'}]}}];
  assert.equal(hostEditedSource('codex',codex,project,'src/slug.mjs'),true);
  assert.equal(hostEditedSource('codex',codex,project,'src/other.mjs'),false);
  const claude=[{message:{content:[{type:'tool_use',name:'Edit',id:'edit',input:{file_path:join(project,'src/slug.mjs')}}]}},
    {message:{content:[{type:'tool_result',tool_use_id:'edit',is_error:false,content:'updated'}]}}];
  assert.equal(hostEditedSource('claude',claude,project,'src/slug.mjs'),true);
  assert.doesNotThrow(()=>rejectCounterpartAccess('claude',claude,project,counterpart));
  claude[0].message.content[0].input.file_path=join(counterpart,'src/slug.mjs');
  assert.equal(hostEditedSource('claude',claude,project,'src/slug.mjs'),false);
  assert.throws(()=>rejectCounterpartAccess('claude',claude,project,counterpart),/counterpart project/);
});
test('new untracked code is rejected even when it would not appear in git diff',()=>{
  assert.deepEqual(unexpectedProjectFiles(['config.json','mcp.json','.devlink-runtime/broker.json','src/helper.mjs']),['src/helper.mjs']);
});
test('the two Hosts must cross-match task IDs and exact receiver diffs',()=>{
  const proof=(host,incomingId,outgoingId,sourcePath,otherPath,diff,otherDiff)=>({host,incomingId,outgoingId,sourcePath,diff,
    observed:{value:{data:{taskFiles:[{relativePath:otherPath,diffText:otherDiff}]}}}});
  const a=proof('codex','22','21','src/slug.mjs','src/invoice.mjs','+ slug\n','+ invoice');
  const b=proof('claude','21','22','src/invoice.mjs','src/slug.mjs','+ invoice\r\n','+ slug');
  assert.equal(verifyTwoHostExchange([a,b]).length,2);
  b.incomingId='99';assert.throws(()=>verifyTwoHostExchange([a,b]),/cross-match/);b.incomingId='21';
  a.observed.value.data.taskFiles[0].diffText='+ forged';assert.throws(()=>verifyTwoHostExchange([a,b]),/diff differs/);
});
test('Codex plan updates are evidence without being mistaken for MCP calls',()=>{
  const lines=[{type:'item.completed',item:{id:'plan',type:'todo_list',items:[]}},{type:'turn.completed'}].map(JSON.stringify).join('\n');
  assert.equal(centralHostTranscript('codex',lines).parsed.calls.length,0);
});

test('AC3: every successful project edit must be issued after the acceptance result', () => {
  const project = resolve('fixtures/a');
  const accept = { id: 'accept' };
  const acceptUse = { message: { content: [{ type: 'tool_use', id: 'accept', name: 'mcp__devlink__update_task_status', input: {} }] } };
  const acceptResult = { message: { content: [{ type: 'tool_result', tool_use_id: 'accept', content: '{}' }] } };
  const editUse = (path, id = 'e-' + path) => ({ message: { content: [{ type: 'tool_use', id, name: 'Edit', input: { file_path: join(project, path) } }] } });
  const editResult = (path, ok = true, id = 'e-' + path) => ({ message: { content: [{ type: 'tool_result', tool_use_id: id, is_error: !ok, content: 'x' }] } });
  const after = [acceptUse, acceptResult, editUse('src/a.mjs'), editResult('src/a.mjs')];
  assert.equal(assertEditsFollowAcceptance('claude', after, project, accept).lastEdit, 2);
  assert.throws(() => assertEditsFollowAcceptance('claude', [editUse('src/a.mjs'), editResult('src/a.mjs'), acceptUse, acceptResult], project, accept), /before the explicit acceptance/);
  // Issued in the same turn as the acceptance: its result arriving later does not excuse it.
  const parallel = { message: { content: [{ type: 'tool_use', id: 'accept', name: 'mcp__devlink__update_task_status', input: {} },
    { type: 'tool_use', id: 'e-src/a.mjs', name: 'Edit', input: { file_path: join(project, 'src/a.mjs') } }] } };
  assert.throws(() => assertEditsFollowAcceptance('claude', [parallel, acceptResult, editResult('src/a.mjs')], project, accept), /before the explicit acceptance/);
  // A failed edit attempt is not a change; an edit outside the project is not counted.
  assert.doesNotThrow(() => assertEditsFollowAcceptance('claude', [editUse('src/b.mjs'), editResult('src/b.mjs', false), acceptUse, acceptResult, editUse('src/a.mjs'), editResult('src/a.mjs')], project, accept));
  assert.doesNotThrow(() => assertEditsFollowAcceptance('claude', [editUse('../other/x.mjs'), editResult('../other/x.mjs'), ...after], project, accept));
  assert.throws(() => assertEditsFollowAcceptance('claude', [acceptUse, acceptResult], project, accept), /no successful project edit/);
  const codexAcceptDone = { type: 'item.completed', item: { id: 'accept', type: 'mcp_tool_call' } };
  const codexEdit = (type) => ({ type, item: { id: 'f', type: 'file_change', status: 'completed', changes: [{ path: 'src/a.mjs' }] } });
  assert.doesNotThrow(() => assertEditsFollowAcceptance('codex', [codexAcceptDone, codexEdit('item.started'), codexEdit('item.completed')], project, accept));
  assert.throws(() => assertEditsFollowAcceptance('codex', [codexEdit('item.started'), codexAcceptDone, codexEdit('item.completed')], project, accept), /before the explicit acceptance/);
  assert.throws(() => assertEditsFollowAcceptance('codex', [codexEdit('item.completed')], project, accept), /Tool result is missing/);
});
test('AC2: the receiver must accept, then fail with a reason and the partial change flag', () => {
  const good = (tool, args, data) => ({ tool, args, completed: true, value: { data } });
  const options = { agentId: '11', counterpartId: '12', incomingNonce: 'incoming' };
  const calls = () => [
    good('get_task_result', { taskId: '22' }, { taskId: '22', status: 'PENDING', prompt: 'incoming', requesterAgent: { agentId: '12' }, receiverAgent: { agentId: '11' } }),
    good('update_task_status', { taskId: '22', status: 'IN_PROGRESS' }, { taskId: '22', status: 'IN_PROGRESS' }),
    good('update_task_status', { taskId: '22', status: 'FAILED', failureReason: 'format test still fails', partiallyModified: true }, { taskId: '22', status: 'FAILED' }),
  ];
  assert.equal(verifyFailedIncomingFlow(calls(), options).incomingId, '22');
  const noFlag = calls(); noFlag[2].args.partiallyModified = false;
  assert.throws(() => verifyFailedIncomingFlow(noFlag, options), /partial source change/);
  const noReason = calls(); noReason[2].args.failureReason = ' ';
  assert.throws(() => verifyFailedIncomingFlow(noReason, options), /failure reason/);
  const early = calls(); early.splice(1, 0, early.splice(2, 1)[0]);
  assert.throws(() => verifyFailedIncomingFlow(early, options), /before accepting/);
  const completed = [...calls(), good('complete_task', { taskId: '22' }, { taskId: '22', status: 'COMPLETED' })];
  assert.throws(() => verifyFailedIncomingFlow(completed, options), /must not also be completed/);
  const otherTask = calls(); otherTask[1].value.data.taskId = '23';
  assert.throws(() => verifyFailedIncomingFlow(otherTask, options), { actual: '23', expected: '22' });
});
test('AC2: the requester must read FAILED with the reason and partial-change flag', () => {
  const good = (tool, args, data) => ({ tool, args, completed: true, value: { data } });
  const options = { agentId: '12', counterpartId: '11', outgoingNonce: 'n' };
  const read = (data) => [good('create_task', { recipientAgentId: '11', taskContext: 'n' }, { taskId: '22', status: 'PENDING' }),
    good('get_task_result', { taskId: '22' }, { taskId: '22', requesterAgent: { agentId: '12' }, receiverAgent: { agentId: '11' }, ...data })];
  assert.equal(verifyOutgoingFailure(read({ status: 'FAILED', failureReason: 'why', partiallyModified: true }), options).outgoingId, '22');
  assert.throws(() => verifyOutgoingFailure(read({ status: 'IN_PROGRESS' }), options), /did not read the FAILED/);
  assert.throws(() => verifyOutgoingFailure(read({ status: 'FAILED', failureReason: '', partiallyModified: true }), options), /failure reason/);
  assert.throws(() => verifyOutgoingFailure(read({ status: 'FAILED', failureReason: 'why', partiallyModified: false }), options), /partial-modification/);
});
test('AC2: only a real test run showing the partial fix counts, at its issue position', () => {
  const run = (id, command, content, isError) => [
    { message: { content: [{ type: 'tool_use', id, name: 'Bash', input: { command } }] } },
    { message: { content: [{ type: 'tool_result', tool_use_id: id, content, is_error: isError }] } }];
  const summary = 'ℹ tests 2\nℹ pass 1\nℹ fail 1';
  assert.deepEqual(testRunPositions('claude', run('t', 'node --test', summary, true), partialFailure), [0]);
  // A chained command can end with exit 0; the runner's own summary lines still decide.
  assert.deepEqual(testRunPositions('claude', run('t', 'cd x && node --test 2>&1; echo exit=$?; git diff', summary + '\nexit=1', false), partialFailure), [0]);
  assert.deepEqual(testRunPositions('claude', run('t', 'node --test', 'ℹ pass 0\nℹ fail 2', true), partialFailure), [], 'before the edit nothing passes');
  assert.deepEqual(testRunPositions('claude', run('t', 'node --test', 'Permission to use Bash has been denied', true), partialFailure), [], 'a denied command is not a test result');
  assert.deepEqual(testRunPositions('claude', run('t', 'node --test', 'expected: pass 1 fail 1', true), partialFailure), [], 'prose is not a runner summary');
  assert.deepEqual(testRunPositions('claude', run('t', 'echo node --test', summary, true), partialFailure), []);
  const codex = (exitCode, output = summary) => [{ type: 'item.completed', item: { id: 'c', type: 'command_execution', command: 'node --test', exit_code: exitCode, aggregated_output: output } }];
  assert.deepEqual(testRunPositions('codex', codex(1), partialFailure), [0]);
  assert.deepEqual(testRunPositions('codex', codex(undefined), partialFailure), [], 'a run without an exit code is not a result');
  assert.deepEqual(testRunPositions('codex', codex(0, 'ℹ pass 2\nℹ fail 0'), partialFailure), []);
});
test('the pre-acceptance watcher ignores runner files and flags a Host change while the task is PENDING', async () => {
  const { mkdtemp, writeFile, mkdir, rm } = await import('node:fs/promises');
  const { tmpdir } = await import('node:os');
  const folder = await mkdtemp(join(tmpdir(), 'pending-watch-'));
  const until = async (predicate) => { for (let i = 0; i < 400 && !predicate(); i++) await new Promise((r) => setTimeout(r, 10)); assert(predicate()); };
  try {
    await mkdir(join(folder, 'src')); await writeFile(join(folder, 'src/a.mjs'), 'one');
    const initial = await projectSnapshot(folder);
    let statuses = [];
    const watch = watchUnchangedWhilePending({ folder, initial, receivedStatuses: async () => statuses, intervalMs: 5 });
    statuses = ['PENDING'];
    await writeFile(join(folder, 'config.json'), 'managed by the runner');
    await mkdir(join(folder, '.devlink-runtime')); await writeFile(join(folder, '.devlink-runtime/broker.json'), '{}');
    const seen = watch.current().samples;
    await until(() => watch.current().samples > seen + 3 && watch.current().taskPendingSamples > 0);
    assert.equal(watch.current().violations, 0, 'runner-managed files are not Host changes');
    await writeFile(join(folder, 'src/a.mjs'), 'two (shell write before acceptance)');
    await until(() => watch.current().violations > 0);
    statuses = ['IN_PROGRESS'];
    await until(() => watch.current().acceptedObserved);
    const before = watch.current().violations, after = watch.current().samples;
    await writeFile(join(folder, 'src/a.mjs'), 'three (edit after acceptance)');
    await until(() => watch.current().samples > after + 3);
    assert.equal(watch.current().violations, before, 'changes after acceptance are not violations');
    const result = await watch.stop();
    assert.throws(() => assertWatchEvidence(result), /changed while the incoming task was still PENDING/);
    assert.throws(() => assertWatchEvidence({ ...result, violations: 0, errors: 1 }), /sampling errors/);
    assert.throws(() => assertWatchEvidence({ ...result, violations: 0, taskPendingSamples: 0 }), /both PENDING and accepted/);
    assert.doesNotThrow(() => assertWatchEvidence({ ...result, violations: 0 }));
  } finally { await rm(folder, { recursive: true, force: true }); }
});
test('only read-only shell commands may run before the acceptance', () => {
  for (const command of ['sleep 10', 'cat src/a.mjs && node --test 2>&1', 'git diff -- src', 'ls -la src | head -5',
    '"C:\\Program Files\\PowerShell\\7\\pwsh.exe" -Command \'Start-Sleep -Seconds 1\'', 'pwsh -NoProfile -Command "Get-Content src/a.mjs"',
    'node --test > /dev/null 2>&1', 'timeout 5', 'timeout /t 3 /nobreak']) assert.equal(readOnlyCommand(command), true, command);
  for (const command of ['echo x > src/a.mjs', "sed -i 's/a/b/' src/a.mjs", 'cat a | tee src/a.mjs', 'rm src/a.mjs', 'git checkout -- src',
    'pwsh -Command "Set-Content src/a.mjs x"', 'python fix.py', 'node fix.mjs', 'touch src/new.mjs', 'cp a b',
    'timeout 5 node fix.mjs', 'find . -name x -delete', 'find . -exec node x \\;', 'cat $(node fix.mjs)', 'echo `node fix.mjs`',
    'cat <(node fix.mjs)', 'git diff --output=src/a.mjs'])
    assert.equal(readOnlyCommand(command), false, command);
  const accept = { id: 'accept' };
  const bash = (id, command) => ({ message: { content: [{ type: 'tool_use', id, name: 'Bash', input: { command } }] } });
  const acceptUse = { message: { content: [{ type: 'tool_use', id: 'accept', name: 'mcp__devlink__update_task_status', input: {} }] } };
  const acceptResult = { message: { content: [{ type: 'tool_result', tool_use_id: 'accept', content: '{}' }] } };
  assert.equal(assertReadOnlyBeforeAcceptance('claude', [bash('a', 'sleep 1'), acceptUse, acceptResult, bash('b', 'echo x > src/a.mjs')], accept), 1);
  // A write issued in the same turn as the acceptance still precedes its result.
  assert.throws(() => assertReadOnlyBeforeAcceptance('claude', [acceptUse, bash('w', "sed -i 's/a/b/' src/a.mjs"), acceptResult], accept), /non-read-only/);
  const codex = (id, command) => ({ type: 'item.started', item: { id, type: 'command_execution', command } });
  const codexAccept = { type: 'item.completed', item: { id: 'accept', type: 'mcp_tool_call' } };
  assert.throws(() => assertReadOnlyBeforeAcceptance('codex', [codex('w', 'pwsh -Command "Set-Content src/a.mjs x"'), codexAccept], accept), /non-read-only/);
});
