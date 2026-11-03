import assert from 'node:assert/strict';
import test from 'node:test';
import {createServer} from 'node:http';
import {Client} from '@modelcontextprotocol/client';
import {InMemoryTransport} from '@modelcontextprotocol/server';
import {BrokerClient} from '../dist/broker/client.js';
import {connectionSchema} from '../dist/broker/onboarding.js';
import {createServer as createMcpServer} from '../dist/server.js';
function deferred(){let resolve;const promise=new Promise(r=>{resolve=r;});return {promise,resolve};}
async function fixture(t){
 let owner=false,project='C:/review/project-a',patch={},claimFault,claimStatus=409,connectionFault,paused;
 const calls=[],sessions=new Map(),clients=[];
 const server=createServer(async(req,res)=>{
  let text='';for await(const c of req)text+=c;const body=text?JSON.parse(text):{};
  const id=req.headers['x-request-id']??body.requestId,instance=req.headers['x-instance-id']??body.instanceId;
  calls.push({method:req.method,path:req.url});
  const send=data=>res.writeHead(200,{'content-type':'application/json'}).end(JSON.stringify({requestId:id,data}));
  const session=sessions.get(instance);
  if(req.url==='/v1/connection'){
   if(connectionFault)return res.writeHead(409,{'content-type':'application/json'}).end(JSON.stringify({requestId:id,error:connectionFault}));
   if(paused){paused.entered.resolve();await paused.release.promise;paused=undefined;}
   return send({observedAt:new Date().toISOString(),broker:'running',central:'Connected',identity:'verified',credentials:'valid',mcpConfig:'unknown',hostSession:owner?'conflict':session?.principal.agentId?'bound':'unbound',setupReady:false,collaborationReady:false,nextActions:owner?['inspect_connection_owner','review_mcp_config']:session?.principal.agentId?['review_mcp_config']:['register_agent','review_mcp_config'],...patch});
  }
  if(req.url==='/v1/context')return send({identity:'verified',principal:{userId:'1',teamId:'2',agentId:'3',toolType:'codex'},agent:{name:'Review',roleDescription:null,toolType:'codex',projectName:'Review',source:'central_agents_me'},project:{path:project,source:'bound_profile'},hostSession:session?.principal.agentId?'bound':'unbound'});
  if(req.url==='/v1/sessions'){
   if(owner||claimFault)return res.writeHead(claimStatus,{'content-type':'application/json'}).end(JSON.stringify({requestId:id,error:claimFault??{code:'SESSION_CONFLICT',scope:'session',outcome:'rejected'}}));
   const s={sessionId:'review-session',principal:{userId:'1',teamId:'2'}};sessions.set(instance,s);return send(s);
  }
  if(req.url==='/v1/agents/register')return send({agentId:'3'});
  if(req.url.endsWith('/bind')){session.principal.agentId='3';session.principal.toolType='codex';return send(session);}
  if(req.method==='DELETE'){sessions.delete(instance);return send({});}
  return send({items:[]});
 });
 await new Promise(r=>server.listen(0,'127.0.0.1',r));
 const settings={schemaVersion:1,brokerUrl:`http://127.0.0.1:${server.address().port}`,localToken:'synthetic-review-local-token'};
 const make=()=>{const c=new BrokerClient({settings,heartbeatMs:3600000,retryLimit:0});clients.push(c);return c;};
 t.after(async()=>{paused?.release.resolve();for(const c of clients)await c.close();server.closeAllConnections();await new Promise(r=>server.close(r));});
 return {calls,client:make(),make,owner(v){owner=v;},project(v){project=v;},patch(v){patch=v;},connectionFault(v){connectionFault=v;},claimFault(v,status=409){claimFault=v;claimStatus=status;},hold(){paused={entered:deferred(),release:deferred()};return paused;}};
}
const hasBlock=(result,code)=>{
 assert.equal(result.data.clientIssue.code,code);assert.equal(result.data.clientIssue.requestId,result.requestId);
 assert(result.data.nextActions.includes('open_or_restart_host'));assert(!result.data.nextActions.includes('register_agent'));
 assert.equal(result.data.nextActions.at(-1),'open_or_restart_host');assert.equal(result.data.collaborationReady,false);assert(connectionSchema.safeParse(result.data).success);
};
test('terminal owner conflict remains visible after the provider owner releases; diagnosis never unblocks it',async t=>{
 const f=await fixture(t);f.owner(true);await assert.rejects(f.client.execute('register_agent',{}),{code:'SESSION_CONFLICT'});f.owner(false);
 const before=f.calls.length,result=await f.client.getConnectionStatus({mcpRequestId:7});hasBlock(result,'SESSION_CONFLICT');
 assert.equal(result.mcpRequestId,7);assert.equal(result.data.hostSession,'unbound');assert.equal(result.data.identity,'verified');assert.equal(result.data.credentials,'valid');assert.equal(result.data.issue,undefined);
 assert.equal(result.data.clientIssue.scope,'session');assert(result.data.nextActions.includes('inspect_connection_owner'));
 assert.deepEqual(f.calls.slice(before),[{method:'GET',path:'/v1/connection'}]);
 const count=f.calls.length;await assert.rejects(f.client.execute('register_agent',{}),{code:'SESSION_CONFLICT'});assert.equal(f.calls.length,count);
 assert.equal((await f.make().execute('register_agent',{})).data.agentId,'3');
});
test('changed observed context reports a client block while preserving provider bound state',async t=>{
 const f=await fixture(t);await f.client.getMyContext();await f.client.execute('register_agent',{});
 f.project('C:/review/project-b');await assert.rejects(f.client.getMyContext(),{code:'BROKER_CONTEXT_INVALID'});
 const result=await f.client.getConnectionStatus();hasBlock(result,'BROKER_CONTEXT_INVALID');assert.equal(result.data.hostSession,'bound');assert.equal(result.data.identity,'verified');assert.equal(result.data.issue,undefined);
 assert(result.data.nextActions.includes('inspect_profile_then_rebind'));assert.equal(result.data.clientIssue.scope,'domain');
 const before=f.calls.length;for(const operation of ['get_tasks','register_agent'])await assert.rejects(f.client.execute(operation,{}),{code:'BROKER_CONTEXT_INVALID'});assert.equal(f.calls.length,before);
 f.project('C:/review/project-a');await f.client.getMyContext();hasBlock(await f.client.getConnectionStatus(),'BROKER_CONTEXT_INVALID');
});
test('terminal state established during an in-flight diagnosis is included before returning',async t=>{
 const f=await fixture(t),hold=f.hold(),pending=f.client.getConnectionStatus();await hold.entered.promise;
 f.owner(true);await assert.rejects(f.client.execute('register_agent',{}),{code:'SESSION_CONFLICT'});f.owner(false);hold.release.resolve();
 const result=await pending;hasBlock(result,'SESSION_CONFLICT');assert.equal(result.data.hostSession,'unbound');
});
test('a context contradiction detected during a diagnosis cannot be missed by an early-only guard',async t=>{
 const f=await fixture(t);await f.client.getMyContext();const hold=f.hold(),pending=f.client.getConnectionStatus();await hold.entered.promise;
 f.project('C:/review/project-b');await assert.rejects(f.client.getMyContext(),{code:'BROKER_CONTEXT_INVALID'});hold.release.resolve();hasBlock(await pending,'BROKER_CONTEXT_INVALID');
});
test('setup readiness remains environmental while a local block prevents collaboration',async t=>{
 const f=await fixture(t);await f.client.getMyContext();await f.client.execute('register_agent',{});
 f.patch({mcpConfig:'applied',setupReady:true,collaborationReady:true});assert.equal((await f.client.getConnectionStatus()).data.collaborationReady,true);
 f.project('C:/review/project-b');await assert.rejects(f.client.getMyContext(),{code:'BROKER_CONTEXT_INVALID'});
 const result=await f.client.getConnectionStatus();hasBlock(result,'BROKER_CONTEXT_INVALID');assert.equal(result.data.setupReady,true);assert.equal(result.data.mcpConfig,'applied');assert.equal(result.data.hostSession,'bound');
 assert.equal(connectionSchema.safeParse({...result.data,collaborationReady:true}).success,false);
});
test('provider issue and cached terminal scope/outcome are preserved independently',async t=>{
 const f=await fixture(t);f.claimFault({code:'SESSION_REPLACED',scope:'session',outcome:'unknown'});await assert.rejects(f.client.execute('register_agent',{}),{code:'SESSION_REPLACED'});
 const issue={code:'CENTRAL_UNAVAILABLE',scope:'transport',outcome:'unknown',requestId:'provider-diagnostic'};
 f.patch({central:'Disconnected',identity:'unknown',hostSession:'not_checked',issue,nextActions:['check_network_then_retry','register_agent']});
 const result=await f.client.getConnectionStatus();hasBlock(result,'SESSION_REPLACED');assert.deepEqual(result.data.issue,issue);assert.equal(result.data.clientIssue.outcome,'unknown');assert.equal(result.data.clientIssue.scope,'session');assert(result.data.nextActions.includes('check_network_then_retry'));
});
test('local auth and context failures require repair before reopening, without suggesting a claim',async t=>{
 for(const [fault,status,action] of [['BROKER_UNAUTHORIZED',401,'repair_private_file'],['LOCAL_AUTH_FORBIDDEN',403,'repair_private_file'],['BROKER_CONTEXT_INVALID',200,'inspect_profile_then_rebind']]){
  const f=await fixture(t);
  if(status!==200)f.claimFault({code:fault,scope:'local_auth',outcome:'rejected'},status);
  else{await f.client.getMyContext();f.project('C:/review/project-b');await assert.rejects(f.client.getMyContext(),{code:fault});}
  if(status!==200)await assert.rejects(f.client.execute('register_agent',{}),{code:fault});
  const result=await f.client.getConnectionStatus();hasBlock(result,fault);assert(result.data.nextActions.includes(action));
 }
});
test('a provider cannot invent a local client block, and normal diagnostics remain backward compatible',async t=>{
 const f=await fixture(t);assert.equal((await f.client.getConnectionStatus()).data.clientIssue,undefined);
 f.patch({clientIssue:{code:'SESSION_CONFLICT',scope:'session',outcome:'rejected',requestId:'injected'}});
 const result=await f.client.getConnectionStatus();assert.equal(result.data.issue.code,'BROKER_PROTOCOL_ERROR');assert.equal(result.data.clientIssue,undefined);assert.equal(f.client.state,'inactive');
});
test('cancellation and closure take precedence even when the client is terminal',async t=>{
 const f=await fixture(t);f.owner(true);await assert.rejects(f.client.execute('register_agent',{}),{code:'SESSION_CONFLICT'});const count=f.calls.length;
 await assert.rejects(f.client.getConnectionStatus({signal:AbortSignal.abort()}),{code:'REQUEST_ABORTED'});await f.client.close();await assert.rejects(f.client.getConnectionStatus(),{code:'CLIENT_CLOSED'});assert.equal(f.calls.length,count);
});
test('published MCP diagnostics include local blocks in both structured and text results',async t=>{
 const f=await fixture(t);f.owner(true);await assert.rejects(f.client.execute('register_agent',{}),{code:'SESSION_CONFLICT'});f.owner(false);
 const server=createMcpServer({debug(){},info(){},error(){}},f.client),client=new Client({name:'local-block-test',version:'1'},{versionNegotiation:{mode:'legacy'}});
 const [a,b]=InMemoryTransport.createLinkedPair();t.after(async()=>{await client.close();await server.close();});await Promise.all([server.connect(a),client.connect(b)]);
 const result=await client.callTool({name:'get_connection_status',arguments:{}});assert.notEqual(result.isError,true);assert.deepEqual(JSON.parse(result.content[0].text),result.structuredContent);hasBlock(result.structuredContent,'SESSION_CONFLICT');
});

test('cached settings failures distinguish missing, invalid and unknown without guessing a new profile',async()=>{
 const options=[
  [{},'missing','prepare_cli_profile'],
  [{settings:{schemaVersion:1,brokerUrl:'https://invalid.example',localToken:'synthetic-invalid-token'}},'invalid','repair_private_file'],
  // NUL causes lstat to fail with a non-ENOENT error on both supported path syntaxes.
  [{settingsPath:(process.platform==='win32'?'C:/review/':'/review/')+String.fromCharCode(0)+'broker.json'},'unknown',undefined],
 ];
 for(const [settings,state,action]of options){
  const client=new BrokerClient({...settings,retryLimit:0});
  try{
   await assert.rejects(client.execute('register_agent',{}),{code:'CREDENTIAL_FILE_INVALID'});
   const result=await client.getConnectionStatus();hasBlock(result,'CREDENTIAL_FILE_INVALID');assert.equal(result.data.credentials,state);
   if(action)assert(result.data.nextActions.includes(action));else assert.deepEqual(result.data.nextActions,['open_or_restart_host']);
   if(state!=='missing')assert(!result.data.nextActions.includes('prepare_cli_profile'));
  }finally{await client.close();}
 }
});
test('a provider session error cannot hide the client block or reinstate impossible registration advice',async t=>{
 const f=await fixture(t);f.owner(true);await assert.rejects(f.client.execute('register_agent',{}),{code:'SESSION_CONFLICT'});f.owner(false);
 f.connectionFault({code:'SESSION_EXPIRED',scope:'session',outcome:'rejected'});
 const count=f.calls.length,result=await f.client.getConnectionStatus();hasBlock(result,'SESSION_CONFLICT');
 assert.equal(result.data.issue.code,'SESSION_EXPIRED');assert.equal(result.data.issue.requestId,result.requestId);assert.equal(f.client.state,'SESSION_CONFLICT');
 assert.deepEqual(f.calls.slice(count),[{method:'GET',path:'/v1/connection'}]);
});
