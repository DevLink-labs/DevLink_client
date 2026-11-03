// Real central + DB verification using two synthetic users on loopback only.
import assert from 'node:assert/strict';
import {randomUUID} from 'node:crypto';
import {spawnSync} from 'node:child_process';
import {mkdtemp,mkdir,readFile,writeFile,rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join,resolve,dirname,basename,isAbsolute} from 'node:path';
const base=new URL(process.env.DEVLINK_TEST_CENTRAL_URL??'');
assert(base.protocol==='http:' && ['localhost','127.0.0.1','[::1]'].includes(base.hostname) && base.pathname==='/' && !base.search && !base.username && !base.password);
const cli=process.env.DEVLINK_TEST_CLI;
assert(cli&&isAbsolute(cli));
const parent=resolve(tmpdir());
const root=await mkdtemp(join(parent,'devlink-team-setup-'));
const env=Object.fromEntries(Object.entries(process.env).filter(([k])=>!/^DEVLINK_/i.test(k)));
const secrets=[];
function run(config,args,input,ok=true) {
 const r=spawnSync(cli,['--config',config,...args],{env,input,encoding:'utf8',timeout:45_000,windowsHide:true});
 const text=`${r.stdout??''}${r.stderr??''}`;
 assert(secrets.every(s=>!text.includes(s)),'CLI exposed an authentication secret');
 assert(!r.error && (ok?r.status===0:r.status!==0),'Unexpected CLI outcome');
 return ok?JSON.parse(r.stdout||'{}'):text;
}
async function user(name) {
 const folder=join(root,name);await mkdir(folder);
 const config=join(folder,'config.json');
 const password=`Test-${randomUUID()}`;const email=`team90-${randomUUID()}@example.test`;
 secrets.push(password);
 const csrfResponse=await fetch(new URL('/api/v1/auth/csrf',base));
 const csrf=(await csrfResponse.json()).data;
 const cookie=csrfResponse.headers.getSetCookie().find(x=>x.startsWith('JSESSIONID=')).split(';')[0];
 const signup=await fetch(new URL('/api/v1/auth/signup',base),{method:'POST',headers:{Cookie:cookie,'Content-Type':'application/json',[csrf.headerName]:csrf.token},body:JSON.stringify({email,password,name:'Team integration'})});
 assert(signup.status===201);
 // Init prints a short status line rather than JSON.
 const init=spawnSync(cli,['--config',config,'init'],{env,encoding:'utf8',windowsHide:true});assert(init.status===0);
 const set=spawnSync(cli,['--config',config,'config','set','serverUrl',base.toString().replace(/^http:/,'ws:')],{env,encoding:'utf8',windowsHide:true});assert(set.status===0);
 const logged=run(config,['auth','login','--email',email,'--password-stdin'],`${password}\n`);
 const snapshot=JSON.parse(await readFile(join(folder,'.devlink-runtime','user-session.json'),'utf8'));
 secrets.push(snapshot.state.cookie);
 return {config,folder,userId:logged.user.userId};
}
try {
 const a=await user('a'),b=await user('b');
 const ta=run(a.config,['team','create','--name',`Team A ${randomUUID()}`]);
 const tb=run(b.config,['team','create','--name',`Team B ${randomUUID()}`]);
 assert(ta.myRole==='OWNER'&&tb.myRole==='OWNER'&&ta.inviteCode);
 const members=run(a.config,['team','members','--id',`${ta.teamId}`]);
 assert(members.some(m=>m.userId===a.userId&&m.role==='OWNER'));
 assert(run(a.config,['team','current']).selected===false);
 run(a.config,['team','select','--id',`${ta.teamId}`]);
 const selectionPath=join(a.folder,'.devlink-runtime','selected-team.json');
 const before=await readFile(selectionPath,'utf8');
 const forbidden=run(a.config,['team','select','--id',`${tb.teamId}`],undefined,false);
 assert(forbidden.includes('ACCESS_DENIED'));
 assert(before===await readFile(selectionPath,'utf8'));
 const invalid=run(b.config,['team','join','--code-stdin'],'invalid-test-code\n',false);
 assert(invalid.includes('RESOURCE_NOT_FOUND'));
 const joined=run(b.config,['team','join','--code-stdin'],`${ta.inviteCode}\n`);
 const duplicate=run(b.config,['team','join','--code-stdin'],`${ta.inviteCode}\n`);
 assert(joined.myRole==='MEMBER'&&!joined.replayed&&duplicate.replayed);
 assert(!JSON.stringify(joined).includes(ta.inviteCode));
 run(b.config,['team','select','--id',`${ta.teamId}`]);
 assert(run(b.config,['team','current']).team.teamId===ta.teamId);
 const two=run(a.config,['team','members','--id',`${ta.teamId}`]);
 assert(two.filter(m=>m.userId===b.userId).length===1);
 const selected=JSON.parse(await readFile(join(b.folder,'.devlink-runtime','selected-team.json'),'utf8'));
 assert(selected.userId===b.userId);
 // A cached team ID is not proof that a different user selected it.
 await writeFile(join(b.folder,'.devlink-runtime','selected-team.json'),before);
 assert(run(b.config,['team','current'],undefined,false).includes('current central user'));
 run(b.config,['team','select','--id',`${ta.teamId}`]);
 run(b.config,['auth','logout']);
 assert(run(b.config,['team','list'],undefined,false).includes('USER_LOGIN_REQUIRED'));
 run(a.config,['auth','logout']);
 console.log(JSON.stringify({story:'S15P31D108-90',actualCentralApi:true,users:2,checks:['create owner membership','explicit selection','cross-team denial preserves selection','invalid invite','idempotent join','verified user binding','logout blocks team commands'],secretOutput:false}));
} finally {
 assert(dirname(resolve(root))===parent&&basename(root).startsWith('devlink-team-setup-'));
 await rm(root,{recursive:true,force:true});
}
