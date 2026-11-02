import assert from 'node:assert/strict';
import test from 'node:test';
import {BrokerClient} from '../dist/broker/client.js';
import {Client} from '@modelcontextprotocol/client';
import {InMemoryTransport} from '@modelcontextprotocol/server';
import {createServer} from '../dist/server.js';
import {fakeBroker} from './helpers/fake-broker.mjs';
async function fixture(t) {
 const broker=await fakeBroker();const adapters=[];
 const adapter=(agentId)=>{const client=new BrokerClient({settings:{...broker.settings,...(agentId?{agentId}:{})},retryBaseMs:60000});adapters.push(client);return client;};
 t.after(async()=>{await Promise.all(adapters.map(c=>c.close()));await broker.close();});
 return {broker,adapter};
}
test('register_agent({}) and independently optional legacy fields use the actual published schema',async t=>{
 const {broker,adapter}=await fixture(t);const backend=adapter();
 const server=createServer({debug(){},info(){},error(){}},backend);
 const client=new Client({name:'registration-test',version:'1'},{versionNegotiation:{mode:'legacy'}});
 const [a,b]=InMemoryTransport.createLinkedPair();t.after(async()=>{await client.close();await server.close();});
 await Promise.all([server.connect(a),client.connect(b)]);
 for(const args of [{},{name:'worker'},{toolType:'codex'},{role:'role'},{name:'worker',toolType:'codex',role:'role'}]) {
  const result=await client.callTool({name:'register_agent',arguments:args});assert.equal(result.isError,undefined);
  assert.equal(result.structuredContent.data.agentId,'registered-agent');
  assert.deepEqual(broker.calls.findLast(c=>c.path==='/v1/agents/register').body.arguments,args);
 }
 const count=broker.calls.length;
 for(const args of [{name:null},{toolType:null},{role:null},{name:''},{name:' '},{toolType:'CODEX'},{role:' '},{agentId:'other'}]) {
  try{const result=await client.callTool({name:'register_agent',arguments:args});assert.equal(result.isError,true);}catch(e){assert.equal(e.code,-32602);}
 }
 assert.equal(broker.calls.length,count);
 assert(broker.calls.filter(c=>c.path==='/v1/sessions').every(c=>c.body.mode==='recover'));
});
test('explicit no-argument registration never replaces an active owner, with or without legacy selector',async t=>{
 const {broker,adapter}=await fixture(t);const owner=adapter();await owner.execute('register_agent',{});
 const winning=broker.owners.get('registered-agent');
 for(const selector of [undefined,'registered-agent']) {
  const other=adapter(selector);await assert.rejects(other.execute('register_agent',{}),{code:'SESSION_CONFLICT'});
  assert.equal(broker.owners.get('registered-agent'),winning);await owner.heartbeat();
 }
});
test('simultaneous registrations have one owner and preserve the same confirmed Agent',async t=>{
 const {broker,adapter}=await fixture(t);const a=adapter(),b=adapter();
 const results=await Promise.allSettled([a.execute('register_agent',{}),b.execute('register_agent',{})]);
 assert.equal(results.filter(r=>r.status==='fulfilled').length,1);
 assert.equal(results.find(r=>r.status==='rejected').reason.code,'SESSION_CONFLICT');
 assert.equal(results.find(r=>r.status==='fulfilled').value.data.agentId,'registered-agent');
 assert.equal(broker.owners.size,1);
});
test('parallel business bootstrap cannot weaken registration owner protection',async t=>{
 const {broker,adapter}=await fixture(t);const owner=adapter('registered-agent');await owner.execute('get_tasks',{});
 const winning=broker.owners.get('registered-agent'),other=adapter('registered-agent');
 const results=await Promise.allSettled([other.execute('list_agents',{}),other.execute('register_agent',{})]);
 assert(results.every(r=>r.status==='rejected'));assert.equal(broker.owners.get('registered-agent'),winning);
 assert(broker.calls.filter(c=>c.path==='/v1/sessions').every(c=>c.body.mode==='recover'));
});
test('repeated, concurrent and restarted registration reuses identity; recovery never replays confirmation',async t=>{
 const {broker,adapter}=await fixture(t);let client=adapter();
 const results=await Promise.all([client.execute('register_agent',{}),client.execute('register_agent',{})]);
 assert(results.every(r=>r.data.agentId==='registered-agent'));
 const registrations=broker.calls.filter(c=>c.path==='/v1/agents/register').length;
 broker.restart();await assert.rejects(client.heartbeat(),{code:'SESSION_EXPIRED'});await client.heartbeat();
 assert.equal(client.state,'bound');assert.equal(broker.calls.filter(c=>c.path==='/v1/agents/register').length,registrations);
 await client.close();client=adapter();assert.equal((await client.execute('register_agent',{})).data.agentId,'registered-agent');
 assert.equal(broker.owners.size,1);assert(broker.calls.every(c=>c.path.startsWith('/v1/')));
});
