// Story -93 only: actual local HTTP -> Rust -> isolated Spring/PostgreSQL.
// No MCP DTO dependency, no Host execution, no automatic business retries.
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { randomUUID } from 'node:crypto';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { dirname, isAbsolute, join, resolve } from 'node:path';

const base = new URL(process.env.DEVLINK_TEST_CENTRAL_URL);
assert(base.protocol === 'http:' && base.hostname === '127.0.0.1' && base.port, 'isolated loopback central required');
const cli = process.env.DEVLINK_TEST_CLI;
const psql = process.env.DEVLINK_TEST_PSQL;
assert(isAbsolute(cli ?? '') && isAbsolute(psql ?? ''));
const dbPort = process.env.DEVLINK_TEST_DB_PORT;
assert(/^\d+$/.test(dbPort ?? ''));
const temp = await mkdtemp(join(tmpdir(), 'devlink-relay93-'));
const brokers = [], secrets = [], children = [];
const env = { ...process.env }; delete env.DEVLINK_TOKEN;
const pause = ms => new Promise(r => setTimeout(r, ms));
function sql(query) {
  const r = spawnSync(psql, ['-X', '-At', '-h', '127.0.0.1', '-p', dbPort,
    '-U', 'devlink_relay_test', '-d', 'devlink_relay_test', '-c', query],
  { encoding: 'utf8', windowsHide: true, timeout: 5000 });
  assert.equal(r.status, 0, 'isolated SQL failed'); return r.stdout.trim();
}
async function wait(predicate, label, ms = 12000) {
  const end = Date.now() + ms;
  while (Date.now() < end) { if (await predicate()) return; await pause(100); }
  assert.fail(label);
}
class User {
  cookie = ''; csrf;
  async api(path, body) {
    const headers = { cookie: this.cookie };
    if (body) { headers['content-type'] = 'application/json'; headers[this.csrf.headerName] = this.csrf.token; }
    const r = await fetch(new URL(path, base), { method: body ? 'POST' : 'GET', headers,
      body: body ? JSON.stringify(body) : undefined, redirect: 'manual', signal: AbortSignal.timeout(10000) });
    const cookies = r.headers.getSetCookie();
    if (cookies.length) { this.cookie = cookies.map(c => c.split(';')[0]).join('; '); secrets.push(this.cookie); }
    const value = await r.json(); assert(r.ok && value.success, `setup API ${path}: ${value.code}`);
    return value.data;
  }
  async signup() {
    this.csrf = await this.api('/api/v1/auth/csrf');
    const email = `relay93-${randomUUID()}@example.test`, password = `Test-${randomUUID()}`; secrets.push(password);
    await this.api('/api/v1/auth/signup', { email, password, name: 'Relay93 synthetic' });
    this.identity = await this.api('/api/v1/auth/login', { email, password });
    this.csrf = await this.api('/api/v1/auth/csrf');
    return this;
  }
  async agent(team) {
    const agent = await this.api(`/api/v1/agents/teams/${team}`, { name: 'Existing Agent', toolType: 'CODEX', projectName: 'Relay93' });
    const credential = await this.api(`/api/v1/agents/${agent.agentId}/token`, {}); secrets.push(credential.token);
    return { ...agent, token: credential.token };
  }
}
async function start(agent, label) {
  const config = join(temp, label, 'config.json');
  const run = args => {
    const r = spawnSync(cli, ['--config', config, ...args], { env, encoding: 'utf8', windowsHide: true, timeout: 20000 });
    assert.equal(r.status, 0, `CLI ${args[0]} failed`);
  };
  run(['init']); run(['config', 'set', 'serverUrl', base.href.replace(/^http:/, 'ws:')]);
  run(['config', 'set', 'localPort', '0']);
  const child = spawn(cli, ['--config', config, 'broker'], { env: { ...env, DEVLINK_TOKEN: agent.token },
    windowsHide: true, stdio: ['ignore', 'ignore', 'pipe'] });
  let errors = ''; child.stderr.on('data', b => { errors += b; }); children.push({ child, errors: () => errors });
  const broker = { config, child, directory: dirname(config) };
  await wait(async () => {
    try {
      Object.assign(broker, JSON.parse(await readFile(join(broker.directory, '.devlink-runtime', 'broker.json'), 'utf8')));
      const r = await fetch(`${broker.brokerUrl}/health`, { headers: { authorization: `Bearer ${broker.localToken}` } });
      return (await r.json()).centralServer === 'Connected';
    } catch { return false; }
  }, 'actual Rust broker did not connect');
  secrets.push(broker.localToken); brokers.push(broker); return broker;
}
async function local(broker, path, body, code) {
  const started = Date.now();
  const r = await fetch(`${broker.brokerUrl}${path}`, { method: 'POST',
    headers: { authorization: `Bearer ${broker.localToken}`, 'content-type': 'application/json' },
    body: JSON.stringify(body), signal: AbortSignal.timeout(10000) });
  const result = await r.json(); assert.equal(result.requestId, body.requestId);
  assert(Date.now() - started < 9500, 'local response exceeded caller budget');
  assert(secrets.every(secret => !JSON.stringify(result).includes(secret)), 'secret response');
  if (code) { assert.equal(result.error?.code, code); assert(!r.ok); }
  else assert(r.ok, `local ${path}: ${result.error?.code}`);
  return result;
}
function business(session, operation, args, requestId = randomUUID()) {
  return { requestId, operation, arguments: args, context: { ...session.principal, sessionId: session.sessionId } };
}
let report;
try {
  const a = await new User().signup(), b = await new User().signup(), outsider = await new User().signup();
  const team = await a.api('/api/v1/teams', { name: 'Relay93 team' });
  await b.api('/api/v1/teams/join', { inviteCode: team.inviteCode });
  const otherTeam = await outsider.api('/api/v1/teams', { name: 'Other team' });
  const agentA = await a.agent(team.teamId), agentB = await b.agent(team.teamId), other = await outsider.agent(otherTeam.teamId);
  const brokerA = await start(agentA, 'a');
  const unbound = (await local(brokerA, '/v1/sessions', { requestId: randomUUID(), instanceId: 'relay-a', mode: 'fresh' })).data;
  const agentsBefore = sql('SELECT count(*) FROM agents');
  const confirm = business(unbound, 'register_agent', { name: 'Existing Agent', toolType: 'codex' });
  assert.equal((await local(brokerA, '/v1/agents/register', confirm)).data.agentId, String(agentA.agentId));
  await local(brokerA, '/v1/agents/register', confirm);
  assert.equal(sql('SELECT count(*) FROM agents'), agentsBefore, 'registration duplicated an Agent');
  const sessionA = (await local(brokerA, `/v1/sessions/${unbound.sessionId}/bind`,
    { requestId: randomUUID(), instanceId: 'relay-a', agentId: String(agentA.agentId) })).data;
  const listed = (await local(brokerA, '/v1/agents/list', business(sessionA, 'list_agents', {}))).data.agents;
  assert.deepEqual(listed.map(agent => agent.agentId).sort(), [String(agentA.agentId), String(agentB.agentId)].sort());
  assert(!listed.some(agent => agent.agentId === String(other.agentId)));
  // Observe the provider's offline policy faithfully; do not patch it in Rust.
  const offlineResponse = await fetch(`${brokerA.brokerUrl}/v1/tasks/create`, { method: 'POST',
    headers: { authorization: `Bearer ${brokerA.localToken}`, 'content-type': 'application/json' },
    body: JSON.stringify(business(sessionA, 'create_task', { receiverAgentId: String(agentB.agentId), prompt: 'offline policy probe' })) });
  const offline = await offlineResponse.json();
  const offlinePreSaveRejection = !offlineResponse.ok && offline.error?.code === 'RECEIVER_OFFLINE';
  const blockers = offlinePreSaveRejection ? [] : ['-67/-71: offline receiver was persisted before rejection'];
  if (!offlinePreSaveRejection) {
    assert.equal(offline.data.delivery.status, 'DELIVERY_NOT_SENT');
    assert.equal(offline.data.delivery.reasonCode, 'RECEIVER_OFFLINE');
    assert.equal(sql(`SELECT count(*) FROM tasks WHERE id=${offline.data.taskId}`), '1');
  }
  const brokerB = await start(agentB, 'b');
  await local(brokerB, '/v1/sessions', { requestId: randomUUID(), instanceId: 'relay-b', mode: 'fresh', agentId: String(agentB.agentId) });
  await wait(() => sql(`SELECT status FROM agents WHERE id=${agentB.agentId}`) === 'ONLINE', 'B not ONLINE');
  const prompt = '  Actual Rust -> central\n한글 exact prompt  ';
  const create = business(sessionA, 'create_task', { receiverAgentId: String(agentB.agentId), prompt });
  const created = await local(brokerA, '/v1/tasks/create', create);
  const taskId = created.data.taskId; assert(/^\d+$/.test(taskId));
  assert(['ACK_PENDING', 'ACK_RECEIVED', 'DELIVERY_UNKNOWN'].includes(created.data.delivery.status));
  assert.equal(sql(`SELECT encode(convert_to(prompt,'UTF8'),'hex') FROM tasks WHERE id=${taskId}`), Buffer.from(prompt).toString('hex'));
  await wait(async () => {
    try { const inbox = JSON.parse(await readFile(join(brokerB.directory, '.devlink-runtime', 'inbox.json'), 'utf8'));
      return JSON.stringify(inbox).includes(prompt.replaceAll('\n', '\\n').replaceAll('"', '\\"')); } catch { return false; }
  }, 'B did not persist task');
  await wait(() => sql(`SELECT count(*) FROM task_events WHERE task_id=${taskId} AND event_type='ACK_RECEIVED'`) === '1', 'ACK not recorded');
  assert.equal(sql(`SELECT delivered_at IS NOT NULL FROM tasks WHERE id=${taskId}`), 't');
  const repeated = await local(brokerA, '/v1/tasks/create', create);
  assert.equal(repeated.requestKey, created.requestKey); assert.equal(repeated.data.taskId, taskId);
  assert.equal(repeated.data.replayed, true);
  const changed = structuredClone(create); changed.arguments.prompt = 'changed';
  const conflict = await local(brokerA, '/v1/tasks/create', changed, 'REQUEST_KEY_CONFLICT');
  assert.equal(conflict.requestKey, created.requestKey);
  assert.equal(sql(`SELECT count(*) FROM tasks WHERE id=${taskId}`), '1');
  // Reject through the authoritative provider, then explicitly replay the same
  // logical creation. This must return the existing REJECTED task, not recreate it.
  const detailResponse = await fetch(new URL(`/api/v1/tasks/${taskId}`, base), {
    headers: { authorization: `Bearer ${agentB.token}` }, signal: AbortSignal.timeout(10000) });
  assert.equal(detailResponse.status, 200);
  const detail = await detailResponse.json(); assert(detail.success);
  const rejectionResponse = await fetch(new URL(`/api/v1/tasks/${taskId}/status`, base), {
    method: 'PATCH', headers: { authorization: `Bearer ${agentB.token}`, 'content-type': 'application/json' },
    body: JSON.stringify({ status: 'REJECTED', version: detail.data.version, rejectionReason: 'Synthetic explicit rejection' }),
    signal: AbortSignal.timeout(10000) });
  assert.equal(rejectionResponse.status, 200);
  const rejection = await rejectionResponse.json(); assert(rejection.success);
  assert.equal(rejection.data.status, 'REJECTED');
  const rejectedReplay = await local(brokerA, '/v1/tasks/create', create);
  assert.equal(rejectedReplay.data.status, 'REJECTED');
  assert.equal(rejectedReplay.data.replayed, true);
  assert.equal(rejectedReplay.data.taskId, taskId);
  assert.equal(rejectedReplay.requestKey, created.requestKey);
  assert.equal(sql(`SELECT count(*) FROM tasks WHERE request_key='${created.requestKey}'`), '1');
  assert.equal(sql(`SELECT status FROM tasks WHERE id=${taskId}`), 'REJECTED');
  for (const receiver of [String(other.agentId), '9223372036854775807'])
    await local(brokerA, '/v1/tasks/create', business(sessionA, 'create_task', { receiverAgentId: receiver, prompt: 'denied' }), 'RESOURCE_NOT_FOUND');
  const spoof = business(sessionA, 'list_agents', {}); spoof.context.userId = '9223372036854775807';
  await local(brokerA, '/v1/agents/list', spoof, 'FORBIDDEN');
  await local(brokerA, '/v1/agents/register', business(sessionA, 'register_agent', { name: 'different' }), 'VALIDATION_FAILED');
  // Separate provider dependency probe (-68), deliberately not hidden inside
  // Rust recovery. Never correct a 409 by querying or sending a new key.
  const concurrentKey = randomUUID();
  const concurrent = await Promise.all([0,1].map(async () => {
    const r = await fetch(new URL(`/api/v1/tasks/${agentA.teamId}`, base), { method: 'POST',
      headers: { authorization: `Bearer ${agentA.token}`, 'content-type': 'application/json' },
      body: JSON.stringify({ requestKey: concurrentKey, receiverAgentId: agentB.agentId, prompt: 'provider concurrent-key probe' }),
      signal: AbortSignal.timeout(10000) });
    const value = await r.json(); return { status: r.status, code: value.code, taskId: value.data?.taskId };
  }));
  assert.equal(sql(`SELECT count(*) FROM tasks WHERE request_key='${concurrentKey}'`), '1');
  if (concurrent.some(result => result.status === 409)) blockers.push('-68: concurrent same-key request returned 409; no recovery inferred');
  assert(concurrent.every(result => [200,201,409].includes(result.status)));
  assert(children.every(child => secrets.every(secret => !child.errors().includes(secret))), 'secret in stderr');
  report = { passed: true, storyComplete: blockers.length === 0, actualRustHttp: true, actualCentral: true,
    actualPostgres: true, mcpRequired: false, offlinePreSaveRejection, concurrentProviderProbe: concurrent, blockers,
    checks: ['existing Agent confirmation without creation', 'authenticated team list', 'exact prompt',
      'actual task persistence', 'receiver disk persistence and ACK', 'explicit same-key replay', 'REJECTED same-key replay without recreation',
      'changed-payload conflict/key preservation', 'cross-team/missing receiver', 'spoofed principal', 'metadata mismatch', 'no secret output'] };
} finally {
  for (const { child } of children) if (child.exitCode === null) {
    child.kill(); await Promise.race([new Promise(r => child.once('exit', r)), pause(5000)]);
  }
  assert.equal(dirname(resolve(temp)), resolve(tmpdir())); await rm(temp, { recursive: true, force: true });
}
console.log(JSON.stringify(report));
