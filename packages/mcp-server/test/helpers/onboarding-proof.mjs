import assert from 'node:assert/strict';
/** Assert correlated native tool results; model prose is never a success signal. */
export function verifyOnboardingProof(parsed,{phase,agentId,userId,teamId,tool}){
 const calls=parsed.calls;
 assert(['initial','host-restart','broker-restart','recovered','ownership-conflict','early-restart-conflict','credentials-missing','credentials-restored','broker-stopped'].includes(phase),'Unsupported proof phase');
 const expected=['initial','host-restart','broker-restart','recovered'].includes(phase)?['get_my_context','get_connection_status','register_agent','get_connection_status','get_my_context','register_agent']:['ownership-conflict','early-restart-conflict'].includes(phase)?['get_connection_status','register_agent']:['get_connection_status'];
 assert.deepEqual(calls.map(c=>c.tool),expected,'Unexpected/missing/duplicate tool invocation');
 assert(calls.every(c=>c.completed&&c.args&&typeof c.args==='object'&&!Array.isArray(c.args)&&Object.keys(c.args).length===0),'Additional metadata or incomplete call');
 let registers=0;
 if(expected.length===6){
  for(const c of calls){assert(c.value?.data && !c.value.error,'Tool returned an error');assert.equal(c.hostError,false);}
  const [ctx,diag,reg,bound,after,again]=calls.map(c=>c.value.data);
  for(const c of [ctx,after]){assert.equal(c.identity,'verified');assert.equal(c.principal.agentId,agentId);assert.equal(c.principal.userId,userId);assert.equal(c.principal.teamId,teamId);assert.equal(c.principal.toolType,tool);}
  assert.equal(ctx.hostSession,'unbound');
  assert.equal(diag.central,'Connected');assert.equal(diag.identity,'verified');assert.equal(diag.credentials,'valid');assert.equal(diag.issue,undefined);assert.equal(diag.hostSession,'unbound');
  assert.equal(bound.central,'Connected');assert.equal(bound.identity,'verified');assert.equal(bound.credentials,'valid');assert.equal(bound.issue,undefined);assert.equal(bound.hostSession,'bound');assert.equal(after.hostSession,'bound');
  assert.equal(reg.agentId,agentId);assert.equal(again.agentId,agentId);registers=2;
 }else if(['ownership-conflict','early-restart-conflict'].includes(phase)){
  assert.equal(calls[0].hostError,false);assert(calls[0].value?.data && !calls[0].value.error);assert.equal(calls[0].value.data.hostSession,'conflict');
  assert.equal(calls[1].value?.data,undefined);assert.equal(calls[1].hostError,true);assert.equal(calls[1].value.error.code,'SESSION_CONFLICT');
  assert(calls[1].value.error.nextActions?.includes('inspect_connection_owner'));registers=1;
 }else{
  assert.equal(calls[0].hostError,false);assert(calls[0].value?.data && !calls[0].value.error);const d=calls[0].value.data;
  if(phase==='credentials-missing'){assert.equal(d.credentials,'missing');assert(d.nextActions.includes('prepare_cli_profile'));assert.equal(d.issue?.scope,'credentials');assert.equal(d.issue?.code,'CREDENTIAL_FILE_INVALID');}
  else if(phase==='credentials-restored'){assert.equal(d.credentials,'valid');assert.equal(d.central,'Connected');assert.equal(d.identity,'verified');assert.equal(d.issue,undefined);}
  else {assert.equal(phase,'broker-stopped');assert.notEqual(d.broker,'running');assert.equal(d.setupReady,false);assert(d.nextActions.includes('inspect_local_broker'));assert.equal(d.issue?.scope,'transport');assert.equal(d.issue?.code,'BROKER_UNAVAILABLE');}
 }
 return {verifiedCalls:calls.length,registerCalls:registers,metadataInputs:0,actualNativeToolResults:true};
}
