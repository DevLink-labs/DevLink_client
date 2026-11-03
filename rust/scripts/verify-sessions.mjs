// Isolated actual central + PostgreSQL + actual CLI. No production credentials.
import assert from 'node:assert/strict';
import { randomUUID } from 'node:crypto';
import { spawn, spawnSync } from 'node:child_process';
import { mkdtemp,readFile,rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join,isAbsolute,dirname,resolve } from 'node:path';
import { setTimeout as pause } from 'node:timers/promises';
import { registerHooks } from 'node:module';

const packageUrl=new URL('../../packages/mcp-server/package.json',import.meta.url).href;
const distUrl=new URL('../../packages/mcp-server/dist/',import.meta.url).href;
registerHooks({resolve(specifier,context,nextResolve){
  if((context.parentURL===import.meta.url||context.parentURL?.startsWith(distUrl))&&!specifier.startsWith('.')&&!specifier.includes(':')){
    return nextResolve(specifier,{...context,parentURL:packageUrl});
  }return nextResolve(specifier,context);
}});
const {Client}=await import('@modelcontextprotocol/client');
const {InMemoryTransport}=await import('@modelcontextprotocol/server');
const {BrokerClient}=await import(new URL('broker/client.js',distUrl));
const {createServer}=await import(new URL('server.js',distUrl));

const base=new URL(process.env.DEVLINK_TEST_CENTRAL_URL ?? '');
assert(base.protocol==='http:' && base.hostname==='127.0.0.1' && base.pathname==='/' && !base.username && !base.password && !base.search && !base.hash);
const cli=process.env.DEVLINK_TEST_CLI,psql=process.env.DEVLINK_TEST_PSQL;
assert(cli&&psql&&isAbsolute(cli)&&isAbsolute(psql));
const temp=await mkdtemp(join(tmpdir(),'devlink-session-real-'));
const config=join(temp,'config.json');
const env=Object.fromEntries(Object.entries(process.env).filter(([k])=>!/^DEVLINK_/i.test(k)));
const secrets=[];let child;let stderr='';let localToken;let brokerUrl;let cookie;let csrf;
function run(args) {
  const r=spawnSync(cli,['--config',config,...args],{env,encoding:'utf8',windowsHide:true,timeout:15000});
  assert.equal(r.status,0,`CLI ${args[0]} failed`);
  return r.stdout;
}
async function api(path,body) {
  const r=await fetch(new URL(path,base),{method:body?'POST':'GET',headers:{...(cookie?{Cookie:cookie}:{}),...(body?{'Content-Type':'application/json',[csrf.headerName]:csrf.token}:{})},body:body?JSON.stringify(body):undefined,redirect:'error',signal:AbortSignal.timeout(8000)});
  const v=await r.json();assert(r.ok&&v.success,`central ${path} ${r.status}`);
  const session=r.headers.getSetCookie().find(c=>c.startsWith('JSESSIONID='));
  if(session) {cookie=session.split(';')[0];secrets.push(cookie);}
  return v.data;
}
async function local(path,body,code,method='POST') {
  const requestId=randomUUID();
  const r=await fetch(`${brokerUrl}${path}`,{method,headers:{authorization:`Bearer ${localToken}`,'content-type':'application/json'},body:JSON.stringify({...body,requestId}),signal:AbortSignal.timeout(6000)});
  const v=await r.json();assert.equal(v.requestId,requestId);assert.equal(r.ok,!code,`local ${path}: ${v.error?.code}`);
  if(code)assert.equal(v.error.code,code);
  return v.data;
}
function sql(query) {
  const r=spawnSync(psql,['-X','-At','-h','127.0.0.1','-p',process.env.DEVLINK_TEST_DB_PORT??'55432','-U','devlink_session_test','-d','devlink_session_test','-c',query],{encoding:'utf8',windowsHide:true,timeout:5000});
  assert.equal(r.status,0,'DB query failed');return r.stdout.trim();
}
async function wait(predicate,label,ms=10000) {
  const end=Date.now()+ms;while(Date.now()<end) {if(await predicate())return;await pause(100);}assert.fail(label);
}
async function startBroker(token) {
  child=spawn(cli,['--config',config,'broker'],{env:{...env,DEVLINK_TOKEN:token},windowsHide:true,stdio:['ignore','ignore','pipe']});
  child.stderr.on('data',b=>{stderr+=b.toString();});
  await wait(async()=>{
    try {const descriptor=JSON.parse(await readFile(join(temp,'.devlink-runtime','broker.json'),'utf8'));localToken=descriptor.localToken;brokerUrl=descriptor.brokerUrl;
      const r=await fetch(`${brokerUrl}/health`,{headers:{authorization:`Bearer ${localToken}`}});return(await r.json()).centralServer==='Connected';}catch{return false;}
  },'CLI did not connect');
}
try {
  await wait(async()=>{try{return(await fetch(new URL('/api/v1/health',base),{signal:AbortSignal.timeout(1000)})).ok;}catch{return false;}},'Central did not start',30000);
  csrf=await api('/api/v1/auth/csrf');
  const email=`sessions-${randomUUID()}@example.test`,password=`Test-${randomUUID()}`;secrets.push(password);
  await api('/api/v1/auth/signup',{email,password,name:'Session integration'});
  const user=await api('/api/v1/auth/login',{email,password});csrf=await api('/api/v1/auth/csrf');
  const team=await api('/api/v1/teams',{name:'Session integration'});
  const agent=await api(`/api/v1/agents/teams/${team.teamId}`,{name:'Session integration',toolType:'CODEX',projectName:'synthetic-session-project'});
  assert(Number.isSafeInteger(agent.agentId)&&agent.agentId>0);
  const credential=await api(`/api/v1/agents/${agent.agentId}/token`,{});secrets.push(credential.token);
  run(['init']);run(['config','set','serverUrl',base.toString().replace(/^http:/,'ws:')]);
  await startBroker(credential.token);secrets.push(localToken);
  const aid=String(agent.agentId);
  const status=()=>sql(`SELECT status FROM agents WHERE id=${aid}`);
  const seen=()=>sql(`SELECT last_seen_at IS NOT NULL FROM agents WHERE id=${aid}`);
  assert.equal(status(),'OFFLINE');
  const unbound=await local('/v1/sessions',{instanceId:'first',mode:'fresh'});
  assert.deepEqual(unbound.principal,{userId:String(user.userId),teamId:String(team.teamId)});
  await local(`/v1/sessions/${unbound.sessionId}/bind`,{instanceId:'first',agentId:'9223372036854775807'},'FORBIDDEN');
  await local(`/v1/sessions/${unbound.sessionId}/bind`,{instanceId:'first',agentId:aid});
  await wait(()=>status()==='ONLINE','DB did not become ONLINE');assert.equal(seen(),'t');
  const owner=await local('/v1/sessions',{instanceId:'second',mode:'fresh',agentId:aid});
  await local(`/v1/sessions/${unbound.sessionId}/heartbeat`,{instanceId:'first'},'SESSION_REPLACED');
  await local(`/v1/sessions/${unbound.sessionId}`,{instanceId:'first'},'SESSION_REPLACED','DELETE');
  await local('/v1/sessions',{instanceId:'late',mode:'recover',agentId:aid},'SESSION_CONFLICT');
  assert.equal((await local('/v1/sessions',{instanceId:'second',mode:'recover',agentId:aid})).sessionId,owner.sessionId);
  await local(`/v1/sessions/${owner.sessionId}/heartbeat`,{instanceId:'second'});assert.equal(status(),'ONLINE');
  await local(`/v1/sessions/${owner.sessionId}`,{instanceId:'second'},undefined,'DELETE');
  await wait(()=>status()==='OFFLINE','DELETE did not persist OFFLINE');
  const adapter=new BrokerClient({settings:{schemaVersion:1,brokerUrl,localToken,agentId:aid}});
  const mcp=createServer({debug(){},info(){},error(){}},adapter);
  const sdk=new Client({name:'actual-central-sessions',version:'1'},{versionNegotiation:{mode:'legacy'}});
  const [transportA,transportB]=InMemoryTransport.createLinkedPair();
  try {
    await Promise.all([mcp.connect(transportA),sdk.connect(transportB)]);
    const result=await sdk.callTool({name:'list_agents',arguments:{}});
    // list_agents provider is a separate story and is currently absent. The
    // preceding real session claim must still succeed against real identity.
    assert.equal(result.isError,true);assert.equal(adapter.state,'bound');
    assert.equal(result.structuredContent.error.code,'BROKER_PROTOCOL_ERROR');
    await wait(()=>status()==='ONLINE','SDK session did not persist ONLINE');
  }finally {await sdk.close();await adapter.close();await mcp.close();}
  await wait(()=>status()==='OFFLINE','SDK close did not persist OFFLINE');
  const previous=await local('/v1/sessions',{instanceId:'before-restart',mode:'fresh',agentId:aid});
  await wait(()=>status()==='ONLINE','pre-restart owner not ONLINE');
  run(['stop']);await wait(()=>status()==='OFFLINE','broker shutdown did not persist OFFLINE');
  await startBroker(credential.token);
  await local(`/v1/sessions/${previous.sessionId}/heartbeat`,{instanceId:'before-restart'},'SESSION_EXPIRED');
  const winner=await local('/v1/sessions',{instanceId:'first-recover',mode:'recover',agentId:aid});
  await local('/v1/sessions',{instanceId:'before-restart',mode:'recover',agentId:aid},'SESSION_CONFLICT');
  await local(`/v1/sessions/${winner.sessionId}`,{instanceId:'first-recover'},undefined,'DELETE');
  const expiring=await local('/v1/sessions',{instanceId:'expiring',mode:'fresh',agentId:aid});
  await wait(()=>status()==='ONLINE','expiry owner not ONLINE');
  await pause(91_000);
  await wait(()=>status()==='OFFLINE','lease did not persist OFFLINE');
  await local(`/v1/sessions/${expiring.sessionId}/heartbeat`,{instanceId:'expiring'},'SESSION_EXPIRED');
  const recovered=await local('/v1/sessions',{instanceId:'expiring',mode:'recover',agentId:aid});
  await wait(()=>status()==='ONLINE','recovered owner not ONLINE');
  await api(`/api/v1/agents/${agent.agentId}/token`,{});
  await wait(()=>status()==='OFFLINE','rotated token did not persist OFFLINE');
  // Rotation may already close WS; either auth or transport refusal is valid.
  const r=await fetch(`${brokerUrl}/v1/sessions/${recovered.sessionId}/heartbeat`,{method:'POST',headers:{authorization:`Bearer ${localToken}`,'content-type':'application/json'},body:JSON.stringify({requestId:'revoked',instanceId:'expiring'})});
  assert(!r.ok);const denial=await r.json();assert(['CENTRAL_UNAUTHENTICATED','CENTRAL_UNAVAILABLE'].includes(denial.error.code));
  assert(secrets.every(s=>!stderr.includes(s)),'CLI secret exposure');
  console.log(JSON.stringify({passed:true,actualCentral:true,actualPostgres:true,actualSdkSession:true,businessListProvider:'not provided (404)',leaseSeconds:90,checks:['principal','bind','forgery rejection','fresh/recover','stale heartbeat/delete','DB ONLINE/last_seen_at','DELETE OFFLINE','SDK claim/close DB presence','broker restart first recovery wins','lease OFFLINE','recovery','token rotation refusal'],secretOutput:false}));
} finally {
  if(child&&child.exitCode===null){child.kill();await Promise.race([new Promise(r=>child.once('exit',r)),pause(5000)]);}
  assert.equal(dirname(resolve(temp)),resolve(tmpdir()));await rm(temp,{recursive:true,force:true});
}
