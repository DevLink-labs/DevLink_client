// Actual loopback Spring/PostgreSQL -> Rust inbox -> authoritative ACK history. Synthetic tasks only.
import assert from 'node:assert/strict';
import {randomUUID} from 'node:crypto';
import {spawnSync} from 'node:child_process';
import {mkdtemp,mkdir,readFile,rename,rmdir,rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join,resolve,dirname,basename,isAbsolute} from 'node:path';
import {setTimeout as delay} from 'node:timers/promises';
const base=new URL(process.env.DEVLINK_TEST_CENTRAL_URL??'');
assert(base.protocol==='http:'&&['localhost','127.0.0.1','[::1]'].includes(base.hostname)&&base.pathname==='/'&&!base.search&&!base.hash&&!base.username&&!base.password);
const cli=process.env.DEVLINK_TEST_CLI;assert(cli&&isAbsolute(cli));
const env=Object.fromEntries(Object.entries(process.env).filter(([key])=>!/^DEVLINK_/i.test(key)));
const root=await mkdtemp(join(tmpdir(),'devlink-inbox95-'));const profiles=[];const secrets=[];let restore;let timer;
function raw(p,args,input,ok=true){
 const r=spawnSync(cli,['--config',p.config,...args],{env,input,encoding:'utf8',windowsHide:true,timeout:65_000});
 const text=`${r.stdout??''}${r.stderr??''}`;assert(secrets.every(s=>!text.includes(s)),'Secret appeared in CLI output');
 assert(!r.error&&(ok?r.status===0:r.status!==0),`Unexpected CLI result for ${args.slice(0,2).join(' ')}: ${text}`);return text;
}
function run(p,args,input,ok=true){const text=raw(p,args,input,ok);return ok?JSON.parse(text):text;}
async function saved(p,name){return JSON.parse(await readFile(join(p.folder,'.devlink-runtime',name),'utf8'));}
async function user(label){
 const folder=join(root,label);await mkdir(folder);const p={folder,config:join(folder,'config.json')};profiles.push(p);
 const email=`inbox95-${randomUUID()}@example.test`,password=`Test-${randomUUID()}`;secrets.push(password);
 const response=await fetch(new URL('/api/v1/auth/csrf',base));const csrf=(await response.json()).data;
 const cookie=response.headers.getSetCookie().find(x=>x.startsWith('JSESSIONID=')).split(';')[0];
 const signup=await fetch(new URL('/api/v1/auth/signup',base),{method:'POST',headers:{Cookie:cookie,'Content-Type':'application/json',[csrf.headerName]:csrf.token},body:JSON.stringify({email,password,name:'Synthetic inbox verification'})});assert.equal(signup.status,201);
 raw(p,['init']);raw(p,['config','set','serverUrl',base.toString().replace(/^http:/,'ws:')]);
 p.user=run(p,['auth','login','--email',email,'--password-stdin'],`${password}\n`).user;
 secrets.push((await saved(p,'user-session.json')).state.cookie);return p;
}
async function agent(p,tool){
 p.agent=run(p,['agent','create','--name','Inbox verification','--tool',tool,'--project-name','Isolated inbox','--project',p.folder]).agent;
 p.token=(await saved(p,'agent-profile.json')).state.token;secrets.push(p.token);
}
async function api(p,path,body){
 const response=await fetch(new URL(path,base),{method:body?'POST':'GET',headers:{Authorization:`Bearer ${p.token}`,...(body?{'Content-Type':'application/json'}:{})},...(body?{body:JSON.stringify(body)}:{})});
 assert(response.ok,`Actual central API ${path} rejected with ${response.status}`);const value=await response.json();assert(value.success);return value.data;
}
async function until(operation,predicate,label){
 const deadline=Date.now()+15_000;
 while(Date.now()<deadline){const value=await operation();if(predicate(value))return value;await delay(100);}
 throw new Error(`Timed out: ${label}`);
}
async function start(p){
 raw(p,['start']);p.runtime=await saved(p,'runtime.json');secrets.push(p.runtime.controlToken);p.mcp=await saved(p,'broker.json');secrets.push(p.mcp.localToken);
 await until(async()=> (await fetch(`${p.mcp.brokerUrl}/health`,{headers:{Authorization:`Bearer ${p.mcp.localToken}`}})).json(),h=>h.centralServer==='Connected','central WS');
 const instanceId=`receipt-host-${randomUUID()}`;
 const claim=await fetch(`${p.mcp.brokerUrl}/v1/sessions`,{method:'POST',headers:{Authorization:`Bearer ${p.mcp.localToken}`,'Content-Type':'application/json'},body:JSON.stringify({requestId:randomUUID(),instanceId,mode:'fresh',agentId:String(p.agent.agentId)})});
 assert.equal(claim.status,200);const session=(await claim.json()).data;
 assert.deepEqual(session.principal,{userId:String(p.user.userId),teamId:String(p.agent.teamId),agentId:String(p.agent.agentId)});
 timer=setInterval(()=>{fetch(`${p.mcp.brokerUrl}/v1/sessions/${session.sessionId}/heartbeat`,{method:'POST',headers:{Authorization:`Bearer ${p.mcp.localToken}`,'Content-Type':'application/json'},body:JSON.stringify({requestId:randomUUID(),instanceId})}).catch(()=>{});},5000);
 await until(()=>api(p,'/api/v1/agents/me'),value=>value.status==='ONLINE','verified presence');
}
function stop(p){clearInterval(timer);timer=undefined;raw(p,['stop']);}
async function details(p,id){return api(p,`/api/v1/tasks/${id}`);}
function has(value,type){return value.events.some(event=>event.eventType===type);}
try{
 const a=await user('sender'),b=await user('receiver');
 const team=run(a,['team','create','--name',`Inbox ${randomUUID()}`]);
 run(a,['team','select','--id',String(team.teamId)]);run(b,['team','join','--code-stdin'],`${team.inviteCode}\n`);run(b,['team','select','--id',String(team.teamId)]);
 await agent(a,'codex');await agent(b,'claude-code');await start(b);
 const creationPath=`/api/v1/tasks/${a.agent.teamId}`;
 const firstRuntime=b.runtime.instanceId;
 const bodies=Array.from({length:16},(_,i)=>({requestKey:randomUUID(),receiverAgentId:b.agent.agentId,prompt:`Synthetic inbox ${i} ${randomUUID()}`}));
 const created=await Promise.all(bodies.map(body=>api(a,creationPath,body)));
 assert(created.every(task=>task.status==='PENDING'&&!task.replayed));
 await until(()=>Promise.all(created.map(task=>details(a,task.taskId))),values=>values.every(value=>has(value,'ACK_RECEIVED')&&!has(value,'DELIVERY_UNKNOWN')&&value.status==='PENDING'),'16 durable central receipts');
 const received=run(b,['inbox','list','--limit','1024']);assert.equal(received.length,16);
 assert(received.every(item=>item.message.payload.requester_agent_id===a.agent.agentId&&item.message.payload.receiver_agent_id===b.agent.agentId));
 const file=join(b.folder,'.devlink-runtime','inbox.json');const before=await readFile(file,'utf8');
 assert(run(b,['inbox','clear'],undefined,false).includes('Stop the broker'));assert((await readFile(file,'utf8'))===before);
 stop(b);await start(b);assert(b.runtime.instanceId!==firstRuntime,'Broker process instance must change');assert((await readFile(file,'utf8'))===before);
 // The real server does not resend a created task on request-key replay. Wire-redelivery dedup is tested by the socket fixture.
 const replay=await api(a,creationPath,bodies[0]);assert(replay.replayed&&replay.taskId===created[0].taskId);assert((await readFile(file,'utf8'))===before);
 const next=await api(a,creationPath,{requestKey:randomUUID(),receiverAgentId:b.agent.agentId,prompt:'Synthetic receipt after broker restart'});
 await until(()=>details(a,next.taskId),value=>has(value,'ACK_RECEIVED')&&value.status==='PENDING','receipt after restart');
 assert.equal(run(b,['inbox','list','--limit','1024']).length,17);
 // Inject an actual local write failure in an owned disposable profile, preserve and restore its original file.
 assert.equal(dirname(resolve(file)),resolve(join(b.folder,'.devlink-runtime')));assert(resolve(file).startsWith(`${resolve(root)}\\`)||resolve(file).startsWith(`${resolve(root)}/`));
 const backup=join(b.folder,'.devlink-runtime','inbox95-backup.json');await rename(file,backup);await mkdir(file);
 restore=async()=>{await rmdir(file);await rename(backup,file);};
 const failed=await api(a,creationPath,{requestKey:randomUUID(),receiverAgentId:b.agent.agentId,prompt:'Synthetic failed local persistence must not ACK'});
 const health=await until(async()=>(await fetch(`${b.mcp.brokerUrl}/health`,{headers:{Authorization:`Bearer ${b.mcp.localToken}`}})).json(),h=>h.centralServer==='InboxFailed','local failure status');assert(health.inbox.rejectedEvents>0);
 const failure=await until(()=>details(a,failed.taskId),value=>has(value,'DELIVERY_UNKNOWN'),'central failed-receipt history');assert(!has(failure,'ACK_RECEIVED'));assert.equal(failure.status,'PENDING');
 stop(b);await restore();restore=undefined;assert.equal(run(b,['inbox','list','--limit','1024']).length,17);
 raw(b,['inbox','clear']);assert.equal(run(b,['inbox','list']).length,0);
 console.log(JSON.stringify({story:'S15P31D108-95',actualCentral:true,actualPostgreSQLHistory:true,actualRustBroker:true,users:2,burstAckReceived:16,afterRestartAckReceived:1,writeFailureAckReceived:0,writeFailureRecorded:'DELIVERY_UNKNOWN',automaticAcceptance:false,taskStatuses:'PENDING',sameRequestKeyReplayDoesNotResend:true,wireRedeliveryProof:'separate socket fixture',secretOutput:false}));
}finally{
 clearInterval(timer);let cleanupFailed=false;
 for(const p of profiles)try{raw(p,['stop']);}catch{cleanupFailed=true;}
 if(restore&&!cleanupFailed)try{await restore();}catch{cleanupFailed=true;}
 assert(!cleanupFailed,'Owned test cleanup failed; private temporary profiles preserved for recovery');
 assert.equal(dirname(resolve(root)),resolve(tmpdir()));assert(basename(root).startsWith('devlink-inbox95-'));
 await rm(root,{recursive:true,force:true});
}
