// Shared MCP SDK stdio driver for real-central scripts and the client·server·frontend E2E harness.
// It only talks to an already running local broker through dist/index.js; it never starts a broker.
import assert from 'node:assert/strict';
import {fileURLToPath} from 'node:url';
import {Client} from '@modelcontextprotocol/client';
import {getDefaultEnvironment,StdioClientTransport} from '@modelcontextprotocol/client/stdio';

export const DEFAULT_ENTRY=fileURLToPath(new URL('../../dist/index.js',import.meta.url));

// Tools a real-central check relies on. Compare names, not a count, so adding a tool does not
// break the scripts (DevLink_client#88).
export const REQUIRED_TOOLS=Object.freeze([
  'devlink_ping','get_my_context','get_connection_status',
  'register_agent','list_agents','update_agent','create_task','update_task_status','complete_task',
  'get_tasks','get_task_result','ask_question','answer_question','get_updates',
]);

/** Fails when any required tool is missing; returns every listed name. */
export async function assertRequiredTools(client,required=REQUIRED_TOOLS) {
  const names=(await client.listTools()).tools.map(t=>t.name);
  const missing=required.filter(name=>!names.includes(name));
  assert.deepEqual(missing,[],'MCP tools missing: '+missing.join(','));
  return names;
}

/** Drops DEVLINK_* so a developer's own broker or token cannot leak into a child process. */
export function withoutDevlinkEnv(source=process.env) {
  return Object.fromEntries(Object.entries(source).filter(([key,value])=>value!==undefined&&!/^DEVLINK_/i.test(key)));
}

/**
 * Starts `node <entry> --transport stdio` bound to one broker descriptor and connects an SDK client.
 * stderr is captured up to 2 MB for later secret checks.
 */
export async function connectMcpStdio({brokerFile,name,entry=DEFAULT_ENTRY,node=process.execPath,env=withoutDevlinkEnv()}) {
  const client=new Client({name,version:'1'},{versionNegotiation:{mode:'legacy'}});
  const transport=new StdioClientTransport({command:node,args:[entry,'--transport','stdio'],
    env:{...getDefaultEnvironment(),...withoutDevlinkEnv(env),DEVLINK_BROKER_FILE:brokerFile},stderr:'pipe'});
  const session={client,stderr:'',stderrOverflow:false};
  transport.stderr.on('data',chunk=>{
    if(session.stderr.length+chunk.length<=2_000_000)session.stderr+=chunk.toString();else session.stderrOverflow=true;
  });
  session.close=()=>client.close();
  try{await client.connect(transport);}
  catch(error){await transport.close().catch(()=>{});throw error;}
  return session;
}

/**
 * Calls a tool and checks that text and structured output agree. With expectedCode the call must
 * fail with that domain code; otherwise it must succeed.
 */
export async function callTool(client,name,args,expectedCode) {
  const r=await client.callTool({name,arguments:args});const v=r.structuredContent;
  assert.deepEqual(JSON.parse(r.content[0].text),v,'text and structured output differ');
  if(expectedCode){assert.equal(r.isError,true,name+' should fail with '+expectedCode);assert.equal(v.error.code,expectedCode);return v;}
  assert.notEqual(r.isError,true,'MCP operation failed: '+name+' '+v?.error?.code);return v;
}
