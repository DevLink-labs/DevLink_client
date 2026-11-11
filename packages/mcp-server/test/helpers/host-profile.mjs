import {readFile} from 'node:fs/promises';
import {join} from 'node:path';
import {createHash} from 'node:crypto';
export const authDigest=bytes=>createHash('sha256').update(bytes).digest('hex');
/** The bare version in a Host's `--version` output. It only reads the output: whether
 * that version is applied automatically stays the Rust provider's decision. */
export function hostVersion(tool,output){
 // Rust's `str::trim` keeps a byte order mark, so the product parser rejects it; do the same here.
 const raw=String(output),text=raw.includes('\uFEFF')?'':raw.trim(),codex=tool==='codex';
 const version=codex?(text.startsWith('codex-cli ')?text.slice(10):''):(text.endsWith(' (Claude Code)')?text.slice(0,-14):'');
 if(version.length>64||!/^\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.+-]*)?$/.test(version))throw new Error('Unrecognized '+(codex?'Codex':'Claude Code')+' version output');
 return version;
}
/** A private access-token-only login: the user's refresh token never reaches a child. */
export async function prepareCodexHome({directory,project,originalHome,run}){
 const script=String.raw`import os,json,tomllib,datetime,time,base64
from pathlib import Path
import sys
source=Path(sys.argv[1]);target=Path(sys.argv[2]);project=sys.argv[3]
data=tomllib.loads((source/'config.toml').read_text(encoding='utf8')) if (source/'config.toml').exists() else {}
def value(v):
 if isinstance(v,dict): return '{'+','.join(json.dumps(k)+' = '+value(x) for k,x in v.items())+'}'
 if isinstance(v,list): return '['+','.join(value(x) for x in v)+']'
 return json.dumps(v,ensure_ascii=False)
keys=['model','model_provider','model_providers','model_reasoning_effort','windows']
text='\n'.join(json.dumps(k)+' = '+value(data[k]) for k in keys if k in data)+'\n'
text+='[projects.'+json.dumps(project)+']\ntrust_level = "trusted"\n'
auth=json.loads((source/'auth.json').read_bytes())
tokens=auth.get('tokens')
if tokens:
 access=tokens.get('access_token','')
 claims=json.loads(base64.urlsafe_b64decode(access.split('.')[1]+'==='))
 last=datetime.datetime.fromisoformat(auth['last_refresh'].replace('Z','+00:00'))
 age=(datetime.datetime.now(datetime.timezone.utc)-last).total_seconds()
 if claims.get('exp',0)-time.time()<3600 or not 0<=age<86400: raise RuntimeError('A fresh existing login is required')
 tokens['refresh_token']=''
elif not auth.get('OPENAI_API_KEY'): raise RuntimeError('Unsupported existing login')
for name,content in [('config.toml',text),('auth.json',json.dumps(auth))]:
 (target/name).write_text(content,encoding='utf8');os.chmod(target/name,0o600)
secrets=[]
def collect(v,key=''):
 if isinstance(v,dict):
  for k,x in v.items(): collect(x,key+' '+k)
 elif isinstance(v,list):
  for x in v: collect(x,key)
 elif isinstance(v,str) and len(v)>=16 and any(k in key.lower() for k in ['token','key','secret','authorization','header']): secrets.append(v)
collect(auth);collect(data.get('model_providers',{}))
(target/'secret-watch.json').write_text(json.dumps(secrets),encoding='utf8');os.chmod(target/'secret-watch.json',0o600)
print('Private Codex access-only login prepared')`;
 await run('python',['-I','-c',script,originalHome,directory,project],project);
 const auth=await readFile(join(directory,'auth.json'));
 return {digest:authDigest(auth),secrets:JSON.parse(await readFile(join(directory,'secret-watch.json'),'utf8')),refreshTokenSuppressed:true};
}
