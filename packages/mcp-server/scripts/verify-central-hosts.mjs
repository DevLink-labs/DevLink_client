// S15P31D108-66: opt-in real Codex/Claude -> MCP -> Rust -> loopback Spring/PostgreSQL.
import assert from 'node:assert/strict';
import {randomUUID} from 'node:crypto';
import {spawn} from 'node:child_process';
import {mkdtemp,mkdir,readFile,writeFile,rm,readdir} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join,resolve,dirname,basename,isAbsolute} from 'node:path';
import {setTimeout as delay} from 'node:timers/promises';

const base=new URL(process.env.DEVLINK_TEST_CENTRAL_URL??'');
assert(base.protocol==='http:'&&['localhost','127.0.0.1','[::1]'].includes(base.hostname)&&base.pathname==='/'&&!base.search&&!base.hash&&!base.username&&!base.password,'Only an isolated loopback server is allowed');
const cli=process.env.DEVLINK_TEST_CLI;
assert(cli&&isAbsolute(cli),'DEVLINK_TEST_CLI must name the built CLI');
const serverCommit=process.env.DEVLINK_TEST_CENTRAL_COMMIT;
assert(/^[a-f0-9]{40}$/.test(serverCommit??''),'Record the exact server source commit');
const entryPath=process.env.DEVLINK_TEST_MCP_ENTRY;
assert(entryPath&&isAbsolute(entryPath),'Explicit absolute DEVLINK_TEST_MCP_ENTRY is required');
const clientContractCommit=process.env.DEVLINK_TEST_CONTRACT_COMMIT;
const clientNotificationsCommit=process.env.DEVLINK_TEST_NOTIFICATIONS_COMMIT;
for(const commit of [clientContractCommit,clientNotificationsCommit])assert(/^[a-f0-9]{40}$/.test(commit??''),'Record both client source commits');
const artifactRoot=process.env.DEVLINK_TEST_HOST_ARTIFACTS;
assert(artifactRoot&&isAbsolute(artifactRoot),'Absolute DEVLINK_TEST_HOST_ARTIFACTS is required');
const artifactDir=join(artifactRoot,'run-'+randomUUID());
const mcpFiles=await buildFiles(dirname(entryPath));
const buildProvenance={serverCommit,clientContractCommit,clientNotificationsCommit,cli,cliSha256:hash(await readFile(cli)),mcpEntry:entryPath,mcpEntrySha256:hash(await readFile(entryPath)),mcpDistSha256:hash(JSON.stringify(mcpFiles))};
const env=Object.fromEntries(Object.entries(process.env).filter(([key])=>!/^DEVLINK_/i.test(key)));
const root=await mkdtemp(join(tmpdir(),'devlink-central66-'));
const profiles=[], secrets=[], checks=[];
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
async function until(operation,predicate,label,timeoutMs=15000) {
 const end=Date.now()+timeoutMs;
 while(Date.now()<end){const value=await operation();if(predicate(value))return value;await delay(100);}
 throw new Error('Timed out: '+label);
}
async function user(label,tool) {
 const folder=join(root,label);await mkdir(folder);const p={folder,config:join(folder,'config.json'),label,tool,name:'Roundtrip '+label};profiles.push(p);
 const email='central66-'+randomUUID()+'@example.test',password='Test-'+randomUUID();secrets.push(password);
 const response=await fetch(new URL('/api/v1/auth/csrf',base),{signal:AbortSignal.timeout(5000)});assert(response.ok);
 const csrf=(await response.json()).data;
 const cookie=response.headers.getSetCookie().find(value=>value.startsWith('JSESSIONID=')).split(';')[0];secrets.push(cookie);
 const signup=await fetch(new URL('/api/v1/auth/signup',base),{method:'POST',headers:{Cookie:cookie,'Content-Type':'application/json',[csrf.headerName]:csrf.token},body:JSON.stringify({email,password,name:'Synthetic central66 '+label}),signal:AbortSignal.timeout(5000)});
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
 const r=await fetch(new URL(path,base),{method:method??(body?'POST':'GET'),headers:{Authorization:'Bearer '+p.token,...(body?{'Content-Type':'application/json'}:{})},...(body?{body:JSON.stringify(body)}:{}),signal:AbortSignal.timeout(5000)});
 assert(r.ok,'Central request rejected: '+r.status);const value=await r.json();assert(value.success);return value.data;
}
// These Host processes own their MCP sessions; the runner does not bind a competing SDK session.
import {execFile} from 'node:child_process';
import {promisify} from 'node:util';
import {createHash} from 'node:crypto';
import {captureHost} from '../test/helpers/host-process.mjs';
import {hostTools} from '../test/helpers/host-transcript.mjs';
import {centralHostTranscript,hostRanPassingTests,verifyIncomingFlow,verifyOutgoingFlow,normalizeDiff,hostEditedSource,rejectCounterpartAccess,unexpectedProjectFiles,verifyTwoHostExchange} from '../test/helpers/central-host-proof.mjs';
const execute=promisify(execFile);
const hostResults=[],hostPromises=[],abort=new AbortController();
const exchangeProofs=[];let exchanges=[];
function hash(text){return createHash('sha256').update(text).digest('hex');}
async function buildFiles(directory,prefix=''){
 const files=[];
 for(const item of await readdir(directory,{withFileTypes:true})){
  const path=join(directory,item.name),name=prefix+item.name;
  assert(!item.isSymbolicLink(),'MCP build cannot contain symlinks');
  if(item.isDirectory())files.push(...await buildFiles(path,name+'/'));
  else if(item.isFile())files.push({path:name,sha256:hash(await readFile(path))});
 }
 return files.sort((a,b)=>a.path<b.path?-1:a.path>b.path?1:0);
}
async function command(p,args,expected=0){
 try{const r=await execute(args[0],args.slice(1),{cwd:p.folder,windowsHide:true,timeout:20000,maxBuffer:2_000_000,env});assert.equal(expected,0);safe(r.stdout+r.stderr);return r.stdout;}
 catch(e){if(expected!==0&&e.code===expected){safe(e.stdout+e.stderr);return e.stdout;}throw e;}
}
async function seed(p){
 await mkdir(join(p.folder,'src'));await mkdir(join(p.folder,'test'));
 p.file=p.label==='A'?'src/slug.mjs':'src/invoice.mjs';
 p.initial=p.label==='A'?"export function slug(text) { return text.trim().toLowerCase().replaceAll(' ', '-'); }\n":"export function totalCents(lines) { return lines.reduce((sum, item) => sum + item.unitCents, 0); }\n";
 p.tests=p.label==='A'?"import {test} from 'node:test';import assert from 'node:assert/strict';import {slug} from '../src/slug.mjs';test('collapse whitespace runs',()=>assert.equal(slug('  Hello\\t  WORLD  '),'hello-world'));test('preserve unicode',()=>assert.equal(slug('가나   다라'),'가나-다라'));\n":"import {test} from 'node:test';import assert from 'node:assert/strict';import {totalCents} from '../src/invoice.mjs';test('multiply each unit price by quantity',()=>assert.equal(totalCents([{unitCents:125,quantity:3},{unitCents:400,quantity:2}]),1175));test('zero quantity contributes zero',()=>assert.equal(totalCents([{unitCents:800,quantity:0}]),0));\n";
 await writeFile(join(p.folder,p.file),p.initial);await writeFile(join(p.folder,'test/task.test.mjs'),p.tests);
 await writeFile(join(p.folder,'package.json'),JSON.stringify({private:true,type:'module',scripts:{test:'node --test'}}));
 await command(p,['git','init','--quiet']);
 await command(p,['git','config','--local','core.autocrlf','false']);await command(p,['git','config','--local','core.safecrlf','false']);
 await command(p,['git','add','--','src','test','package.json']);
 p.index=await command(p,['git','ls-files','--stage']);
 await command(p,[process.execPath,'--test'],1);
}
async function startBroker(p){
 p.started=true;await raw(p,['start']);
 p.descriptor=await saved(p,'broker.json');const runtime=await saved(p,'runtime.json');secrets.push(p.descriptor.localToken,runtime.controlToken);
 await until(async()=> (await fetch(p.descriptor.brokerUrl+'/health',{headers:{Authorization:'Bearer '+p.descriptor.localToken},signal:AbortSignal.timeout(5000)})).json(),h=>h.centralServer==='Connected','broker '+p.label);
}
async function ownedNodeProcesses(marker,kill=false){
 assert(/^devlink-host66-[0-9a-f-]{36}$/.test(marker));
 const script="$items=@(Get-CimInstance Win32_Process -Filter \"Name = 'node.exe'\" | Where-Object { $_.CommandLine -and $_.CommandLine.Contains($env:DEVLINK_HOST_MARKER) }); if($env:DEVLINK_HOST_KILL -eq '1'){foreach($item in $items){Stop-Process -Id $item.ProcessId -Force -ErrorAction SilentlyContinue}}; ConvertTo-Json -Compress -InputObject @($items | ForEach-Object {$_.ProcessId})";
 const r=await execute('powershell.exe',['-NoProfile','-NonInteractive','-Command',script],{windowsHide:true,timeout:10000,maxBuffer:4096,env:{...env,DEVLINK_HOST_MARKER:marker,DEVLINK_HOST_KILL:kill?'1':'0'}});
 return JSON.parse(r.stdout||'[]');
}
async function hostRun(host,p,prompt){
 const marker='devlink-host66-'+randomUUID();
 const argsMcp=[`--title=${marker}`,entryPath,'--transport','stdio','--request-timeout-ms','10000','--call-timeout-ms','30000'];
 const credentialPath=join(p.folder,'.devlink-runtime','broker.json');
 let args;
 if(host==='claude'){
  const config=join(p.folder,'mcp.json');await writeFile(config,JSON.stringify({mcpServers:{devlink:{command:process.execPath,args:argsMcp,env:{DEVLINK_BROKER_FILE:credentialPath}}}}));
  args=['-p','--model','claude-opus-5-5','--effort','high','--restricted','--strict-mcp-config','--mcp-config',config,'--tools','Read,Edit,Write,Bash','--allowedTools',['Read','Edit','Write','Bash',...hostTools.map(t=>'mcp__devlink__'+t)].join(','),'--permission-mode','dontAsk','--permission-prompts','none','--disable-slash-commands','--no-session-persistence','--no-chrome','--output-format','stream-json','--verbose',prompt];
 }else{
  const mcp=`mcp_servers={devlink={command=${JSON.stringify(process.execPath)},args=${JSON.stringify(argsMcp)},env={DEVLINK_BROKER_FILE=${JSON.stringify(credentialPath)}},enabled_tools=${JSON.stringify(hostTools)},required=true,default_tools_approval_mode="approve",startup_timeout_sec=30,tool_timeout_sec=40}}`;
  args=['--no-daemon','-a','on-request','exec','--ephemeral','--skip-git-repo-check','-c','default_permissions=":workspace"','--json','--cd',p.folder,'-c',mcp,'-c','mcp_optional_startup_grace_ms=0',prompt];
 }
 await mkdir(artifactDir,{recursive:true});await writeFile(join(artifactDir,host+'.prompt.txt'),prompt);
 let result;
 try {
  result=await captureHost(host==='claude'?(process.env.DEVLINK_TEST_CLAUDE??'claude.exe'):(process.env.DEVLINK_TEST_CODEX??'codex.exe'),args,{cwd:p.folder,timeoutMs:420000,signal:abort.signal});
 } finally {
  try {
   const deadline=Date.now()+7000;let remaining=await ownedNodeProcesses(marker);while(remaining.length&&Date.now()<deadline){await delay(200);remaining=await ownedNodeProcesses(marker);}
   if(remaining.length){await ownedNodeProcesses(marker,true);cleanupFailed=true;throw Error('Host left owned MCP process running');}
  } catch(error) { cleanupFailed=true;await ownedNodeProcesses(marker,true).catch(()=>{});throw error; }
 }
 safe(result.stdout+result.stderr);
 await mkdir(artifactDir,{recursive:true});await writeFile(join(artifactDir,host+'.jsonl'),result.stdout);await writeFile(join(artifactDir,host+'.stderr.txt'),result.stderr);
 assert.equal(result.code,0,'Host did not exit successfully');assert(!result.timedOut&&!result.overflow&&!result.interrupted&&!result.spawnError,'Host capture failed');
 const proof=centralHostTranscript(host,result.stdout);if(host==='claude')assert.equal(proof.parsed.model,'claude-opus-5-5');
 hostResults.push({host,p,...proof,transcriptSha256:hash(result.stdout)});return proof;
}
function beginHost(host,p,prompt){
 const promise=hostRun(host,p,prompt).then(value=>({value}),error=>{abort.abort();return{error};});hostPromises.push(promise);return promise;
}
function prompt(p,other,nonceAB,nonceBA){
 const localRequest=p.label==='A'?'slug: collapse all whitespace runs into one hyphen, trim outer whitespace, lowercase while preserving non-whitespace unicode':'invoice: totalCents must multiply unitCents by quantity for every line; a zero quantity contributes zero';
 const remoteRequest=p.label==='A'?'invoice totalCents must multiply unitCents by quantity; zero quantity contributes zero':'slug must trim outer whitespace, lowercase and collapse every whitespace run (including tabs) into one hyphen, preserving unicode';
 const ownNonce=p.label==='A'?nonceBA:nonceAB,sendNonce=p.label==='A'?nonceAB:nonceBA;
 p.incomingNonce=ownNonce;p.outgoingNonce=sendNonce;
 return `You are the real ${p.tool} Host in a DevLink first-MVP acceptance test. Work only in current isolated project ${p.folder}. No subagents, other MCP servers, browser, network tools, commits, pushes, package install, reading credential/config/hidden state files, or changing tests. Allowed local shell purposes: read src/test, node --test, git diff -- src, short local polling sleeps. The operator explicitly accepts the incoming synthetic request containing nonce ${ownNonce} for execution now; no other task is approved. Register the existing Agent with register_agent name=${JSON.stringify(p.name)}, toolType=${p.tool}, omit role and all unmentioned metadata; then list_agents and verify the counterpart ID ${other.agent.agentId}. Use all DevLink tool calls serially. Poll at most 120 times for each phase, spaced by real local shell delay of about 1 second if needed. Never claim a step without its tool evidence.
Incoming work: use get_tasks(direction=received) then get_task_result to locate nonce ${ownNonce}; verify PENDING, call update_task_status with its current string version and IN_PROGRESS (this operator message is the explicit acceptance); analyze actual ${p.file} and test/task.test.mjs, fix ${localRequest} using ${p.label==='A'?'apply_patch (recorded file_change)':'the local Edit/Write tools'}, run node --test and ensure exit0. Do not modify tests/package.json, stage changes, add any source files or access a counterpart project. Get current version again, use git diff -- src to capture exact diff, then complete_task with version/resultText describing analysis + node --test results and taskFiles[{relativePath:${JSON.stringify(p.file)},changeType:'MODIFIED',diffText:<actual git diff stdout>}]. The completion must be based on actual changed source and successful test execution.
Outgoing work: call create_task to recipientAgentId=${other.agent.agentId}, title=${JSON.stringify('MVP66 '+sendNonce)}, description=${JSON.stringify(remoteRequest)}, taskContext describing counterpart file ${other.file}, node --test and nonce ${sendNonce}. Wait for get_task_result to become COMPLETED; verify summary, verification and nonempty changed-file diff, do not exit before it arrives.
Order: ${p.label==='A'?'create outgoing A→B, poll its completed result, then process incoming B→A and complete it':'wait for incoming A→B and fix/complete it, then create outgoing B→A and poll completed result'}. Keep this MCP session bound throughout both phases. Do not issue duplicate create or retry mutations on unknown outcomes. End with JSON containing both task IDs and observed results. This is request→explicit acceptance→actual code change→verification→result, not free chat.`;
}
try {
 await mkdir(artifactDir,{recursive:true});await writeFile(join(artifactDir,'build-provenance.json'),JSON.stringify({...buildProvenance,mcpFiles},null,2));
 const a=await user('A','codex'),b=await user('B','claude_code');
 await seed(a);await seed(b);
 const team=await run(a,['team','create','--name','Central66 '+randomUUID()]);await raw(a,['team','select','--id',String(team.teamId)]);await raw(b,['team','join','--code-stdin'],team.inviteCode+'\n');await raw(b,['team','select','--id',String(team.teamId)]);
 await agent(a);await agent(b);await startBroker(a);await startBroker(b);
 const nonceAB=randomUUID(),nonceBA=randomUUID();
 const claude=beginHost('claude',b,prompt(b,a,nonceAB,nonceBA));
 await until(()=>central(b,'/api/v1/agents/me'),x=>x.status==='ONLINE','Claude Host registration',120000);
 const codex=beginHost('codex',a,prompt(a,b,nonceAB,nonceBA));
 const results=await Promise.all([claude,codex]);for(const r of results)if(r.error)throw r.error;
 for(const h of hostResults){
  const check={host:h.host,passed:false};checks.push(check);
  const p=h.p,other=p===a?b:a,calls=h.parsed.calls;
  const options={agentId:p.agent.agentId,counterpartId:other.agent.agentId,sourcePath:p.file,counterpartPath:other.file,incomingNonce:p.incomingNonce,outgoingNonce:p.outgoingNonce};
  let outgoing;
  try { outgoing=verifyOutgoingFlow(calls,options);Object.assign(check,{outgoingId:outgoing.outgoingId,requesterReadCompletedResult:true}); }
  catch(error) { check.outgoingError=error.message;failure??=error; }
  try {
  const {completed:done,incomingId}=verifyIncomingFlow(calls,options);
  rejectCounterpartAccess(h.host,h.events,p.folder,other.folder);
  assert(hostEditedSource(h.host,h.events,p.folder,p.file),'Receiving Host has no successful edit evidence for its own source');
  assert.equal(await readFile(join(p.folder,'test/task.test.mjs'),'utf8'),p.tests,'Host modified tests');
  assert.notEqual(await readFile(join(p.folder,p.file),'utf8'),p.initial,'Host did not edit actual source');
  assert.equal(await command(p,['git','ls-files','--stage']),p.index,'Host changed the fixture index');
  assert.deepEqual((await command(p,['git','diff','--name-only'])).trim().split(/\r?\n/),[p.file]);
  const untracked=(await command(p,['git','ls-files','--others','-z'])).split('\0').filter(Boolean);
  assert.deepEqual(unexpectedProjectFiles(untracked),[],'Host created an unexpected untracked file');
  const diff=await command(p,['git','diff','--',p.file]);assert.equal(normalizeDiff(done.args.taskFiles[0].diffText),normalizeDiff(diff));
  const tested=await command(p,[process.execPath,'--test']);assert(tested.includes('pass 2')&&tested.includes('fail 0'));
  const taskId=done.args.taskId,d=await central(p,'/api/v1/tasks/'+taskId);assert.equal(d.status,'COMPLETED');assert(d.resultText.includes('node --test'));assert.equal(normalizeDiff(d.taskFiles[0].diffText),normalizeDiff(diff));
  assert(hostRanPassingTests(h.host,h.events),'Host did not run a successful verification command');
  Object.assign(check,{taskId,sourcePath:p.file,actualSourceChanged:true,testsUnchanged:true,hostRanVerification:true,parentTestsPassed:true,actualDiffMatchesCentral:true,transcriptSha256:h.transcriptSha256,model:h.parsed.model??'CLI default; model not reported in transcript'});
  assert(outgoing,'Host has no verified outgoing completion');
  const {outgoingId,observed}=outgoing;assert.notEqual(incomingId,outgoingId);Object.assign(check,{passed:true});
  exchangeProofs.push({host:h.host,incomingId,outgoingId,sourcePath:p.file,diff,observed});
  } catch(error) { check.error=error.message;failure??=error; }
 }
 if(!failure)exchanges=verifyTwoHostExchange(exchangeProofs);
} catch(error){failure=error;} finally {
 abort.abort();await Promise.allSettled(hostPromises);
 for(const p of profiles)if(p.started)try{await raw(p,['stop']);p.started=false;}catch{cleanupFailed=true;}
 if(!cleanupFailed){assert.equal(dirname(resolve(root)),resolve(tmpdir()));assert(basename(root).startsWith('devlink-central66-'));await rm(root,{recursive:true,force:true});}
 else failure??=Error('Owned broker cleanup failed; test profiles retained');
}
const report={story:'S15P31D108-66',buildProvenance,passed:!failure,actualLlmHostsExecuted:hostResults.length===2,checks,exchanges,ownedProcessesStopped:!cleanupFailed};safe(JSON.stringify(report));
await mkdir(artifactDir,{recursive:true});await writeFile(join(artifactDir,'report.json'),JSON.stringify(report,null,2));console.log(JSON.stringify(report,null,2));
if(failure)throw failure;
