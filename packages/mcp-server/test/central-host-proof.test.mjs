import assert from 'node:assert/strict';
import { test } from 'node:test';
import { resolve, join } from 'node:path';
import { centralHostTranscript, hostRanPassingTests, verifyHostFlow, verifyIncomingFlow,normalizeDiff,hostEditedSource,rejectCounterpartAccess,unexpectedProjectFiles,verifyTwoHostExchange } from './helpers/central-host-proof.mjs';

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
