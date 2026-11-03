import assert from 'node:assert/strict';
import test from 'node:test';
import {createServer} from 'node:http';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {setTimeout as delay} from 'node:timers/promises';
import {BrokerClient} from '../dist/broker/client.js';
import {SettingsFault} from '../dist/broker/settings.js';
import {connectionSchema} from '../dist/broker/onboarding.js';
import {Client} from '@modelcontextprotocol/client';
import {InMemoryTransport} from '@modelcontextprotocol/server';
import {createServer as createMcpServer} from '../dist/server.js';

const snapshot=()=>({observedAt:new Date().toISOString(),broker:'running',central:'Connected',identity:'verified',
 credentials:'valid',mcpConfig:'unknown',hostSession:'unbound',setupReady:false,collaborationReady:false,
 nextActions:['register_agent','review_mcp_config']});
async function fixture(t,options={}) {
 let reply=snapshot(),status=200,paused=false;const calls=[];
 const server=createServer(async(req,res)=>{
  let body='';for await(const chunk of req)body+=chunk;
  calls.push({method:req.method,path:req.url,body,id:req.headers['x-request-id'],instance:req.headers['x-instance-id']});
  if(paused)return;
  res.writeHead(status,{'content-type':'application/json'}).end(JSON.stringify({requestId:req.headers['x-request-id'],...(reply.error?reply:{data:reply})}));
 });
 await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
 const client=new BrokerClient({settings:{schemaVersion:1,brokerUrl:`http://127.0.0.1:${server.address().port}`,localToken:'synthetic-private-local-token'},...options});
 t.after(async()=>{await client.close();server.closeAllConnections();await new Promise(resolve=>server.close(resolve));});
 return {client,calls,set(value,code=200){reply=value;status=code;},pause(){paused=true;}};
}
test('missing/invalid credentials are safe partial diagnostics without setup or a claim',async()=>{
 for(const [options,state] of [[{},'missing'],[{settingsPath:join(tmpdir(),'devlink-nonexistent-'+crypto.randomUUID(),'broker.json')},'missing'],
  [{settings:{brokerUrl:'https://invalid.example/secret-sentinel'}},'invalid']]) {
  const client=new BrokerClient(options);const result=await client.getConnectionStatus({mcpRequestId:0});
  assert.equal(result.mcpRequestId,0);assert.equal(result.data.credentials,state);assert.equal(result.data.broker,'unknown');
  assert.equal(result.data.mcp,'running');assert.equal(result.data.setupReady,false);assert.equal(client.state,'inactive');
  assert.equal(result.data.issue.requestId,result.requestId);
  assert(!JSON.stringify(result).includes('secret-sentinel'));await client.close();
 }
});
test('diagnosis uses bodyless authenticated GET and does not bootstrap or retry sessions',async t=>{
 const {client,calls}=await fixture(t);const result=await client.getConnectionStatus();await client.getConnectionStatus();
 assert.equal(result.data.identity,'verified');assert.equal(result.data.mcpConfig,'unknown');assert.equal(result.data.setupReady,false);
 assert.equal(client.state,'inactive');assert.equal(calls.length,2);
 for(const call of calls){assert.equal(call.method,'GET');assert.equal(call.path,'/v1/connection');assert.equal(call.body,'');assert(call.instance);}
 assert.equal(result.requestId,calls[0].id);
});
test('the published diagnostic tool accepts no selectors or commands',async t=>{
 const {client:broker,calls}=await fixture(t);
 const server=createMcpServer({debug(){},info(){},error(){}},broker);
 const client=new Client({name:'diagnostic-test',version:'1'},{versionNegotiation:{mode:'legacy'}});
 const [a,b]=InMemoryTransport.createLinkedPair();t.after(async()=>{await client.close();await server.close();});
 await Promise.all([server.connect(a),client.connect(b)]);
 assert.equal((await client.listTools()).tools.find(t=>t.name==='get_connection_status').annotations.readOnlyHint,true);
 try{const result=await client.callTool({name:'get_connection_status',arguments:{command:'start',userId:'99'}});assert.equal(result.isError,true);}catch(e){assert.equal(e.code,-32602);}
 assert.equal(calls.length,0);
 const result=await client.callTool({name:'get_connection_status',arguments:{}});
 assert.equal(result.isError,undefined);assert.equal(result.structuredContent.data.mcp,'running');assert.equal(calls.length,1);
});
test('unreachable broker is unknown rather than falsely confirmed stopped or ready',async t=>{
 const {client}=await fixture(t,{settings:{schemaVersion:1,brokerUrl:'http://127.0.0.1:1',localToken:'synthetic-private-local-token'}});
 const result=await client.getConnectionStatus();assert.equal(result.data.broker,'unknown');assert.equal(result.data.issue.code,'BROKER_UNAVAILABLE');
 assert.equal(result.data.credentials,'unknown');assert.deepEqual(result.data.nextActions,['inspect_local_broker']);
});
test('unknown stages and inconsistent readiness fail closed without raw provider text',async t=>{
 const {client,set}=await fixture(t);
 for(const value of [{...snapshot(),setupReady:true},{...snapshot(),central:'raw-secret-sentinel'},
   {...snapshot(),hostSession:'bound',identity:'unknown'},{...snapshot(),observedAt:'invalid'}]) {
  set(value);const result=await client.getConnectionStatus();assert.equal(result.data.issue.code,'BROKER_PROTOCOL_ERROR');
  assert.equal(result.data.broker,'unhealthy');assert.equal(result.data.setupReady,false);assert(!JSON.stringify(result).includes('sentinel'));
 }
 assert.equal(connectionSchema.safeParse({...snapshot(),mcp:'running',hostSession:'bound',mcpConfig:'applied',setupReady:true,collaborationReady:true}).success,true);
});
test('local denial and original central/session error metadata remain distinct',async t=>{
 const {client,set,calls}=await fixture(t);
 for(const [code,scope,outcome,status,action] of [['BROKER_UNAUTHORIZED','local_auth','rejected',401,'repair_private_file'],
  ['CENTRAL_UNAUTHENTICATED','domain','rejected',503,'stop_then_reauthenticate_agent'],
  ['CENTRAL_TIMEOUT','transport','unknown',503,'check_network_then_retry'],
  ['BROKER_CONTEXT_UNAVAILABLE','domain','unknown',503,'inspect_profile_then_rebind'],
  ['SESSION_EXPIRED','session','rejected',409,'register_agent'],['SESSION_CONFLICT','session','rejected',409,'inspect_connection_owner']]) {
  set({error:{code,scope,outcome,message:'secret-sentinel'}},status);
  const result=await client.getConnectionStatus();assert.equal(result.data.issue.code,code);assert.equal(result.data.issue.outcome,outcome);
  assert.equal(result.data.issue.scope,scope);
  assert.equal(result.data.issue.requestId,calls.at(-1).id);assert.deepEqual(result.data.nextActions,[action,...(action==='repair_private_file'?['open_or_restart_host']:[])]);
  assert.equal(client.state,'inactive');assert(!JSON.stringify(result).includes('sentinel'));
 }
});
test('bounded diagnosis timeout does not schedule background recovery',async t=>{
 const {client,calls,pause}=await fixture(t,{requestTimeoutMs:25,retryBaseMs:1});pause();
 const result=await client.getConnectionStatus();assert.equal(result.data.issue.code,'BROKER_TIMEOUT');
 await delay(50);assert.equal(calls.length,1);assert.equal(client.pendingCount,0);assert.equal(client.state,'inactive');
});
test('cancelled and closed diagnostic calls do not perform any IO',async t=>{
 const {client,calls}=await fixture(t);const signal=AbortSignal.abort();
 await assert.rejects(client.getConnectionStatus({signal}),{code:'REQUEST_ABORTED'});
 await client.close();await assert.rejects(client.getConnectionStatus(),{code:'CLIENT_CLOSED'});assert.equal(calls.length,0);
});
test('local credential errors receive unique call IDs in both read tools',async()=>{
 const client=new BrokerClient();const ids=[];
 for(let i=0;i<2;i++) {
  const value=await client.getConnectionStatus();ids.push(value.requestId);assert.equal(value.data.issue.requestId,value.requestId);
  try{await client.getMyContext();assert.fail('must reject');}catch(e){assert.equal(e.code,'CREDENTIAL_FILE_INVALID');assert.notEqual(e.requestId,'settings');ids.push(e.requestId);}
 }
 assert.equal(new Set(ids).size,4);await client.close();
});
test('unobservable credential reads and a settings deadline cannot be labelled invalid',async()=>{
 // Deterministic transport-load fault injection avoids machine-dependent ACL/race timing.
 const client=new BrokerClient();client.transport=async()=>{throw new SettingsFault('unknown');};
 const unknown=await client.getConnectionStatus();assert.equal(unknown.data.credentials,'unknown');assert.equal(unknown.data.issue.requestId,unknown.requestId);
 await client.close();
 const slow=new BrokerClient({callTimeoutMs:10});slow.transport=()=>new Promise(()=>{});
 const timed=await slow.getConnectionStatus();assert.equal(timed.data.credentials,'unknown');assert.equal(timed.data.issue.code,'BROKER_TIMEOUT');
 assert.equal(timed.data.issue.requestId,timed.requestId);await slow.close();
 const cancelled=new BrokerClient();cancelled.transport=()=>new Promise(()=>{});
 const controller=new AbortController();const pending=cancelled.getConnectionStatus({signal:controller.signal});controller.abort();
 await assert.rejects(pending,{code:'REQUEST_ABORTED'});await cancelled.close();
});
