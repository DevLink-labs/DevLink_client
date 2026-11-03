import {test} from 'node:test';
import assert from 'node:assert/strict';
import {verifyOnboardingProof} from './helpers/onboarding-proof.mjs';
const options={phase:'initial',agentId:'3',userId:'6',teamId:'9',tool:'codex'};
function fixture(){
 const context={identity:'verified',principal:{agentId:'3',userId:'6',teamId:'9',toolType:'codex'},hostSession:'unbound'};
 const diagnostic={identity:'verified',central:'Connected',credentials:'valid',hostSession:'unbound'};
 const values=[context,diagnostic,{agentId:'3'},{...diagnostic,hostSession:'bound'},{...context,hostSession:'bound'},{agentId:'3'}];
 return {calls:['get_my_context','get_connection_status','register_agent','get_connection_status','get_my_context','register_agent'].map((tool,i)=>({tool,args:{},completed:true,hostError:false,value:{requestId:'test-'+i,data:values[i]}}))};
}
test('accepts native correlated results for empty registration and stable subject',()=>{assert.equal(verifyOnboardingProof(fixture(),options).registerCalls,2);});
test('model completion prose without tool events cannot count as successful onboarding',()=>{assert.throws(()=>verifyOnboardingProof({calls:[],finalText:'all successful'},options));});
test('rejects metadata supplied by a Host',()=>{const f=fixture();f.calls[2].args={name:'manual'};assert.throws(()=>verifyOnboardingProof(f,options));});
test('rejects tool failure, wrong principal and changed Agent results',()=>{
 for(const change of [f=>f.calls[2].value={error:{code:'CENTRAL_RESULT_UNKNOWN'}},f=>f.calls[0].value.data.principal.userId='7',f=>f.calls[5].value.data.agentId='4']){
  const f=fixture();change(f);assert.throws(()=>verifyOnboardingProof(f,options));
 }
});
test('rejects bound state that was only claimed in prose',()=>{const f=fixture();f.calls[3].value.data.hostSession='unbound';f.finalText='bound';assert.throws(()=>verifyOnboardingProof(f,options));});
test('requires empty object call order and complete calls',()=>{for(const change of [f=>f.calls.reverse(),f=>f.calls[2].completed=false,f=>f.calls.push(f.calls[5])]){const f=fixture();change(f);assert.throws(()=>verifyOnboardingProof(f,options));}});
test('owner conflict must be rejected with safe recovery advice',()=>{
 const f={calls:[{tool:'get_connection_status',args:{},completed:true,hostError:false,value:{data:{hostSession:'conflict'}}},{tool:'register_agent',args:{},completed:true,hostError:true,value:{error:{code:'SESSION_CONFLICT',nextActions:['inspect_connection_owner']}}}]};
 assert.equal(verifyOnboardingProof(f,{...options,phase:'ownership-conflict'}).registerCalls,1);
 f.calls[1].value.error.nextActions=[];assert.throws(()=>verifyOnboardingProof(f,{...options,phase:'ownership-conflict'}));
});
test('missing credentials and stopped broker require the relevant diagnosis',()=>{
 for(const [phase,data]of [['credentials-missing',{credentials:'missing',issue:{code:'CREDENTIAL_FILE_INVALID',scope:'credentials'},nextActions:['prepare_cli_profile']}],['broker-stopped',{broker:'unknown',setupReady:false,issue:{code:'BROKER_UNAVAILABLE',scope:'transport'},nextActions:['inspect_local_broker']}]] ){
  const f={calls:[{tool:'get_connection_status',args:{},completed:true,hostError:false,value:{data}}]};assert.equal(verifyOnboardingProof(f,{...options,phase}).registerCalls,0);
  data.nextActions=[];assert.throws(()=>verifyOnboardingProof(f,{...options,phase}));
 }
});

test('rejects wrong team, tool, first binding state and unhealthy bound diagnosis',()=>{
 for(const change of [f=>f.calls[0].value.data.principal.teamId='10',f=>f.calls[0].value.data.principal.toolType='claude_code',f=>f.calls[0].value.data.hostSession='bound',f=>f.calls[2].value.data.agentId='4',f=>f.calls[3].value.data.central='Disconnected']){const f=fixture();change(f);assert.throws(()=>verifyOnboardingProof(f,options));}
});
test('diagnostic proof rejects argument injection, native errors and wrong fault classification',()=>{
 const f={calls:[{tool:'get_connection_status',args:{},completed:true,hostError:false,value:{data:{credentials:'missing',issue:{scope:'credentials',code:'CREDENTIAL_FILE_INVALID'},nextActions:['prepare_cli_profile']}}}]};
 for(const change of [g=>g.calls[0].args={selector:'other'},g=>g.calls[0].hostError=true,g=>g.calls[0].value.data.credentials='valid',g=>g.calls[0].value.data.issue.scope='transport']){const g=structuredClone(f);change(g);assert.throws(()=>verifyOnboardingProof(g,{...options,phase:'credentials-missing'}));}
});

test('healthy diagnosis with an issue is rejected and restored credentials require full health',()=>{
 for(const index of [1,3]){const f=fixture();f.calls[index].value.data.issue={code:'CENTRAL_UNAVAILABLE'};assert.throws(()=>verifyOnboardingProof(f,options));}
 const f={calls:[{tool:'get_connection_status',args:{},completed:true,hostError:false,value:{data:{credentials:'valid',central:'Connected',identity:'verified'}}}]};
 assert.equal(verifyOnboardingProof(f,{...options,phase:'credentials-restored'}).registerCalls,0);
 f.calls[0].value.data.issue={code:'CENTRAL_UNAVAILABLE'};assert.throws(()=>verifyOnboardingProof(f,{...options,phase:'credentials-restored'}));
});
