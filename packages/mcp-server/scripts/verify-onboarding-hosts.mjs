// S15P31D108-144: explicit opt-in, real Hosts + native Rust + isolated central.
import assert from 'node:assert/strict';
import {createServer,request} from 'node:http';
import {connect} from 'node:net';
import {spawn,execFile} from 'node:child_process';
import {promisify} from 'node:util';
import {randomUUID,createHash} from 'node:crypto';
import {createReadStream} from 'node:fs';
import {mkdir,writeFile,readFile,rename,readdir} from 'node:fs/promises';
import {join,resolve,isAbsolute,dirname,basename} from 'node:path';
import {homedir,tmpdir} from 'node:os';
import {pathToFileURL,fileURLToPath} from 'node:url';
const repoRoot=fileURLToPath(new URL('../../../',import.meta.url));
import {setTimeout as delay} from 'node:timers/promises';
import {Client} from '@modelcontextprotocol/client';
import {StdioClientTransport,getDefaultEnvironment} from '@modelcontextprotocol/client/stdio';
import {privateDirectory} from '../test/helpers/private-settings.mjs';
import {captureHost} from '../test/helpers/host-process.mjs';
import {readHostTranscript,hostTools} from '../test/helpers/host-transcript.mjs';
import {prepareCodexHome,authDigest,hostVersion} from '../test/helpers/host-profile.mjs';
import {verifyOnboardingProof} from '../test/helpers/onboarding-proof.mjs';

assert.equal(process.platform,'win32','This actual Host process-cleanup harness currently supports Windows only');
const abort=new AbortController();let cleaning=false;
const interrupt=()=>abort.abort();process.on('SIGINT',interrupt);process.on('SIGTERM',interrupt);
const central=new URL(process.env.DEVLINK_TEST_CENTRAL_URL??'');
assert(central.protocol==='http:'&&central.hostname==='127.0.0.1'&&central.pathname==='/'&&!central.search&&!central.hash&&!central.username&&!central.password,'Isolated IPv4 loopback central required');
const cli=process.env.DEVLINK_TEST_CLI,provider=process.env.DEVLINK_TEST_CONFIG_PROVIDER,entry=process.env.DEVLINK_TEST_MCP_ENTRY,artifacts=process.env.DEVLINK_TEST_HOST_ARTIFACTS;
for(const p of [cli,provider,entry,artifacts])assert(p&&isAbsolute(p),'Explicit absolute binary, entry and artifact paths required');
const commit=process.env.DEVLINK_TEST_CENTRAL_COMMIT;
assert(/^[a-f0-9]{40}$/.test(commit??''),'Exact central source commit required');
const artifact=join(artifacts,'onboarding-'+randomUUID());await mkdir(artifact,{recursive:true});
const fixture=await privateDirectory(),root=fixture.dir,profiles=[],secrets=[],proofs=[];
const counters={agentCreates:0,tokenIssues:0,agentMutations:0};const sockets=new Set();
const proxy=createServer((req,res)=>{
 const pathname=new URL(req.url,central).pathname;
 if(!['GET','HEAD','OPTIONS'].includes(req.method)&&pathname.startsWith('/api/v1/agents/'))counters.agentMutations++;
 if(req.method==='POST'&&/^\/api\/v1\/agents\/teams\/[0-9]+$/.test(pathname))counters.agentCreates++;
 if(req.method==='POST'&&/^\/api\/v1\/agents\/[0-9]+\/token$/.test(pathname))counters.tokenIssues++;
 const upstream=request(new URL(req.url,central),{method:req.method,headers:{...req.headers,host:central.host}},r=>{res.writeHead(r.statusCode,r.headers);r.pipe(res);});
 upstream.on('error',()=>{if(!res.headersSent)res.writeHead(502);res.end();});req.pipe(upstream);res.on('close',()=>upstream.destroy());
});
proxy.on('connection',s=>{sockets.add(s);s.on('close',()=>sockets.delete(s));});
proxy.on('upgrade',(req,socket,head)=>{
 const upstream=connect(Number(central.port),central.hostname,()=>{
  const headers=Object.entries({...req.headers,host:central.host}).flatMap(([k,v])=>Array.isArray(v)?v.map(s=>`${k}: ${s}`):[`${k}: ${v}`]);
  upstream.write(`${req.method} ${req.url} HTTP/1.1\r\n${headers.join('\r\n')}\r\n\r\n`);if(head.length)upstream.write(head);
  socket.pipe(upstream);upstream.pipe(socket);
 });
 upstream.on('error',()=>socket.destroy());socket.on('error',()=>upstream.destroy());socket.on('close',()=>upstream.destroy());
});
await new Promise(r=>proxy.listen(0,'127.0.0.1',r));const base=new URL(`http://127.0.0.1:${proxy.address().port}/`);
const env=Object.fromEntries(Object.entries(process.env).filter(([k])=>!/^DEVLINK_|^CODEX_|^CLAUDECODE$|^CLAUDE_CONFIG_DIR$|^CLAUDE_CODE_(SSE_PORT|ENTRYPOINT)$|^NODE_OPTIONS$/i.test(k)));
const hash=b=>createHash('sha256').update(b).digest('hex');
function safe(v){assert(secrets.every(s=>!s||!String(v).includes(s)),'Secret in shareable evidence');}
async function run(command,args,cwd,input='',timeout=65000,childEnv=env){
 if(!cleaning&&abort.signal.aborted)throw Error('Verification interrupted');
 return await new Promise((res,rej)=>{
  const child=spawn(command,args,{cwd,env:childEnv,windowsHide:true,stdio:['pipe','pipe','pipe']});let stdout='',stderr='',overflow=false;
  const terminate=()=>{if(child.exitCode===null&&child.signalCode===null)child.kill();};
  if(!cleaning)abort.signal.addEventListener('abort',terminate,{once:true});
  const timer=setTimeout(terminate,timeout);
  for(const [stream,key]of [[child.stdout,'out'],[child.stderr,'err']])stream.on('data',v=>{if(key==='out')stdout+=v;else stderr+=v;if(stdout.length+stderr.length>2_000_000){overflow=true;child.kill();}});
  child.once('error',()=>{clearTimeout(timer);rej(Error('Process spawn failed'));});
  child.once('close',code=>{clearTimeout(timer);abort.signal.removeEventListener('abort',terminate);try{safe(stdout+stderr);assert(!overflow);assert.equal(code,0,'Child command did not complete successfully');res(stdout);}catch(e){rej(e);}});child.stdin.end(input);
 });
}
async function raw(p,args,input){return run(cli,['--config',p.config,...args],p.folder,input);}
async function json(p,args,input){return JSON.parse(await raw(p,args,input));}
async function saved(p,name){return JSON.parse(await readFile(join(p.folder,'.devlink-runtime',name),'utf8'));}
async function until(fn,pred,label,ms=20000){const end=Date.now()+ms;while(Date.now()<end){if(abort.signal.aborted)throw Error('Verification interrupted');try{const v=await fn();if(pred(v))return v;}catch{}await delay(250);}throw Error('Timed out: '+label);}
async function start(p){activeStep=p.tool+'/broker-start';p.started=true;await raw(p,['start']);p.descriptor=await saved(p,'broker.json');secrets.push(p.descriptor.localToken,(await saved(p,'runtime.json')).controlToken);await until(async()=> (await fetch(p.descriptor.brokerUrl+'/health',{headers:{Authorization:'Bearer '+p.descriptor.localToken},signal:AbortSignal.timeout(5000)})).json(),h=>h.centralServer==='Connected','central readiness');}
async function stop(p){if(!cleaning)activeStep=p.tool+'/broker-stop';if(p.started){await raw(p,['stop']);p.started=false;}}
async function prepare(label,tool){activeStep='prepare/'+tool;
 const folder=join(root,label);await mkdir(folder);const p={label,tool,folder,config:join(folder,'config.json')};profiles.push(p);
 await run('git',['init','--quiet','--initial-branch=codex/onboarding',folder],folder);
 const email='onboarding-'+randomUUID()+'@example.test',password='Test-'+randomUUID();secrets.push(password);
 const csrfResponse=await fetch(new URL('/api/v1/auth/csrf',base),{signal:AbortSignal.timeout(5000)}),csrf=(await csrfResponse.json()).data;
 secrets.push(csrf.token);const cookie=csrfResponse.headers.getSetCookie().find(v=>v.startsWith('JSESSIONID=')).split(';')[0];secrets.push(cookie,cookie.slice(cookie.indexOf('=')+1));
 const sign=await fetch(new URL('/api/v1/auth/signup',base),{method:'POST',headers:{Cookie:cookie,'Content-Type':'application/json',[csrf.headerName]:csrf.token},body:JSON.stringify({email,password,name:'Synthetic onboarding '+label}),signal:AbortSignal.timeout(5000)});assert.equal(sign.status,201);
 await raw(p,['init']);await raw(p,['config','set','serverUrl',base.toString().replace(/^http:/,'ws:')]);await raw(p,['config','set','localPort','0']);
 p.user=(await json(p,['auth','login','--email',email,'--password-stdin'],password+'\n')).user;const loginCookie=(await saved(p,'user-session.json')).state.cookie;secrets.push(loginCookie,...loginCookie.split(';').map(v=>v.slice(v.indexOf('=')+1).trim()));
 return p;
}
async function configure(p){activeStep=p.tool+'/configure';
 const made=await json(p,['agent','create','--name','Onboarding '+p.label,'--tool',p.tool==='codex'?'codex':'claude-code','--project-name','Synthetic '+p.label,'--project',p.folder]);assert.equal(made.state,'ready');p.agent=made.agent;
 p.initialToken=(await saved(p,'agent-profile.json')).state.token;secrets.push(p.initialToken);await start(p);
 p.wrapper=join(p.folder,'devlink-onboarding-'+randomUUID()+'.mjs');await writeFile(p.wrapper,`import ${JSON.stringify(pathToFileURL(entry).href)};\n`,{mode:0o600});
 p.target=p.tool==='codex'?join(p.folder,'.codex','config.toml'):join(p.folder,'.mcp.json');
 const input={toolType:p.tool,projectPath:p.folder,targetPath:p.target,brokerFile:join(p.folder,'.devlink-runtime','broker.json'),executable:{command:process.execPath,args:[p.wrapper,'--transport','stdio']}};
 const started=Date.now(),output=JSON.parse(await run(provider,['--apply'],p.folder,JSON.stringify({hostVersion:hostVersions[p.label],input})));assert.equal(output.inspection.status,'applied');
 p.configApplyMs=Date.now()-started;p.configHash=hash(await readFile(p.target));safe(await readFile(p.target,'utf8'));
}
const normal='Call only these DevLink MCP tools, sequentially, with empty object arguments: get_my_context, get_connection_status, register_agent, get_connection_status, get_my_context, register_agent. Do not supply name/toolType/role. Do not use any other tools or read files, secrets, shell or network. Finish after the six calls, even if any returns an error. Briefly state completion.';
const diagnostic='Call only get_connection_status with empty object arguments once. No other tool or file/network/shell access. Finish immediately after the tool result, even if it reports an error.';
const conflict='Call only get_connection_status then register_agent, both with empty object arguments sequentially. Do not retry any errors. No other tool or file/network/shell access. Finish after the two results.';
async function owned(p,kill=false){
 const script="$p=@(Get-CimInstance Win32_Process -Filter \"Name='node.exe'\" | Where-Object {$_.CommandLine -and $_.CommandLine.Contains($env:DEVLINK_ONBOARDING_MARKER)});if($env:DEVLINK_ONBOARDING_KILL -eq '1'){$p | ForEach-Object {Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue}};ConvertTo-Json -Compress -InputObject @($p | ForEach-Object {$_.ProcessId})";
 const r=await promisify(execFile)('powershell.exe',['-NoProfile','-NonInteractive','-Command',script],{env:{...env,DEVLINK_ONBOARDING_MARKER:p.wrapper,DEVLINK_ONBOARDING_KILL:kill?'1':'0'},windowsHide:true,timeout:10000});return JSON.parse(r.stdout||'[]');
}
const verificationFiles=[import.meta.url,...['host-process.mjs','host-profile.mjs','host-transcript.mjs','onboarding-proof.mjs','private-settings.mjs'].map(name=>new URL('../test/helpers/'+name,import.meta.url))];
const verificationCodeHash=hash(JSON.stringify(await Promise.all(verificationFiles.map(async path=>({file:basename(new URL(path).pathname),sha256:await fileHash(new URL(path))})))));
const versions={},hostVersions={},hostSupport={},binaries={};let activeStep='preparation';
async function codexProfile(p){
 if(p.codexHome)return;activeStep='codex/private-profile';
 p.codexHome=join(p.folder,'isolated-codex-home');await mkdir(p.codexHome);
 const prepared=await prepareCodexHome({directory:p.codexHome,project:p.folder,originalHome:process.env.CODEX_HOME??join(homedir(),'.codex'),run});
 secrets.push(...prepared.secrets);p.authDigest=prepared.digest;
 const inventory=JSON.parse(await run(binaries.codex.path,['mcp','list','--json'],p.folder,'',65000,{...env,CODEX_HOME:p.codexHome}));
 assert.deepEqual(inventory.map(v=>v.name).sort(),['devlink'],'Private Codex project inventory is not isolated');
 const transport=inventory[0].transport;
 assert.equal(transport.command.toLowerCase(),process.execPath.toLowerCase());assert.deepEqual(transport.args,[p.wrapper,'--transport','stdio']);
 assert.equal(transport.type,'stdio');assert.deepEqual(transport.env,{DEVLINK_BROKER_FILE:join(p.folder,'.devlink-runtime','broker.json')});
}
async function host(p,phase,prompt=normal){
 let args;
 if(p.tool==='claude_code')args=['-p','--model','claude-opus-5-5','--effort','high','--restricted','--settings','{"disableAllHooks":true}','--strict-mcp-config','--mcp-config',p.target,'--tools','','--allowedTools',hostTools.map(t=>'mcp__devlink__'+t).join(','),'--permission-mode','dontAsk','--permission-prompts','none','--disable-slash-commands','--no-session-persistence','--no-chrome','--output-format','stream-json','--verbose',prompt];
 else {await codexProfile(p);args=['--no-daemon','-a','never','exec','--ephemeral','--skip-git-repo-check','--sandbox','read-only','--json','--cd',p.folder,'-c',`projects.${JSON.stringify(p.folder)}.trust_level="trusted"`,'-c','mcp_servers.devlink.default_tools_approval_mode="approve"','-c','mcp_optional_startup_grace_ms=0',prompt];}
 activeStep=p.tool+'/'+phase;const started=Date.now();console.log(JSON.stringify({host:p.tool,phase,state:'running'}));
 let captured;
 try {captured=await captureHost(binaries[p.tool==='codex'?'codex':'claude'].path,args,{cwd:p.folder,env:{...env,...(p.codexHome?{CODEX_HOME:p.codexHome}:{})},timeoutMs:240000,signal:abort.signal});}
 finally{const deadline=Date.now()+6000;let left=await owned(p);while(left.length&&Date.now()<deadline){await delay(200);left=await owned(p);}if(left.length){await owned(p,true);throw Error('Host left owned MCP process');}}
 if(p.tool==='codex'){
  const auth=await readFile(join(p.codexHome,'auth.json'));const value=JSON.parse(auth);
  for(const token of Object.values(value.tokens??{}))if(typeof token==='string'&&token.length>=16)secrets.push(token);
  assert.equal(authDigest(auth),p.authDigest,'Isolated access-only login changed');
 }
 safe(captured.stdout+captured.stderr);await writeFile(join(artifact,p.tool+'-'+phase+'.jsonl'),captured.stdout);await writeFile(join(artifact,p.tool+'-'+phase+'.stderr.txt'),captured.stderr);
 assert.equal(captured.code,0,'Host did not exit successfully');assert(!captured.timedOut&&!captured.overflow&&!captured.interrupted&&!captured.spawnError,'Host capture failed');
 const parsed=readHostTranscript(p.tool==='codex'?'codex':'claude',captured.stdout);if(p.tool==='claude_code')assert.equal(parsed.model,'claude-opus-5-5');
 if(p.tool==='claude_code'){const events=captured.stdout.split(/\r?\n/).filter(Boolean).map(v=>JSON.parse(v));const init=events.find(v=>v.type==='system'&&v.subtype==='init');assert.deepEqual(init.mcp_servers.map(v=>v.name),['devlink']);const final=captured.stdout.split(/\r?\n/).filter(Boolean).map(v=>JSON.parse(v)).findLast(v=>v.type==='result');assert.deepEqual(Object.keys(final?.modelUsage??{}),['claude-opus-5-5']);
  // The server may expose tools added after this harness was written; the allowed ones must all be offered.
  assert(hostTools.every(t=>parsed.catalog.includes(t)),'Claude catalog lacks an allowed DevLink tool');}
 const proof=verifyOnboardingProof(parsed,{phase,teamId:String(p.agent.teamId),agentId:String(p.agent.agentId),userId:String(p.user.userId),tool:p.tool});
 proofs.push({host:p.tool,phase,...proof,elapsedMs:Date.now()-started,transcriptSha256:hash(captured.stdout),model:parsed.model??'Copied user model preference; resolved model not reported',requestedEffort:p.tool==='claude_code'?'high':'existing user preference'});
 console.log(JSON.stringify({host:p.tool,phase,state:'passed'}));
}
async function fileHash(path){const digest=createHash('sha256');for await(const chunk of createReadStream(path))digest.update(chunk);return digest.digest('hex');}
async function distFiles(path,prefix=''){
 const items=[];for(const f of await readdir(path,{withFileTypes:true})){assert(!f.isSymbolicLink());const p=join(path,f.name),name=prefix+f.name;if(f.isDirectory())items.push(...await distFiles(p,name+'/'));else if(f.isFile())items.push({path:name,sha256:await fileHash(p)});}return items.sort((a,b)=>a.path.localeCompare(b.path));
}
async function metadata(p){
 const response=await fetch(new URL('/api/v1/agents/me',base),{headers:{Authorization:'Bearer '+p.initialToken},signal:AbortSignal.timeout(5000)});assert(response.ok);const value=(await response.json()).data;
 return Object.fromEntries(['agentId','teamId','ownerUserId','name','roleDescription','toolType','projectName','createdAt'].map(k=>[k,value[k]]));
}
async function localSession(p){
 const response=await fetch(p.descriptor.brokerUrl+'/v1/connection',{headers:{Authorization:'Bearer '+p.descriptor.localToken,'x-instance-id':randomUUID(),'x-request-id':randomUUID()},signal:AbortSignal.timeout(10000)});
 assert(response.ok,'Read-only session probe failed');return (await response.json()).data.hostSession;
}
let failure,baseline;const cleanup={brokers:true,fixture:false,proxy:false};
try{
 for(const [label,command]of [['codex',process.env.DEVLINK_TEST_CODEX??'codex.exe'],['claude',process.env.DEVLINK_TEST_CLAUDE??'claude.exe']]){const path=isAbsolute(command)?command:(await run('where.exe',[command],root)).trim().split(/\r?\n/)[0];binaries[label]={path,sha256:await fileHash(path)};}
 versions.codex=(await run(binaries.codex.path,['--version'],root)).trim();versions.claude=(await run(binaries.claude.path,['--version'],root)).trim();
 // The detected version goes to the Rust provider unchanged, so the harness accepts exactly the Hosts setup applies automatically.
 activeStep='preparation/host-version';hostVersions.codex=hostVersion('codex',versions.codex);hostVersions.claude=hostVersion('claude_code',versions.claude);
 // Ask the provider before anything is created on the central server.
 for(const [label,toolType]of [['codex','codex'],['claude','claude_code']]){activeStep='preparation/host-support/'+label;hostSupport[label]=JSON.parse(await run(provider,['--supports'],root,JSON.stringify({toolType,hostVersion:hostVersions[label]}))).supported===true;assert(hostSupport[label],'setup does not apply this Host version automatically');}
 activeStep='preparation';
 const a=await prepare('codex','codex'),b=await prepare('claude','claude_code');
 activeStep='team/preparation';const team=await json(a,['team','create','--name','Synthetic onboarding '+randomUUID()]);secrets.push(team.inviteCode);await raw(a,['team','select','--id',String(team.teamId)]);await raw(b,['team','join','--code-stdin'],team.inviteCode+'\n');await raw(b,['team','select','--id',String(team.teamId)]);
 await configure(a);await configure(b);activeStep='preparation/request-baseline';baseline={...counters};assert.deepEqual(baseline,{agentCreates:2,tokenIssues:2,agentMutations:4});
 for(const p of profiles){activeStep=p.tool+'/metadata-baseline';p.metadata=await metadata(p);}
 for(const p of profiles){
  await host(p,'initial');
  // A Host may kill its stdio child without delivering DELETE. Never steal the old lease.
  activeStep=p.tool+'/prior-lease-probe';const leaseProbe=await localSession(p);
  if(leaseProbe==='conflict')await host(p,'early-restart-conflict',conflict);
  activeStep=p.tool+'/prior-lease-wait';const waitStart=Date.now();console.log(JSON.stringify({host:p.tool,state:'waiting-for-prior-lease'}));
  await until(()=>localSession(p),v=>v==='unbound','old Host lease release',105000);p.recoveryWaitMs=Date.now()-waitStart;
  await host(p,'host-restart');await stop(p);await start(p);await host(p,'broker-restart');
  await stop(p);await start(p);
  activeStep=p.tool+'/holder-acquire';const holder=new Client({name:'onboarding-owner',version:'1'},{versionNegotiation:{mode:'legacy'}});
  const transport=new StdioClientTransport({command:process.execPath,args:[entry,'--transport','stdio'],env:{...getDefaultEnvironment(),...env,DEVLINK_BROKER_FILE:join(p.folder,'.devlink-runtime','broker.json')},stderr:'pipe'});let holderErr='';transport.stderr.on('data',v=>{holderErr+=v;if(holderErr.length>2_000_000)transport.close();});
  try{await holder.connect(transport);const result=await holder.callTool({name:'register_agent',arguments:{}});assert.notEqual(result.isError,true);assert.equal(result.structuredContent.data.agentId,String(p.agent.agentId));await host(p,'ownership-conflict',conflict);activeStep=p.tool+'/holder-retained';const held=await holder.callTool({name:'get_my_context',arguments:{}});assert.equal(held.structuredContent.data.hostSession,'bound');assert.equal(held.structuredContent.data.principal.agentId,String(p.agent.agentId));p.ownerRetained=true;}finally{await holder.close();safe(holderErr);}
  await stop(p);await host(p,'broker-stopped',diagnostic);await start(p);await host(p,'recovered');
  activeStep=p.tool+'/credential-file-hide';const path=join(p.folder,'.devlink-runtime','broker.json'),missing=path+'.held-by-test';await rename(path,missing);
  try{await host(p,'credentials-missing',diagnostic);}finally{try{await rename(missing,path);}catch(e){activeStep=p.tool+'/credential-file-restore';throw e;}}
  await host(p,'credentials-restored',diagnostic);activeStep=p.tool+'/final-profile-checks';assert.deepEqual(await metadata(p),p.metadata);
  assert.equal((await saved(p,'agent-profile.json')).state.token,p.initialToken,'Token changed during onboarding');assert.equal(hash(await readFile(p.target)),p.configHash,'Host changed its configuration');
 }
 activeStep='final-counter-check';assert.deepEqual(counters,baseline,'Host flow created Agent or issued token');
}catch(e){failure=e;console.error('Onboarding verification failed at '+activeStep+' ('+(['AssertionError','SyntaxError','Error'].includes(e.name)?e.name:'VerificationError')+')');}
finally{
 cleaning=true;for(const p of profiles){try{await stop(p);if(p.wrapper)assert.equal((await owned(p)).length,0);}catch{cleanup.brokers=false;if(!failure)activeStep='cleanup/brokers';failure??=Error('Owned process cleanup failed');}}
 try{for(const s of sockets)s.destroy();await new Promise(r=>proxy.close(r));cleanup.proxy=true;}catch{if(!failure)activeStep='cleanup/proxy';failure??=Error('Proxy cleanup failed');}
 if(cleanup.brokers)try{assert.equal(dirname(resolve(root)),resolve(tmpdir()));assert(basename(root).startsWith('devlink-test-'));await fixture.cleanup();cleanup.fixture=true;}catch{if(!failure)activeStep='cleanup/private-fixture';failure??=Error('Private fixture cleanup failed');}
 process.off('SIGINT',interrupt);process.off('SIGTERM',interrupt);
}
const scenarios=profiles.flatMap(p=>['initial','host-restart','broker-restart','ownership-conflict','broker-stopped','recovered','credentials-missing','credentials-restored'].map(phase=>({host:p.tool,phase,status:proofs.some(v=>v.host===p.tool&&v.phase===phase)?'verified':'unverified'})));
const report={story:'S15P31D108-144',passed:!failure,versions,hostVersions,hostSupport,binaries,clientSourceCommit:(await run('git',['rev-parse','HEAD'],repoRoot)).trim(),sourceTreeDirty:Boolean((await run('git',['status','--porcelain=v1'],repoRoot)).trim()),verificationCodeSha256:verificationCodeHash,mcpDistSha256:hash(JSON.stringify(await distFiles(dirname(entry)))),cleanup,scenarios,retainedFixturePath:cleanup.fixture?null:root,failedStep:failure?activeStep:null,actualLlmHostsExecuted:[...new Set(proofs.map(p=>p.host))],serverCommit:commit,cliSha256:hash(await readFile(cli)),mcpEntrySha256:hash(await readFile(entry)),providerSha256:hash(await readFile(provider)),setupRequests:baseline,finalRequests:counters,additionalAgentCreates:baseline?counters.agentCreates-baseline.agentCreates:null,additionalTokenIssues:baseline?counters.tokenIssues-baseline.tokenIssues:null,metadataAdditionalInputs:!failure&&scenarios.length===16&&scenarios.every(s=>s.status==='verified')&&proofs.some(p=>p.registerCalls>0)&&proofs.filter(p=>p.registerCalls>0).every(p=>p.metadataInputs===0)?0:null,profiles:profiles.map(p=>({host:p.tool,agentId:p.agent?String(p.agent.agentId):null,configApplyMs:p.configApplyMs,recoveryWaitMs:p.recoveryWaitMs,ownerRetained:p.ownerRetained})),proofs,limitations:['Manual metadata baseline timing was not measured; do not claim a speedup.','Other fault classes use dedicated unit/native provider regressions, not simulated success in this actual Host report.']};
safe(JSON.stringify(report));await writeFile(join(artifact,'report.json'),JSON.stringify(report,null,2));console.log(JSON.stringify({artifact,report},null,2));if(failure)process.exitCode=1;
