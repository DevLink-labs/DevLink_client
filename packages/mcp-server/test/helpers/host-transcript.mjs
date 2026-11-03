import assert from 'node:assert/strict';

export const hostTools = ['devlink_ping', 'register_agent', 'list_agents', 'create_task', 'update_task_status', 'complete_task', 'get_tasks', 'get_task_result', 'get_my_context', 'get_connection_status'];

function decodeResult(result, hostError) {
  if (typeof result === 'string') result = { content: result };
  const structured = result?.structuredContent ?? result?.structured_content ?? undefined;
  const parts = typeof result?.content === 'string' ? [{ text: result.content }] : result?.content ?? [];
  const text = parts.filter((part) => typeof part.text === 'string').map((part) => part.text).join('\n');
  let parsed;
  try { parsed = JSON.parse(text); } catch { /* SDK argument validation is intentionally plain text. */ }
  if (structured !== undefined && parsed !== undefined) assert.deepEqual(structured, parsed, 'Text and structured result differ');
  const value = structured ?? parsed;
  const errorText = typeof hostError === 'string' ? hostError : hostError?.message ?? '';
  return {
    value,
    inputError: Boolean((result?.isError || result?.is_error || hostError) && `${text}\n${errorText}`.includes('Input validation error')),
    hostError: Boolean(hostError),
    representation: structured !== undefined ? 'structured' : parsed !== undefined ? 'json-text' : 'native-error',
  };
}

/** Use actual CLI tool events, never the model's prose, as evidence that a tool ran. */
export function readHostTranscript(host, jsonl) {
  const calls = new Map(); const unexpected = []; let catalog = []; let model = null; let finalText = '';
  let completed = false;
  for (const line of jsonl.split(/\r?\n/).filter((line) => line.trim())) {
    const event = JSON.parse(line);
    if (host === 'codex') {
      const item = event.item;
      if (item?.type === 'mcp_tool_call') {
        if (item.server !== 'devlink') { unexpected.push(item.server); continue; }
        if (!calls.has(item.id)) assert.ok([...calls.values()].every((call) => call.completed), 'Host overlapped tool invocations');
        const current = calls.get(item.id) ?? { id: item.id, tool: item.tool, args: item.arguments };
        if (event.type === 'item.completed') Object.assign(current, decodeResult(item.result, item.error ?? (item.status === 'failed' ? true : undefined)), { completed: true });
        calls.set(item.id, current);
      } else if (item?.type === 'agent_message' && event.type === 'item.completed') finalText = item.text;
      else if (item && !['agent_message', 'reasoning'].includes(item.type)) unexpected.push(item.type);
      if (event.type === 'turn.completed') completed = true;
    } else if (host === 'claude') {
      if (event.type === 'system' && event.subtype === 'init') {
        catalog = (event.tools ?? []).filter((name) => name.startsWith('mcp__devlink__')).map((name) => name.slice('mcp__devlink__'.length));
        model = event.model ?? null;
      }
      for (const part of event.message?.content ?? []) {
        if (part.type === 'tool_use') {
          if (!part.name.startsWith('mcp__devlink__')) { unexpected.push(part.name); continue; }
          assert.ok([...calls.values()].every((call) => call.completed), 'Host overlapped tool invocations');
          calls.set(part.id, { id: part.id, tool: part.name.slice('mcp__devlink__'.length), args: part.input });
        } else if (part.type === 'tool_result') {
          const current = calls.get(part.tool_use_id);
          if (!current) throw new Error('Tool result has no matching invocation');
          // Claude's convenience tool_use_result wraps errors in "Error: " and drops
          // structuredContent. The correlated tool_result is the actual model-visible content.
          const result = { content: part.content, isError: part.is_error,
            structuredContent: event.message.content.filter((part) => part.type === 'tool_result').length === 1 ? event.tool_use_result?.structuredContent : undefined };
          Object.assign(current, decodeResult(result, part.is_error ? part.content : undefined), { completed: true });
        }
      }
      if (event.type === 'result') { completed = event.is_error === false; finalText = event.result ?? ''; }
    } else throw new Error('Unsupported Host');
  }
  assert.ok(completed, 'Host did not complete a successful turn');
  assert.equal(unexpected.length, 0, 'Host used a tool outside the isolated DevLink fixture');
  const result = [...calls.values()];
  assert.ok(result.every((call) => call.completed), 'Host left an incomplete tool invocation');
  return { calls: result, catalog, model, finalText };
}

export function parseHostAnswer(text) {
  const fenced = text.trim().match(/```(?:json)?\s*\n([^`]*?)\n```$/);
  return JSON.parse(fenced ? fenced[1] : text.trim());
}

export function hostConfiguration(host, { directory, nodePath, entryPath, credentialPath, marker }) {
  const serverArgs = [`--title=${marker}`, entryPath, '--transport', 'stdio', '--request-timeout-ms', '200', '--call-timeout-ms', '3000'];
  if (host === 'claude') return {
    mcp: { mcpServers: { devlink: { command: nodePath, args: serverArgs, env: { DEVLINK_BROKER_FILE: credentialPath } } } },
    args: ['-p', '--model', 'claude-opus-5-5', '--effort', 'high', '--restricted', '--strict-mcp-config', '--mcp-config', `${directory}/mcp.json`,
      '--tools', '', '--allowedTools', hostTools.map((tool) => `mcp__devlink__${tool}`).join(','),
      '--permission-mode', 'dontAsk', '--permission-prompts', 'none', '--no-session-persistence', '--no-chrome', '--output-format', 'stream-json', '--verbose'],
  };
  assert.equal(host, 'codex');
  const mcp = `mcp_servers={devlink={command=${JSON.stringify(nodePath)},args=${JSON.stringify(serverArgs)},env={DEVLINK_BROKER_FILE=${JSON.stringify(credentialPath)}},enabled_tools=${JSON.stringify(hostTools)},required=true,default_tools_approval_mode="approve",startup_timeout_sec=20,tool_timeout_sec=10}}`;
  return { args: ['--no-daemon', '-a', 'never', 'exec', '--ignore-user-config', '--ephemeral', '--skip-git-repo-check', '--sandbox', 'read-only', '--json', '--cd', directory,
    '-c', mcp, '-c', 'mcp_optional_startup_grace_ms=0'] };
}
