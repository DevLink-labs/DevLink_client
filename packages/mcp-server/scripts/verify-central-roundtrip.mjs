// S15P31D108-97: opt-in SDK -> MCP stdio -> Rust -> real loopback Spring/PostgreSQL.
import assert from 'node:assert/strict';
import {randomUUID} from 'node:crypto';
import {spawn} from 'node:child_process';
import {mkdtemp,mkdir,readFile,writeFile,rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join,resolve,dirname,basename,isAbsolute} from 'node:path';
import {setTimeout as delay} from 'node:timers/promises';
import {assertRequiredTools,callTool,connectMcpStdio,withoutDevlinkEnv} from './lib/mcp-stdio.mjs';

const base=new URL(process.env.DEVLINK_TEST_CENTRAL_URL??'');
assert(base.protocol==='http:'&&['localhost','127.0.0.1','[::1]'].includes(base.hostname)&&base.pathname==='/'&&!base.search&&!base.hash&&!base.username&&!base.password,'Only an isolated loopback server is allowed');
const cli=process.env.DEVLINK_TEST_CLI;
assert(cli&&isAbsolute(cli),'DEVLINK_TEST_CLI must name the built CLI');
const serverCommit=process.env.DEVLINK_TEST_CENTRAL_COMMIT;
assert(/^[a-f0-9]{40}$/.test(serverCommit??''),'Record the exact server source commit');
const env=withoutDevlinkEnv();
const root=await mkdtemp(join(tmpdir(),'devlink-central97-'));
const profiles=[], clients=[], secrets=[], checks=[];
let cleanupFailed=false, failure;
function safe(value){assert(secrets.every(secret=>!secret||!String(value).includes(secret)),'Secret appeared in shareable output');}
async function raw(p,args,input) {
  return new Promise((res,rej)=>{
    const child=spawn(cli,['--config',p.config,...args],{env,windowsHide:true,stdio:['pipe','pipe','pipe']});
    let out='',err='',overflow=false;const timer=setTimeout(()=>child.kill(),65000);
    child.stdout.on('data',chunk=>{out+=chunk;if(out.length>2_000_000){overflow=true;child.kill();}});
    child.stderr.on('data',chunk=>{err+=chunk;if(err.length>2_000_000){overflow=true;child.kill();}});
    child.on('error',e=>{clearTimeout(timer);rej(e);});
    child.on('close',code=>{clearTimeout(timer);try{safe(out+err);const codes=[...new Set((out+err).match(/\b[A-Z][A-Z_]{3,}\b/g)??[])].join(',');assert.equal(code,0,'CLI failed: '+p.label+' '+args.slice(0,2).join(' ')+' '+codes);assert(!overflow);res(out);}catch(e){rej(e);}});
    child.stdin.end(input??'');
  });
}
async function run(p,args,input){return JSON.parse(await raw(p,args,input));}
async function saved(p,name){return JSON.parse(await readFile(join(p.folder,'.devlink-runtime',name),'utf8'));}
async function until(operation,predicate,label) {
 const end=Date.now()+15000;
 while(Date.now()<end){const value=await operation();if(predicate(value))return value;await delay(100);}
 throw new Error('Timed out: '+label);
}
async function user(label,tool) {
 const folder=join(root,label);await mkdir(folder);const p={folder,config:join(folder,'config.json'),label,tool,name:'  Roundtrip '+label+'  '};profiles.push(p);
 const email='central97-'+randomUUID()+'@example.test',password='Test-'+randomUUID();secrets.push(password);
 const response=await fetch(new URL('/api/v1/auth/csrf',base));assert(response.ok);
 const csrf=(await response.json()).data;
 const cookie=response.headers.getSetCookie().find(value=>value.startsWith('JSESSIONID=')).split(';')[0];secrets.push(cookie);
 const signup=await fetch(new URL('/api/v1/auth/signup',base),{method:'POST',headers:{Cookie:cookie,'Content-Type':'application/json',[csrf.headerName]:csrf.token},body:JSON.stringify({email,password,name:'Synthetic central97 '+label})});
 assert.equal(signup.status,201);
 await raw(p,['init']);await raw(p,['config','set','serverUrl',base.toString().replace(/^http:/,'ws:')]);
 await raw(p,['config','set','localPort','0']);
 p.user=(await run(p,['auth','login','--email',email,'--password-stdin'],password+'\n')).user;
 secrets.push((await saved(p,'user-session.json')).state.cookie);return p;
}
async function agent(p) {
 const created=await run(p,['agent','create','--name',p.name,'--tool',p.tool==='codex'?'codex':'claude-code','--project-name','Isolated '+p.label,'--project',p.folder]);
 assert.equal(created.state,'ready');p.agent=created.agent;
 assert.equal(p.agent.ownerUserId,p.user.userId);
 p.token=(await saved(p,'agent-profile.json')).state.token;secrets.push(p.token);
 const again=await run(p,['agent','create','--name',p.name,'--tool',p.tool==='codex'?'codex':'claude-code','--project-name','Isolated '+p.label,'--project',p.folder]);
 assert.equal(again.agent.agentId,p.agent.agentId,'Agent create must reuse its known ID');
}
async function central(p,path,body,method) {
 const r=await fetch(new URL(path,base),{method:method??(body?'POST':'GET'),headers:{Authorization:'Bearer '+p.token,...(body?{'Content-Type':'application/json'}:{})},...(body?{body:JSON.stringify(body)}:{})});
 assert(r.ok,'Central request rejected: '+r.status);const value=await r.json();assert(value.success);return value.data;
}
async function start(p) {
 p.started=true;await raw(p,['start']);
 const runtime=await saved(p,'runtime.json');secrets.push(runtime.controlToken);
 p.descriptor=await saved(p,'broker.json');secrets.push(p.descriptor.localToken);
 await until(async()=> (await fetch(p.descriptor.brokerUrl+'/health',{headers:{Authorization:'Bearer '+p.descriptor.localToken}})).json(),h=>h.centralServer==='Connected','central WebSocket '+p.label);
 p.mcp=await connectMcpStdio({brokerFile:join(p.folder,'.devlink-runtime','broker.json'),name:'central97-'+p.label,env});
 p.client=p.mcp.client;clients.push(p.client);
 await assertRequiredTools(p.client);
 const diagnostic=await call(p,'get_connection_status',{});
 assert.equal(diagnostic.data.broker,'running');assert.equal(diagnostic.data.central,'Connected');
 assert.equal(diagnostic.data.identity,'verified');assert.equal(diagnostic.data.credentials,'valid');
 assert.equal(diagnostic.data.hostSession,'unbound');assert.equal(diagnostic.data.mcpConfig,'unknown');assert.equal(diagnostic.data.setupReady,false);
 const context=await call(p,'get_my_context',{});
 assert.equal(context.data.identity,'verified');assert.equal(context.data.principal.userId,String(p.user.userId));
 assert.equal(context.data.principal.teamId,String(p.agent.teamId));assert.equal(context.data.principal.agentId,String(p.agent.agentId));
 assert.equal(context.data.principal.toolType,p.tool);assert.equal(context.data.hostSession,'unbound');
 assert.equal(context.data.project.source,'bound_profile');
 assert.equal(context.data.project.path,(await saved(p,'agent-profile.json')).projectRoot);
 const tokenBefore=(await saved(p,'agent-profile.json')).state.token;
 const registered=await call(p,'register_agent',{});
 assert.equal(registered.data.agentId,String(p.agent.agentId));
 const repeats=await Promise.all([call(p,'register_agent',{}),call(p,'register_agent',{})]);
 assert(repeats.every(r=>r.data.agentId===String(p.agent.agentId)));
 assert.equal((await call(p,'register_agent',{name:p.name,toolType:p.tool})).data.agentId,String(p.agent.agentId));
 for(const args of [{name:'wrong-name'},{toolType:p.tool==='codex'?'claude_code':'codex'},{role:'wrong-role'}]) {
  const denied=await p.client.callTool({name:'register_agent',arguments:args});assert.equal(denied.isError,true);
  assert.equal(denied.structuredContent.error.code,'VALIDATION_FAILED');assert.equal(denied.structuredContent.error.scope,'domain');
 }
 assert.equal((await saved(p,'agent-profile.json')).state.token,tokenBefore,'Registration cannot rotate the CLI credential');
 assert.equal((await call(p,'get_my_context',{})).data.hostSession,'bound');
 assert.equal((await call(p,'get_connection_status',{})).data.hostSession,'bound');
 await until(()=>central(p,'/api/v1/agents/me'),a=>a.status==='ONLINE','ONLINE presence '+p.label);
}
async function call(p,name,args,expectedCode){return callTool(p.client,name,args,expectedCode);}
async function detail(p,taskId){return (await call(p,'get_task_result',{taskId})).data;}
async function exchange(sender,receiver,label) {
 const args={recipientAgentId:String(receiver.agent.agentId),title:'  '+label+' 컨텍스트  ',description:'  요청 설명\n한글/emoji 🧪  ',taskContext:'  project='+receiver.label+'\nfiles=src/context.txt\nnonce='+randomUUID()+'\n수신자에게만 전달  '};
 const made=await call(sender,'create_task',args);assert.equal(made.data.status,'PENDING');assert(made.requestKey);
 const taskId=made.data.taskId;
 await until(()=>detail(sender,taskId),d=>d.events.some(e=>e.eventType==='ACK_RECEIVED'),'durable inbox ACK');
 const inbox=await run(receiver,['inbox','list','--limit','1024']);
 const receipt=inbox.find(item=>String(item.message.payload.task_id)===taskId);
 assert(receipt,'target receiver did not persist the context message');
 assert.deepEqual(JSON.parse(receipt.message.payload.prompt),{title:args.title,description:args.description,taskContext:args.taskContext});
 assert.equal(String(receipt.message.payload.requester_agent_id),String(sender.agent.agentId));
 assert.equal(String(receipt.message.payload.receiver_agent_id),String(receiver.agent.agentId));
 const otherInbox=await run(sender,['inbox','list','--limit','1024']);
 assert(!otherInbox.some(item=>String(item.message.payload.task_id)===taskId),'request reached the wrong inbox');
 let d=await detail(receiver,taskId);assert.deepEqual(JSON.parse(d.prompt),{title:args.title,description:args.description,taskContext:args.taskContext});
 const denied=await call(sender,'update_task_status',{taskId,version:d.version,status:'IN_PROGRESS'},'BROKER_FORBIDDEN');assert.equal(denied.error.taskId,taskId);
 const accepted=await call(receiver,'update_task_status',{taskId,version:d.version,status:'IN_PROGRESS',requestKey:made.requestKey});
 const conflict=await call(receiver,'update_task_status',{taskId,version:d.version,status:'IN_PROGRESS',requestKey:made.requestKey},'TASK_VERSION_CONFLICT');
 assert.equal(conflict.error.taskId,taskId);assert.equal(conflict.error.requestKey,made.requestKey);assert.equal(conflict.error.outcome,'rejected');
 const resultText='  '+label+' 응답\n컨텍스트 원문 확인 완료\n검증: 실제 SDK·수신함 ACK·권한·버전 충돌 확인\n미실행: 실제 LLM의 코드 작성  ';
 const relativePath='reply-'+label.replaceAll('→','-')+'.txt';const diffText='+ '+label+' 응답\n';
 await writeFile(join(receiver.folder,relativePath),diffText);
 assert.equal(await readFile(join(receiver.folder,relativePath),'utf8'),diffText);
 const files=[{relativePath,changeType:'ADDED',diffText}];
 const done=await call(receiver,'complete_task',{taskId,version:accepted.data.version,resultText,taskFiles:files,requestKey:made.requestKey});
 assert.equal(done.data.status,'COMPLETED');
 for(const p of [sender,receiver]){
  d=await detail(p,taskId);assert.equal(d.resultText,resultText);assert.equal(d.version,done.data.version);
  assert.deepEqual(d.taskFiles.map(({fileId,...file})=>file),files);
 }
 const sent=(await call(sender,'get_tasks',{direction:'sent'})).data;
 const received=(await call(receiver,'get_tasks',{direction:'received'})).data;
 assert(sent.items.some(i=>i.taskId===taskId));assert(received.items.some(i=>i.taskId===taskId));
 checks.push({direction:label,taskId,contextPreserved:true,targetInboxOnly:true,centralAck:true,ownershipEnforced:true,versionConflictPreserved:true,completionAndFilesPreserved:true});
}
try {
 const a=await user('A','codex'),b=await user('B','claude_code');
 const team=await run(a,['team','create','--name','Central97 '+randomUUID()]);
 await raw(a,['team','select','--id',String(team.teamId)]);
 await raw(b,['team','join','--code-stdin'],team.inviteCode+'\n');
 await raw(b,['team','select','--id',String(team.teamId)]);
 await agent(a);await agent(b);await start(a);await start(b);
 const listed=(await call(a,'list_agents',{})).data;
 const list=listed.agents??listed.items;assert(Array.isArray(list)&&list.some(i=>i.agentId===String(a.agent.agentId))&&list.some(i=>i.agentId===String(b.agent.agentId)));
 await exchange(a,b,'A→B');await exchange(b,a,'B→A');
 const failed=await call(a,'create_task',{recipientAgentId:String(b.agent.agentId),title:'Failure semantics',description:'A synthetic failure report'});
 let d=await detail(b,failed.data.taskId);
 const accepted=await call(b,'update_task_status',{taskId:d.taskId,version:d.version,status:'IN_PROGRESS'});
 const reason='  실패 사유\n부분 수정 없음  ';
 await call(b,'update_task_status',{taskId:d.taskId,version:accepted.data.version,status:'FAILED',failureReason:reason,partiallyModified:false});
 d=await detail(a,d.taskId);assert.equal(d.failureReason,reason);assert.equal(d.partiallyModified,false);
 const rejection=await call(a,'create_task',{recipientAgentId:String(b.agent.agentId),title:'Rejection read',description:'Synthetic central-only rejection to test REJECTED filter'});
 d=await detail(b,rejection.data.taskId);
 assert(typeof d.version==='string'&&/^(0|[1-9][0-9]*)$/.test(d.version),'SDK versions must remain canonical strings');
 const rejectionVersion=BigInt(d.version);
 assert(rejectionVersion<=BigInt(Number.MAX_SAFE_INTEGER),'Synthetic central rejection requires a safe numeric wire version');
 await central(b,'/api/v1/tasks/'+d.taskId+'/status',{status:'REJECTED',version:Number(rejectionVersion),rejectionReason:'Synthetic central rejection'},'PATCH');
 const rejected=(await call(a,'get_tasks',{direction:'sent',status:'REJECTED'})).data;
 assert(rejected.items.some(item=>item.taskId===d.taskId&&item.status==='REJECTED'));
 checks.push({failedReasonPreserved:true,partialModificationFalsePreserved:true,rejectedFilter:true,rejectionMutationViaCentralApiOnly:true});
} catch(error) { failure=error; } finally {
 for(const client of clients)try{await client.close();}catch{cleanupFailed=true;}
 for(const p of profiles)if(p.started)try{await raw(p,['stop']);p.started=false;}catch{cleanupFailed=true;}
 try {
  for(const p of profiles){safe(p.mcp?.stderr??'');assert(!p.mcp?.stderrOverflow,'MCP diagnostics exceeded the capture limit');}
 } catch(error) { failure??=error; }
 try {
  assert(!cleanupFailed,'Owned process cleanup failed; test profiles retained');
  assert.equal(dirname(resolve(root)),resolve(tmpdir()));assert(basename(root).startsWith('devlink-central97-'));
  await rm(root,{recursive:true,force:true});
 } catch(error) { failure??=error; }
}
if(failure)throw failure;
const report={story:'S15P31D108-97',serverCommit,passed:true,actualSdkStdio:true,actualRustBroker:true,actualCentralSpring:true,actualCentralDatabase:true,users:2,agents:2,agentCreationAndReuse:true,mcpRegistration:true,webSocketConnected:true,checks,ownedProcessesStopped:true,actualLlmHostsExecuted:false};
safe(JSON.stringify(report));console.log(JSON.stringify(report,null,2));
