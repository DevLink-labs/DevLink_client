// S15P31D108-96: opt-in SDK -> MCP stdio -> Rust -> real loopback Spring/PostgreSQL.
import assert from 'node:assert/strict';
import {randomUUID} from 'node:crypto';
import {spawn} from 'node:child_process';
import {mkdtemp,mkdir,readFile,writeFile,rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join,resolve,dirname,basename,isAbsolute} from 'node:path';
import {fileURLToPath} from 'node:url';
import {setTimeout as delay} from 'node:timers/promises';
import {Client} from '@modelcontextprotocol/client';
import {getDefaultEnvironment,StdioClientTransport} from '@modelcontextprotocol/client/stdio';

const base=new URL(process.env.DEVLINK_TEST_CENTRAL_URL??'');
assert(base.protocol==='http:'&&['localhost','127.0.0.1','[::1]'].includes(base.hostname)&&base.pathname==='/'&&!base.search&&!base.hash&&!base.username&&!base.password,'Only an isolated loopback server is allowed');
const cli=process.env.DEVLINK_TEST_CLI;
assert(cli&&isAbsolute(cli),'DEVLINK_TEST_CLI must name the built CLI');
const serverCommit=process.env.DEVLINK_TEST_CENTRAL_COMMIT;
assert(/^[a-f0-9]{40}$/.test(serverCommit??''),'Record the exact server source commit');
const env=Object.fromEntries(Object.entries(process.env).filter(([key])=>!/^DEVLINK_/i.test(key)));
const root=await mkdtemp(join(tmpdir(),'devlink-central96-'));
const profiles=[], clients=[], secrets=[], checks=[];
let cleanupFailed=false, failure;
function safe(value){assert(secrets.every(secret=>!secret||!String(value).includes(secret)),'Secret appeared in shareable output');}
async function raw(p,args,input,expectedSuccess=true) {
  return new Promise((res,rej)=>{
    const child=spawn(cli,['--config',p.config,...args],{env,windowsHide:true,stdio:['pipe','pipe','pipe']});
    let out='',err='',overflow=false;const timer=setTimeout(()=>child.kill(),65000);
    child.stdout.on('data',chunk=>{out+=chunk;if(out.length>2_000_000){overflow=true;child.kill();}});
    child.stderr.on('data',chunk=>{err+=chunk;if(err.length>2_000_000){overflow=true;child.kill();}});
    child.on('error',e=>{clearTimeout(timer);rej(e);});
    child.on('close',code=>{clearTimeout(timer);try{safe(out+err);const codes=[...new Set((out+err).match(/\b[A-Z][A-Z_]{3,}\b/g)??[])].join(',');assert(expectedSuccess?code===0:code!==0,'Unexpected CLI result: '+p.label+' '+args.slice(0,2).join(' ')+' '+codes);assert(!overflow);res(out);}catch(e){rej(e);}});
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
 const folder=join(root,label);await mkdir(folder);const p={folder,config:join(folder,'config.json'),label,tool,name:'Roundtrip '+label};profiles.push(p);
 const email='central96-'+randomUUID()+'@example.test',password='Test-'+randomUUID();secrets.push(password);
 const response=await fetch(new URL('/api/v1/auth/csrf',base));assert(response.ok);
 const csrf=(await response.json()).data;
 const cookie=response.headers.getSetCookie().find(value=>value.startsWith('JSESSIONID=')).split(';')[0];secrets.push(cookie);
 const signup=await fetch(new URL('/api/v1/auth/signup',base),{method:'POST',headers:{Cookie:cookie,'Content-Type':'application/json',[csrf.headerName]:csrf.token},body:JSON.stringify({email,password,name:'Synthetic central96 '+label})});
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
 const client=new Client({name:'central96-'+p.label,version:'1'},{versionNegotiation:{mode:'legacy'}});
 clients.push(client);
 const transport=new StdioClientTransport({command:process.execPath,args:[fileURLToPath(new URL('../dist/index.js',import.meta.url)),'--transport','stdio'],env:{...getDefaultEnvironment(),...env,DEVLINK_BROKER_FILE:join(p.folder,'.devlink-runtime','broker.json')},stderr:'pipe'});
 p.stderr='';p.stderrOverflow=false;
 transport.stderr.on('data',chunk=>{if(p.stderr.length+chunk.length<=2_000_000)p.stderr+=chunk.toString();else p.stderrOverflow=true;});
 await client.connect(transport);p.client=client;
 const names=(await client.listTools()).tools.map(t=>t.name);assert.equal(names.length,9);p.toolNames=names;
 const registered=await call(p,'register_agent',{name:p.name,toolType:p.tool});
 assert.equal(registered.data.agentId,String(p.agent.agentId));
 await until(()=>central(p,'/api/v1/agents/me'),a=>a.status==='ONLINE','ONLINE presence '+p.label);
}
async function call(p,name,args,expectedCode) {
 const r=await p.client.callTool({name,arguments:args});const v=r.structuredContent;
 assert.deepEqual(JSON.parse(r.content[0].text),v,'text and structured output differ');
 if(expectedCode){assert.equal(r.isError,true);assert.equal(v.error.code,expectedCode);return v;}
 assert.notEqual(r.isError,true,'MCP operation failed: '+name+' '+v?.error?.code);return v;
}
async function detail(p,taskId){return (await call(p,'get_task_result',{taskId})).data;}

function knownGuidance(p,text){const names=[...text.matchAll(/\b([a-z_]+)\(/g)].map(m=>m[1]);assert(names.length>=2);for(const name of names)assert(p.toolNames.includes(name),'Unknown tool in fallback guidance: '+name);}
async function notifications(p){const output=await run(p,['notifications','--limit','1024']);knownGuidance(p,output.nextAction);for(const n of output.notifications)knownGuidance(p,n.nextAction);return output.notifications;}
async function notice(p,taskId,status){return until(()=>notifications(p),items=>items.some(n=>n.taskId===taskId&&n.status===status),'notification '+taskId+' '+status);}
async function create(sender,receiver,title){return (await call(sender,'create_task',{recipientAgentId:String(receiver.agent.agentId),title,description:'Synthetic notification contract verification'})).data.taskId;}
async function update(receiver,taskId,status,extra={}){
 const before=await detail(receiver,taskId);const version=Number(before.version);assert(Number.isSafeInteger(version));
 return central(receiver,'/api/v1/tasks/'+taskId+'/status',{status,version,...extra},'PATCH');
}
async function exchange(sender,receiver,label){
 const taskId=await create(sender,receiver,label);await update(receiver,taskId,'IN_PROGRESS');await notice(sender,taskId,'IN_PROGRESS');
 let d=await detail(receiver,taskId);const resultText='  '+label+' 완료\n검증: 실제 중앙 알림과 권한 조회 대조  ';
 await central(receiver,'/api/v1/tasks/'+taskId+'/complete',{version:Number(d.version),resultText,taskFiles:[]});
 const list=await notice(sender,taskId,'COMPLETED'),n=list.find(n=>n.taskId===taskId);d=await detail(sender,taskId);
 assert.equal(n.version,d.version);assert.equal(n.status,d.status);assert.equal(n.summary,d.resultText);assert.equal(n.detailRequired,true);assert.equal(n.summary,resultText);
 assert(!(await notifications(receiver)).some(n=>n.taskId===taskId),'notification went to receiver rather than requester');
 const sent=(await call(sender,'get_tasks',{direction:'sent'})).data;assert(sent.items.some(t=>t.taskId===taskId));
 checks.push({direction:label,taskId,requesterOnly:true,statusAndResultNotification:true,authorizedLocalResultMatches:true});
}
try {
 const a=await user('A','codex'),b=await user('B','claude_code');
 const team=await run(a,['team','create','--name','Central96 '+randomUUID()]);
 await raw(a,['team','select','--id',String(team.teamId)]);await raw(b,['team','join','--code-stdin'],team.inviteCode+'\n');await raw(b,['team','select','--id',String(team.teamId)]);
 await agent(a);await agent(b);await start(a);await start(b);
 await exchange(a,b,'A→B');await exchange(b,a,'B→A');
 const failed=await create(a,b,'Failure notification');await update(b,failed,'IN_PROGRESS');
 const reason='  실제 실패 이유\n부분 수정 없음  ';await update(b,failed,'FAILED',{failureReason:reason,partiallyModified:false});
 await notice(a,failed,'FAILED');let d=await detail(a,failed);assert.equal(d.failureReason,reason);assert.equal(d.partiallyModified,false);
 const rejected=await create(a,b,'Rejection notification');await update(b,rejected,'REJECTED',{rejectionReason:'Explicit synthetic rejection'});await notice(a,rejected,'REJECTED');
 d=await detail(a,rejected);assert.equal(d.status,'REJECTED');assert.equal(d.rejectionReason,'Explicit synthetic rejection');
 const retained=await notifications(a);await a.client.close();clients.splice(clients.indexOf(a.client),1);await raw(a,['stop']);a.started=false;
 assert.deepEqual(await notifications(a),retained,'retention lost across broker stop');
 await start(a);assert.deepEqual(await notifications(a),retained,'retention lost on restart');
 checks.push({failureReasonAndPartialFlagViaAuthorizedDetail:true,rejectionReasonViaAuthorizedDetail:true,retainedAfterRestart:true});
 await raw(a,['notifications','--clear'],undefined,false);
} catch(error) { failure=error; } finally {
 for(const client of clients)try{await client.close();}catch{cleanupFailed=true;}
 for(const p of profiles)if(p.started)try{await raw(p,['stop']);p.started=false;}catch{cleanupFailed=true;}
 try {
  for(const p of profiles){safe(p.stderr??'');assert(!p.stderrOverflow,'MCP diagnostics exceeded the capture limit');}
 } catch(error) { failure??=error; }
 try {
  assert(!cleanupFailed,'Owned process cleanup failed; test profiles retained');
  assert.equal(dirname(resolve(root)),resolve(tmpdir()));assert(basename(root).startsWith('devlink-central96-'));
  await rm(root,{recursive:true,force:true});
 } catch(error) { failure??=error; }
}
if(failure)throw failure;
const report={story:'S15P31D108-96',serverCommit,passed:true,actualSdkStdio:true,actualRustBroker:true,actualCentralSpring:true,actualCentralDatabase:true,checks,ownedProcessesStopped:true,actualLlmHostsExecuted:false};safe(JSON.stringify(report));console.log(JSON.stringify(report,null,2));
