// Actual loopback Spring + PostgreSQL. No production accounts or global Host settings.
import assert from 'node:assert/strict';
import {randomUUID} from 'node:crypto';
import {spawnSync,execFile} from 'node:child_process';
import {promisify} from 'node:util';
import {mkdtemp,mkdir,readFile,writeFile,rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join,resolve,dirname,basename,isAbsolute} from 'node:path';
import {fileURLToPath} from 'node:url';
import {setTimeout as delay} from 'node:timers/promises';
import {captureHost} from '../../packages/mcp-server/test/helpers/host-process.mjs';
import {hostConfiguration,readHostTranscript,hostTools} from '../../packages/mcp-server/test/helpers/host-transcript.mjs';
const execute=promisify(execFile);
const base=new URL(process.env.DEVLINK_TEST_CENTRAL_URL??'');
assert(base.protocol==='http:'&&['localhost','127.0.0.1','[::1]'].includes(base.hostname)&&base.pathname==='/'&&!base.search&&!base.username&&!base.password);
const cli=process.env.DEVLINK_TEST_CLI;assert(cli&&isAbsolute(cli));
const hosts=process.env.DEVLINK_TEST_HOSTS==='both'?['codex','claude']:[];
assert(!process.env.DEVLINK_TEST_HOSTS||process.env.DEVLINK_TEST_HOSTS==='both','Use both or omit actual Host execution');
delete process.env.DEVLINK_TOKEN;
const env=Object.fromEntries(Object.entries(process.env).filter(([k])=>!/^DEVLINK_/i.test(k)));
const root=await mkdtemp(join(tmpdir(),'devlink-agent-bootstrap-'));
const secrets=[];const profiles=[];const hostEvidence=[];
function safe(text){assert(secrets.every(s=>!text.includes(s)),'Authentication secret appeared in output');}
function raw(config,args,input,ok=true){
 const r=spawnSync(cli,['--config',config,...args],{env,input,encoding:'utf8',timeout:65_000,windowsHide:true});
 const text=`${r.stdout??''}${r.stderr??''}`;safe(text);
 assert(!r.error&&(ok?r.status===0:r.status!==0),`Unexpected CLI result for ${args.slice(0,2).join(' ')}: ${text}`);
 return text;
}
function run(config,args,input,ok=true){const text=raw(config,args,input,ok);return ok?JSON.parse(text):text;}
async function snapshot(p,name){return JSON.parse(await readFile(join(p.folder,'.devlink-runtime',name),'utf8'));}
async function login(p){
 const value=run(p.config,['auth','login','--email',p.email,'--password-stdin'],`${p.password}\n`);
 const saved=await snapshot(p,'user-session.json');secrets.push(saved.state.cookie);
 return value.user;
}
async function user(label){
 const folder=join(root,label);await mkdir(folder);const config=join(folder,'config.json');
 const p={folder,config,email:`agent91-${randomUUID()}@example.test`,password:`Test-${randomUUID()}`};profiles.push(p);secrets.push(p.password);
 const csrfResponse=await fetch(new URL('/api/v1/auth/csrf',base));const csrf=(await csrfResponse.json()).data;
 const cookie=csrfResponse.headers.getSetCookie().find(x=>x.startsWith('JSESSIONID=')).split(';')[0];
 const signup=await fetch(new URL('/api/v1/auth/signup',base),{method:'POST',headers:{Cookie:cookie,'Content-Type':'application/json',[csrf.headerName]:csrf.token},body:JSON.stringify({email:p.email,password:p.password,name:'Agent bootstrap integration'})});
 assert.equal(signup.status,201);raw(config,['init']);raw(config,['config','set','serverUrl',base.toString().replace(/^http:/,'ws:')]);
 p.user=await login(p);return p;
}
function create(p,tool){return run(p.config,['agent','create','--name','Bootstrap integration','--tool',tool,'--project-name','Isolated project','--project',p.folder]);}
async function runtime(p){return snapshot(p,'runtime.json');}
async function start(p){
 raw(p.config,['start']);
 const deadline=Date.now()+10_000;
 while(Date.now()<deadline){const r=await runtime(p);const response=await fetch(`http://127.0.0.1:${r.port}/health`,{headers:{Authorization:`Bearer ${r.controlToken}`}});const h=await response.json();if(h.centralServer==='Connected'||h.centralServer==='connected')return r;await delay(100);}
 throw new Error('Actual central WS did not become connected');
}
async function owned(marker,kill=false){
 assert.match(marker,/^devlink-bootstrap91-[0-9a-f-]{36}$/);
 const script=`$ErrorActionPreference='Stop'
$items=@(Get-CimInstance Win32_Process -Filter "Name = 'node.exe'" | Where-Object { $_.CommandLine -and $_.CommandLine.Contains($env:DEVLINK_HOST_MARKER) })
if ($env:DEVLINK_HOST_KILL -eq '1') { foreach ($item in $items) { Stop-Process -Id $item.ProcessId -Force -ErrorAction SilentlyContinue } }
ConvertTo-Json -Compress -InputObject @($items | ForEach-Object { $_.ProcessId })`;
 const r=await execute('powershell.exe',['-NoProfile','-NonInteractive','-Command',script],{env:{...env,DEVLINK_HOST_MARKER:marker,DEVLINK_HOST_KILL:kill?'1':'0'},windowsHide:true,timeout:10_000,maxBuffer:4096});return JSON.parse(r.stdout||'[]');
}
async function hostRead(host,p,file){
 const directory=join(root,`host-${host}`);await mkdir(directory);
 const marker=`devlink-bootstrap91-${randomUUID()}`;
 const entryPath=fileURLToPath(new URL('../../packages/mcp-server/dist/index.js',import.meta.url));
 const configuration=hostConfiguration(host,{directory,nodePath:process.execPath,entryPath,credentialPath:file,marker});
 if(configuration.mcp)await writeFile(join(directory,'mcp.json'),JSON.stringify(configuration.mcp));
 const prompt='Authorized DevLink bootstrap credential-reader test against our isolated local Rust broker and Spring/PostgreSQL. Use only the devlink MCP server. Call list_agents({}) exactly once. Do not retry, run commands, read/edit files, delegate, or use other tools. A CENTRAL_UNAUTHENTICATED response is expected because the provider READY does not supply business-session user/team identity yet. After the real call, report its error code. Do not attempt to repair the session.';
 let r;
 try{r=await captureHost(`${host}.exe`,[...configuration.args,prompt],{cwd:directory,timeoutMs:180_000});}
 finally{
  let remaining=await owned(marker);const deadline=Date.now()+5000;
  while(remaining.length&&Date.now()<deadline){await delay(150);remaining=await owned(marker);}
  if(remaining.length){await owned(marker,true);throw new Error('Host left owned MCP child processes');}
 }
 safe(`${r.stdout??''}${r.stderr??''}`);assert.equal(r.code,0);assert(!r.spawnError&&!r.timedOut&&!r.overflow);
 const transcript=readHostTranscript(host,r.stdout);assert.equal(transcript.calls.length,1);assert.equal(transcript.calls[0].tool,'list_agents');assert.deepEqual(transcript.calls[0].args,{});
 assert.equal(transcript.calls[0].value?.error?.code,'CENTRAL_UNAUTHENTICATED');
 if(host==='claude'){assert.equal(transcript.model,'claude-opus-5-5');assert.deepEqual(transcript.catalog.toSorted(),hostTools.toSorted());}
 const version=(await execute(`${host}.exe`,['--version'],{windowsHide:true,timeout:10_000})).stdout.trim();
 hostEvidence.push({host,version,model:transcript.model,configurationLoaded:true,mcpProcessStarted:true,credentialFileRead:true,error:'CENTRAL_UNAUTHENTICATED',businessSessionEstablished:false,ownedMcpProcessesAfterExit:0});
}
try{
 const a=await user('a');const team=run(a.config,['team','create','--name',`Bootstrap ${randomUUID()}`]);run(a.config,['team','select','--id',`${team.teamId}`]);
 const created=create(a,'codex');assert.equal(created.state,'ready');assert.equal(created.agent.ownerUserId,a.user.userId);assert.equal(created.agent.teamId,team.teamId);
 const bound=await snapshot(a,'agent-profile.json');secrets.push(bound.state.token);
 assert.equal(create(a,'codex').agent.agentId,created.agent.agentId);
 assert.equal(run(a.config,['agent','list']).length,1);
 const b=await user('b');run(b.config,['team','join','--code-stdin'],`${team.inviteCode}\n`);run(b.config,['team','select','--id',`${team.teamId}`]);const other=create(b,'claude-code');
 const bProfile=await snapshot(b,'agent-profile.json');secrets.push(bProfile.state.token);
 assert(run(a.config,['agent','use','--id',`${other.agent.agentId}`,'--project',a.folder],undefined,false).includes('ACCESS_DENIED'));
 const r=await start(a);secrets.push(r.controlToken);const file=raw(a.config,['mcp','path']).trim();
 const mcp=JSON.parse(await readFile(file,'utf8'));secrets.push(mcp.localToken);
 assert(mcp.localToken!==r.controlToken&&mcp.localToken!==bound.state.token,'Credential kinds must differ');
 const published=await readFile(file,'utf8');for(const s of [bound.state.token,r.controlToken,bProfile.state.token])assert(!published.includes(s));
 assert.deepEqual(Object.keys(mcp).sort(),['brokerUrl','localToken','schemaVersion']);
 const healthy=await fetch(`${mcp.brokerUrl}/health`,{headers:{Authorization:`Bearer ${mcp.localToken}`}});assert.equal(healthy.status,200);
 const denied=await fetch(`${mcp.brokerUrl}/shutdown`,{method:'POST',headers:{Authorization:`Bearer ${mcp.localToken}`}});assert.equal(denied.status,401);
 const claim=await fetch(`${mcp.brokerUrl}/v1/sessions`,{method:'POST',headers:{Authorization:`Bearer ${mcp.localToken}`,'Content-Type':'application/json'},body:JSON.stringify({requestId:'bootstrap-reader',instanceId:'actual-profile-reader',mode:'fresh'})});
 assert.equal(claim.status,503);assert.equal((await claim.json()).error.code,'CENTRAL_UNAUTHENTICATED');
 assert(run(a.config,['agent','token'],undefined,false).includes('Stop the broker'));
 for(const host of hosts)await hostRead(host,a,file);
 raw(a.config,['stop']);const second=await start(a);assert(second.controlToken!==r.controlToken,'Runtime control must rotate');assert.equal((await snapshot(a,'agent-profile.json')).state.agent.agentId,created.agent.agentId);raw(a.config,['stop']);
 await login(a);run(a.config,['agent','token']);const rotated=await snapshot(a,'agent-profile.json');secrets.push(rotated.state.token);assert(rotated.state.token!==bound.state.token,'Agent token must rotate');assert((JSON.parse(await readFile(file,'utf8'))).localToken!==mcp.localToken,'MCP token must rotate');
 assert.equal(run(a.config,['agent','list']).length,2);
 console.log(JSON.stringify({story:'S15P31D108-91',actualCentralApi:true,actualPostgreSQL:true,actualRustBroker:true,users:2,checks:['verified selected team','single creation and existing ID reuse','same-team foreign owner rejected','profile-backed WS with no DEVLINK_TOKEN','MCP/control/user/Agent credential separation','MCP path reader','explicit token rotation','same Agent after broker restart'],hosts:hostEvidence,businessSessionEstablished:false,businessSessionBlocker:'READY lacks user_id/team_id; story92',secretOutput:false}));
}finally{
 let cleanupFailed=false;
 for(const p of profiles)try{raw(p.config,['stop']);}catch{cleanupFailed=true;}
 assert(!cleanupFailed,'Owned broker cleanup failed; temporary profiles are preserved for recovery');
 assert.equal(dirname(resolve(root)),resolve(tmpdir()));assert(basename(root).startsWith('devlink-agent-bootstrap-'));
 await rm(root,{recursive:true,force:true});
}
