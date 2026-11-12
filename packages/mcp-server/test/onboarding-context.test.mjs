import assert from 'node:assert/strict';
import test from 'node:test';
import { createServer } from 'node:http';
import { BrokerClient } from '../dist/broker/client.js';
import { contextSchema } from '../dist/broker/onboarding.js';
import { Client } from '@modelcontextprotocol/client';
import { InMemoryTransport } from '@modelcontextprotocol/server';
import { createServer as createMcpServer } from '../dist/server.js';

const verified = () => ({ identity:'verified', principal:{userId:'3',teamId:'1',agentId:'12',toolType:'codex'},
  agent:{name:'worker',roleDescription:null,toolType:'codex',projectName:'project',source:'central_agents_me'},
  project:{path:null,source:'unavailable'},hostSession:'unbound' });
async function fixture(t) {
  const calls=[]; let reply=verified();
  const server=createServer(async(req,res)=>{
    let body='';for await(const chunk of req) body+=chunk;
    calls.push({method:req.method,url:req.url,body,headers:req.headers});
    res.setHeader('content-type','application/json');
    const requestId=req.headers['x-request-id']??JSON.parse(body||'{}').requestId;
    const data=typeof reply==='function'?reply(req):reply;
    if(data.error)res.statusCode=data.error.code.startsWith('CENTRAL_')?503:422;
    res.end(JSON.stringify({requestId,...(data.error?data:{data})}));
  });
  await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
  const client=new BrokerClient({settings:{schemaVersion:1,brokerUrl:`http://127.0.0.1:${server.address().port}`,localToken:'synthetic-private-token'}});
  t.after(async()=>{await client.close();server.closeAllConnections();await new Promise(resolve=>server.close(resolve));});
  return {client,calls,set(value){reply=value;}};
}
test('context uses authenticated bodyless GET and never claims/binds/renews',async t=>{
  const {client,calls}=await fixture(t);
  const result=await client.getMyContext({mcpRequestId:7});
  assert.deepEqual(result.data,verified());assert.equal(result.mcpRequestId,7);
  await client.getMyContext();assert.equal(client.state,'inactive');
  assert.equal(calls.length,2);
  for(const call of calls){assert.equal(call.method,'GET');assert.equal(call.url,'/v1/context');assert.equal(call.body,'');assert(call.headers['x-instance-id']);}
});
test('published context tool is strictly no-argument and output/correlation matches actual calls',async t=>{
  const {client:broker,calls}=await fixture(t);
  const server=createMcpServer({debug(){},info(){},error(){}},broker);
  const client=new Client({name:'context-test',version:'1'},{versionNegotiation:{mode:'legacy'}});
  const [a,b]=InMemoryTransport.createLinkedPair();
  t.after(async()=>{await client.close();await server.close();});
  await Promise.all([server.connect(a),client.connect(b)]);
  const tool=(await client.listTools()).tools.find(t=>t.name==='get_my_context');
  assert.equal(tool.annotations.readOnlyHint,true);
  try {const result=await client.callTool({name:'get_my_context',arguments:{userId:'99'}});assert.equal(result.isError,true);}catch(e){assert.equal(e.code,-32602);}
  assert.equal(calls.length,0);
  const result=await client.callTool({name:'get_my_context',arguments:{}});
  assert.equal(result.isError,undefined);assert.deepEqual(result.structuredContent.data,verified());
  assert.equal(result.structuredContent.requestId,calls[0].headers['x-request-id']);
});
test('unknown metadata is explicit and malformed IDs/identity contradictions fail closed',async t=>{
  const {client,set}=await fixture(t);
  const missing=verified(); missing.agent.name=null;missing.agent.toolType=null;missing.principal.toolType=null;
  set(missing);assert.deepEqual((await client.getMyContext()).data,missing);
  for(const id of ['03','0','-1','9223372036854775808']){const v=verified();v.principal.userId=id;assert.equal(contextSchema.safeParse(v).success,false);}
  const bad=verified();bad.principal=null;set(bad);
  await assert.rejects(client.getMyContext(),{code:'BROKER_CONTEXT_INVALID'});
});
test('part and roles pass through only as the broker reported them',async t=>{
  const {client,set}=await fixture(t);
  const reported=verified();reported.agent.part='BACKEND';reported.agent.roles=['로그인','결제 API'];
  set(reported);assert.deepEqual((await client.getMyContext()).data,reported);
  // No tags is a reported value; an unknown part string must not fail the read.
  const cleared=verified();cleared.agent.part='DESIGN';cleared.agent.roles=[];
  set(cleared);assert.deepEqual((await client.getMyContext()).data,cleared);
  // Absent stays absent: nothing is defaulted to OTHER or [].
  set(verified());const absent=(await client.getMyContext()).data.agent;
  assert.equal('part' in absent,false);assert.equal('roles' in absent,false);
  for(const patch of [{part:null},{roles:null},{part:7},{roles:'로그인'},{roles:['로그인',3]}]){
    const v=verified();Object.assign(v.agent,patch);assert.equal(contextSchema.safeParse(v).success,false);
  }
});
for(const field of ['userId','teamId','agentId','project'])test(`changed ${field} cannot replace observed context`,async t=>{
  const {client,set}=await fixture(t);await client.getMyContext();const value=verified();
  if(field==='project')value.project={path:'C:\\other',source:'bound_profile'};else value.principal[field]='99';
  set(value);await assert.rejects(client.getMyContext(),{code:'BROKER_CONTEXT_INVALID'});
  assert.equal(client.state,'inactive');
  await assert.rejects(client.execute('list_agents',{}),{code:'BROKER_CONTEXT_INVALID'});
});
test('disconnected context has no fabricated principal or project',async t=>{
  const {client,set,calls}=await fixture(t);
  set({identity:'unavailable',principal:null,agent:null,project:{path:null,source:'unavailable'},hostSession:'not_checked'});
  assert.equal((await client.getMyContext()).data.identity,'unavailable');assert.equal(calls.length,1);
});
test('a later claim cannot change the subject observed by the read-only context tool',async t=>{
  const {client,set,calls}=await fixture(t);await client.getMyContext();
  set(()=>({sessionId:'session',principal:{userId:'99',teamId:'1'}}));
  await assert.rejects(client.execute('register_agent',{name:'worker',toolType:'codex'}),{code:'BROKER_CONTEXT_INVALID'});
  assert.equal(calls.filter(c=>c.url==='/v1/agents/register').length,0);
});
test('query auth/forbidden errors preserve a bound session and its heartbeat',async t=>{
  const {client,set,calls}=await fixture(t);
  const session={sessionId:'session',principal:{userId:'3',teamId:'1',agentId:'12'}};
  const healthy=req=>req.url==='/v1/agents/register'?{agentId:'12'}:session;
  set(healthy);await client.execute('register_agent',{name:'worker',toolType:'codex'});
  for(const code of ['BROKER_FORBIDDEN','CENTRAL_UNAUTHENTICATED','CENTRAL_UNAVAILABLE']) {
    set({error:{code,scope:code==='CENTRAL_UNAVAILABLE'?'transport':'domain',outcome:code==='CENTRAL_UNAVAILABLE'?'unknown':'rejected'}});
    await assert.rejects(client.getMyContext(),{code});assert.equal(client.state,'bound');
    set(healthy);await client.heartbeat();assert.equal(client.state,'bound');
    await client.execute('get_tasks',{});
  }
  assert.equal(calls.filter(c=>c.url.endsWith('/heartbeat')).length,3);
  assert.equal(calls.filter(c=>c.url==='/v1/sessions').length,1);
});
test('verified contradiction blocks work while retaining the bound lease and heartbeat',async t=>{
  const {client,set}=await fixture(t);await client.getMyContext();
  const session={sessionId:'session',principal:{userId:'3',teamId:'1',agentId:'12'}};
  const healthy=req=>req.url==='/v1/agents/register'?{agentId:'12'}:session;
  set(healthy);await client.execute('register_agent',{name:'worker',toolType:'codex'});
  const changed=verified();changed.principal.teamId='99';set(changed);
  await assert.rejects(client.getMyContext(),{code:'BROKER_CONTEXT_INVALID'});
  assert.equal(client.state,'bound');set(healthy);await client.heartbeat();
  assert.equal(client.state,'bound');await assert.rejects(client.execute('get_tasks',{}),{code:'BROKER_CONTEXT_INVALID'});
});
