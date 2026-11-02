import {test} from 'node:test';
import assert from 'node:assert/strict';
import {mkdir,writeFile,readFile} from 'node:fs/promises';
import {join} from 'node:path';
import {execFile} from 'node:child_process';
import {promisify} from 'node:util';
import {privateDirectory} from './helpers/private-settings.mjs';
import {prepareCodexHome} from './helpers/host-profile.mjs';
test('private Codex profile cannot rotate the original OAuth refresh token',async()=>{
 const dir=await privateDirectory();
 try{
  const source=join(dir.dir,'source'),target=join(dir.dir,'private'),project=join(dir.dir,'project');for(const p of [source,target,project])await mkdir(p);
  const payload=Buffer.from(JSON.stringify({exp:Math.floor(Date.now()/1000)+7200})).toString('base64url');
  const auth={tokens:{access_token:'synthetic.'+payload+'.signature',refresh_token:'original-refresh-DO-NOT-COPY',id_token:'synthetic-id-token-for-watch'},last_refresh:new Date().toISOString()};
  await writeFile(join(source,'auth.json'),JSON.stringify(auth));await writeFile(join(source,'config.toml'),'model="fixture"\n[windows]\nsandbox="elevated"\n[mcp_servers.unrelated]\ncommand="never-copy"\n');
  const result=await prepareCodexHome({directory:target,project,originalHome:source,run:async(cmd,args,cwd)=>{const r=await promisify(execFile)(cmd,args,{cwd,windowsHide:true});return r.stdout;}});
  const copy=JSON.parse(await readFile(join(target,'auth.json'),'utf8'));assert.equal(copy.tokens.refresh_token,'');assert.equal(copy.tokens.access_token,auth.tokens.access_token);
  assert.equal(JSON.parse(await readFile(join(source,'auth.json'),'utf8')).tokens.refresh_token,auth.tokens.refresh_token);assert(!result.secrets.includes(auth.tokens.refresh_token));assert(result.secrets.includes(auth.tokens.access_token));
  const config=await readFile(join(target,'config.toml'),'utf8');assert(!config.includes('never-copy'));assert(config.includes('trust_level = "trusted"'));assert(result.refreshTokenSuppressed);
 }finally{await dir.cleanup();}
});

test('stale, expiring, incomplete or unsupported Codex logins fail before publication',async()=>{
 const fixture=await privateDirectory();
 try{
  const source=join(fixture.dir,'source'),project=join(fixture.dir,'project');await mkdir(source);await mkdir(project);
  for(const [i,auth]of [
   {tokens:{access_token:'x.'+Buffer.from(JSON.stringify({exp:Math.floor(Date.now()/1000)+10})).toString('base64url')+'.x',refresh_token:'never-copy'},last_refresh:new Date().toISOString()},
   {tokens:{access_token:'x.'+Buffer.from(JSON.stringify({exp:Math.floor(Date.now()/1000)+7200})).toString('base64url')+'.x',refresh_token:'never-copy'},last_refresh:new Date(Date.now()-172800000).toISOString()},
   {tokens:{access_token:'x.'+Buffer.from(JSON.stringify({exp:Math.floor(Date.now()/1000)+7200})).toString('base64url')+'.x',refresh_token:'never-copy'}},
   {}
  ].entries()){
   const target=join(fixture.dir,'rejected-'+i);await mkdir(target);await writeFile(join(source,'auth.json'),JSON.stringify(auth));
   await assert.rejects(prepareCodexHome({directory:target,project,originalHome:source,run:async(cmd,args,cwd)=>{const r=await promisify(execFile)(cmd,args,{cwd,windowsHide:true});return r.stdout;}}));
   await assert.rejects(readFile(join(target,'auth.json')));
  }
 }finally{await fixture.cleanup();}
});
