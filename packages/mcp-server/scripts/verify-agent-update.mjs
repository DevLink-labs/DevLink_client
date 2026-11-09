// Opt-in Jira -146 acceptance: real Hosts -> MCP -> Rust -> loopback Spring/PostgreSQL.
import assert from 'node:assert/strict';
import { randomUUID, createHash } from 'node:crypto';
import { spawn, execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { mkdir, readFile, writeFile, realpath } from 'node:fs/promises';
import { join, resolve, isAbsolute } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
import { captureHost } from '../test/helpers/host-process.mjs';
import { readHostTranscript } from '../test/helpers/host-transcript.mjs';

const base = new URL(process.env.DEVLINK_TEST_CENTRAL_URL ?? '');
assert(base.protocol === 'http:' && ['127.0.0.1', 'localhost', '[::1]'].includes(base.hostname)
  && base.pathname === '/' && !base.search && !base.hash && !base.username && !base.password,
  'Use an isolated loopback Spring/PostgreSQL server');
const cli = process.env.DEVLINK_TEST_CLI, entry = process.env.DEVLINK_TEST_MCP_ENTRY;
const artifactRoot = process.env.DEVLINK_TEST_HOST_ARTIFACTS;
assert([cli, entry, artifactRoot].every(p => p && isAbsolute(p)), 'Explicit absolute CLI, MCP entry and private artifact root required');
const serverCommit = process.env.DEVLINK_TEST_CENTRAL_COMMIT;
assert.match(serverCommit ?? '', /^[a-f0-9]{40}$/);
const root = join(artifactRoot, 'agent-update-' + randomUUID());
await mkdir(root, { recursive: true });
const env = Object.fromEntries(Object.entries(process.env).filter(([key]) => !/^DEVLINK_/i.test(key)));
const profiles = [], secrets = [], hosts = [], markers = [];
const tools = ['register_agent', 'get_my_context', 'update_agent'];
const hash = data => createHash('sha256').update(data).digest('hex');
const report = { serverCommit, cliSha256: hash(await readFile(cli)), mcpEntrySha256: hash(await readFile(entry)),
  actualLlmHostsExecuted: false, ownedBrokersStopped: false, hosts };
function safe(text) { assert(secrets.every(secret => !secret || !text.includes(secret)), 'Secret in shareable output'); }
async function raw(p, args, input) {
  const captured = await new Promise((accept, reject) => {
    const child = spawn(cli, ['--config', p.config, ...args], { cwd: p.folder, env, windowsHide: true, stdio: ['pipe', 'pipe', 'pipe'] });
    let stdout = '', stderr = '';
    const timeout = setTimeout(() => child.kill(), 30000);
    child.stdout.on('data', chunk => { stdout += chunk; });
    child.stderr.on('data', chunk => { stderr += chunk; });
    child.on('error', error => { clearTimeout(timeout); reject(error); });
    child.on('close', code => { clearTimeout(timeout); accept({ code, stdout, stderr }); });
    child.stdin.on('error', () => {});
    child.stdin.end(input ?? '');
  });
  safe(captured.stdout + captured.stderr);
  assert.equal(captured.code, 0, 'CLI failed: ' + args.slice(0, 2).join(' '));
  return captured.stdout;
}
async function run(p, args, input) { return JSON.parse(await raw(p, args, input)); }
async function state(p, file) { return JSON.parse(await readFile(join(p.folder, '.devlink-runtime', file), 'utf8')); }
async function until(fn, predicate) {
  const deadline = Date.now() + 20000;
  while (Date.now() < deadline) { const value = await fn(); if (predicate(value)) return value; await delay(100); }
  throw Error('Broker readiness deadline');
}
async function createUser(host) {
  const p = { host, folder: join(root, host), name: 'Agent update ' + host };
  p.config = join(p.folder, 'config.json'); profiles.push(p);
  await mkdir(p.folder);
  const email = 'agent146-' + randomUUID() + '@example.test', password = 'Test-' + randomUUID();
  secrets.push(password);
  const response = await fetch(new URL('/api/v1/auth/csrf', base), { signal: AbortSignal.timeout(5000) });
  assert(response.ok);
  const csrf = (await response.json()).data;
  const cookie = response.headers.getSetCookie().find(v => v.startsWith('JSESSIONID=')).split(';')[0];
  secrets.push(cookie, csrf.token);
  const signup = await fetch(new URL('/api/v1/auth/signup', base), { method: 'POST',
    headers: { Cookie: cookie, 'Content-Type': 'application/json', [csrf.headerName]: csrf.token },
    body: JSON.stringify({ email, password, name: p.name }), signal: AbortSignal.timeout(5000) });
  assert.equal(signup.status, 201);
  await raw(p, ['init']);
  await raw(p, ['config', 'set', 'serverUrl', base.toString().replace(/^http:/, 'ws:')]);
  await raw(p, ['config', 'set', 'localPort', '0']);
  p.user = (await run(p, ['auth', 'login', '--email', email, '--password-stdin'], password + '\n')).user;
  secrets.push((await state(p, 'user-session.json')).state.cookie);
  return p;
}
async function userGet(p, path) {
  const session = await state(p, 'user-session.json');
  const response = await fetch(new URL(path, base), { headers: { Cookie: 'JSESSIONID=' + session.state.cookie }, signal: AbortSignal.timeout(5000) });
  assert(response.ok, 'Independent central read failed');
  const body = await response.json(); assert(body.success); return body.data;
}
async function ownedProcesses(marker, kill = false) {
  if (process.platform !== 'win32') return [];
  assert.match(marker, /^devlink-agent146-[a-f0-9-]{36}$/);
  const script = "$items=@(Get-CimInstance Win32_Process -Filter \"Name = 'node.exe'\" | Where-Object {$_.CommandLine -and $_.CommandLine.Contains($env:DEVLINK_HOST_MARKER)}); if($env:DEVLINK_HOST_KILL -eq '1'){foreach($item in $items){Stop-Process -Id $item.ProcessId -Force -ErrorAction SilentlyContinue}}; ConvertTo-Json -Compress -InputObject @($items | ForEach-Object {$_.ProcessId})";
  const result = await promisify(execFile)('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', script], {
    env: { ...env, DEVLINK_HOST_MARKER: marker, DEVLINK_HOST_KILL: kill ? '1' : '0' }, windowsHide: true, timeout: 10000 });
  return JSON.parse(result.stdout || '[]');
}
async function hostRun(p) {
  const marker = 'devlink-agent146-' + randomUUID(); markers.push(marker);
  const args = [`--title=${marker}`, entry, '--transport', 'stdio', '--request-timeout-ms', '10000', '--call-timeout-ms', '30000'];
  const credentialPath = join(p.folder, '.devlink-runtime', 'broker.json');
  const projectName = 'Verified project ' + p.host, roleDescription = 'Verified MCP role ' + p.host;
  const prompt = `The operator explicitly authorizes changing only the current DevLink Agent to projectName=${JSON.stringify(projectName)}, roleDescription=${JSON.stringify(roleDescription)} on this isolated local acceptance server. Use only DevLink MCP tools, all calls serially. Do not read/edit files, execute shell commands, delegate, switch identity, create tasks, or log in. Call register_agent({}), get_my_context({}), update_agent with exactly those two values, get_my_context({}), then update_agent with the same values once to verify no-op. Exactly five calls. On any error stop and report it; do not repair or retry. Finish with a short result.`;
  let hostArgs;
  if (p.host === 'claude') {
    const config = join(p.folder, 'mcp.json');
    await writeFile(config, JSON.stringify({ mcpServers: { devlink: { command: process.execPath, args, env: { DEVLINK_BROKER_FILE: credentialPath } } } }));
    hostArgs = ['-p', '--model', 'claude-opus-5-5', '--effort', 'high', '--restricted', '--strict-mcp-config', '--mcp-config', config,
      '--tools', '', '--allowedTools', tools.map(t => 'mcp__devlink__' + t).join(','), '--permission-mode', 'dontAsk', '--permission-prompts', 'none',
      '--disable-slash-commands', '--no-session-persistence', '--no-chrome', '--output-format', 'stream-json', '--verbose', prompt];
  } else {
    const config = `mcp_servers={devlink={command=${JSON.stringify(process.execPath)},args=${JSON.stringify(args)},env={DEVLINK_BROKER_FILE=${JSON.stringify(credentialPath)}},enabled_tools=${JSON.stringify(tools)},required=true,default_tools_approval_mode="approve",startup_timeout_sec=30,tool_timeout_sec=40}}`;
    hostArgs = ['--no-daemon', '-a', 'never', 'exec', '--ignore-user-config', '--ephemeral', '--skip-git-repo-check', '--sandbox', 'read-only',
      '--json', '--cd', p.folder, '-c', config, '-c', 'mcp_optional_startup_grace_ms=0', prompt];
  }
  await writeFile(join(root, p.host + '.prompt.txt'), prompt);
  const result = await captureHost(p.host === 'claude' ? (process.env.DEVLINK_TEST_CLAUDE ?? 'claude.exe')
    : (process.env.DEVLINK_TEST_CODEX ?? 'codex.exe'), hostArgs, { cwd: p.folder, env, timeoutMs: 180000 });
  safe(result.stdout + result.stderr);
  await writeFile(join(root, p.host + '.jsonl'), result.stdout);
  await writeFile(join(root, p.host + '.stderr.txt'), result.stderr);
  assert.equal(result.code, 0, 'Real Host failed: ' + p.host);
  assert(!result.timedOut && !result.overflow && !result.interrupted && !result.spawnError);
  const proof = readHostTranscript(p.host, result.stdout);
  if (p.host === 'claude') assert.equal(proof.model, 'claude-opus-5-5');
  assert.deepEqual(proof.calls.map(c => c.tool), ['register_agent', 'get_my_context', 'update_agent', 'get_my_context', 'update_agent']);
  assert(proof.calls.every(c => c.completed && !c.hostError && !c.inputError && c.value?.data && !c.value.error));
  const changed = proof.calls[2], context = proof.calls[3].value.data, noop = proof.calls[4];
  for (const call of [changed, noop]) assert.deepEqual(call.args, { projectName, roleDescription });
  assert.equal(changed.value.data.updated, true); assert.equal(changed.value.data.credentialsRotated, false);
  assert.equal(changed.value.data.localProfileSynced, true);
  assert.equal(noop.value.data.updated, false); assert.deepEqual(noop.value.data.changedFields, []);
  assert.equal(context.principal.agentId, String(p.agent.agentId));
  assert.equal(context.agent.projectName, projectName); assert.equal(context.agent.roleDescription, roleDescription);
  const central = await userGet(p, '/api/v1/agents/' + p.agent.agentId);
  assert.equal(central.projectName, projectName); assert.equal(central.roleDescription, roleDescription);
  assert.equal(central.name, p.name); assert.equal(central.ownerUserId, p.user.userId);
  const after = await state(p, 'agent-profile.json');
  assert.equal(after.state.token, p.token);
  assert.equal(await realpath(after.projectRoot), await realpath(p.folder));
  const publicProof = { host: p.host, model: proof.model, agentId: p.agent.agentId, changed: true, contextMatches: true,
    noop: true, sameToken: true, independentCentralRead: true, transcriptSha256: hash(result.stdout),
    correlations: proof.calls.map(c => ({ tool: c.tool, requestId: c.value.requestId, mcpRequestId: c.value.mcpRequestId })) };
  safe(JSON.stringify(publicProof)); hosts.push(publicProof);
  console.log(JSON.stringify({ host: p.host, passed: true }));
}
try {
  const a = await createUser('codex'), b = await createUser('claude');
  const made = await run(a, ['team', 'create', '--name', 'Isolated agent146']);
  secrets.push(made.inviteCode);
  await run(a, ['team', 'select', '--id', String(made.teamId)]);
  await run(b, ['team', 'join', '--code-stdin'], made.inviteCode + '\n');
  await run(b, ['team', 'select', '--id', String(made.teamId)]);
  for (const p of profiles) {
    p.agent = (await run(p, ['agent', 'create', '--name', p.name, '--tool', p.host === 'codex' ? 'codex' : 'claude-code',
      '--project-name', 'Original ' + p.host, '--project', p.folder])).agent;
    p.token = (await state(p, 'agent-profile.json')).state.token; secrets.push(p.token);
    p.started = true; await raw(p, ['start']);
    const broker = await state(p, 'broker.json'); secrets.push(broker.localToken);
    const runtime = await state(p, 'runtime.json'); secrets.push(runtime.controlToken);
    await until(async () => (await fetch(broker.brokerUrl + '/health', { headers: { Authorization: 'Bearer ' + broker.localToken }, signal: AbortSignal.timeout(5000) })).json(), v => v.centralServer === 'Connected');
  }
  await hostRun(a);
  const beforeB = await userGet(b, '/api/v1/agents/' + b.agent.agentId);
  assert.equal(beforeB.projectName, 'Original claude');
  await hostRun(b);
  const afterA = await userGet(a, '/api/v1/agents/' + a.agent.agentId);
  assert.equal(afterA.projectName, 'Verified project codex');
  const agents = await userGet(a, '/api/v1/agents/teams/' + made.teamId);
  assert.equal(agents.length, 2, 'No new Agent may be created by an update');
  report.noNewAgent = true; report.counterpartUnchanged = true; report.actualLlmHostsExecuted = true;
  report.passed = true;
} catch (error) {
  report.passed = false; report.failure = String(error.message); safe(report.failure);
  process.exitCode = 1;
} finally {
  const stopped = [];
  for (const p of profiles) if (p.started) {
    try { await raw(p, ['stop']); stopped.push(true); } catch { stopped.push(false); }
  }
  report.ownedBrokersStopped = stopped.every(Boolean);
  report.ownedMcpProcessesStopped = true;
  for (const marker of markers) {
    let remaining = await ownedProcesses(marker);
    for (let tries = 0; remaining.length && tries < 10; tries++) { await delay(200); remaining = await ownedProcesses(marker); }
    if (remaining.length) { report.ownedMcpProcessesStopped = false; await ownedProcesses(marker, true); }
  }
  if (!report.ownedBrokersStopped || !report.ownedMcpProcessesStopped) { report.passed = false; process.exitCode = 1; }
  safe(JSON.stringify(report));
  await writeFile(join(root, 'report.json'), JSON.stringify(report, null, 2));
  console.log(JSON.stringify({ report: resolve(root, 'report.json'), passed: report.passed }));
}
