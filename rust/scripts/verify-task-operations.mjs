// Story -94: actual local HTTP -> Rust -> isolated Spring/PostgreSQL.
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
const temp = await mkdtemp(join(tmpdir(), 'devlink-relay94-'));
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
    const email = `relay94-${randomUUID()}@example.test`, password = `Test-${randomUUID()}`; secrets.push(password);
    await this.api('/api/v1/auth/signup', { email, password, name: 'Relay94 synthetic' });
    this.identity = await this.api('/api/v1/auth/login', { email, password });
    this.csrf = await this.api('/api/v1/auth/csrf');
    return this;
  }
  async agent(team) {
    const agent = await this.api(`/api/v1/agents/teams/${team}`, { name: 'Existing Agent', toolType: 'CODEX', projectName: 'Relay94' });
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
  const team = await a.api('/api/v1/teams', { name: 'Relay94 synthetic team' });
  await b.api('/api/v1/teams/join', { inviteCode: team.inviteCode });
  const otherTeam = await outsider.api('/api/v1/teams', { name: 'Relay94 other team' });
  const agentA = await a.agent(team.teamId), agentB = await b.agent(team.teamId), agentC = await outsider.agent(otherTeam.teamId);
  const brokerA = await start(agentA, 'a'), brokerB = await start(agentB, 'b'), brokerC = await start(agentC, 'c');
  const claim = async (broker, agent) => (await local(broker, '/v1/sessions', {
    requestId: randomUUID(), instanceId: 'relay94', mode: 'fresh', agentId: String(agent.agentId)
  })).data;
  const sessionA = await claim(brokerA, agentA), sessionB = await claim(brokerB, agentB), sessionC = await claim(brokerC, agentC);
  await wait(() => sql(`SELECT count(*) FROM agents WHERE status='ONLINE'`) === '3', 'all brokers not ONLINE');
  const call = (broker, session, path, operation, args, code) => local(broker, path, business(session, operation, args), code);
  const result = async (broker, session, taskId) => (await call(broker, session, '/v1/tasks/result', 'get_task_result', { taskId })).data;
  const create = async prompt => {
    const r = await call(brokerA, sessionA, '/v1/tasks/create', 'create_task', { receiverAgentId: String(agentB.agentId), prompt });
    assert(r.requestKey); const taskId = r.data.taskId;
    await wait(() => sql(`SELECT count(*) FROM task_events WHERE task_id=${taskId} AND event_type='ACK_RECEIVED'`) === '1', 'ACK not recorded');
    return { taskId, requestKey: r.requestKey };
  };
  const task = await create('  exact prompt\n한글  ');
  let detail = await result(brokerB, sessionB, task.taskId);
  assert.equal(detail.status, 'PENDING'); assert.equal(detail.prompt, '  exact prompt\n한글  ');
  const acceptArgs = { ...task, version: detail.version, status: 'IN_PROGRESS' };
  await call(brokerA, sessionA, '/v1/tasks/status', 'update_task_status', acceptArgs, 'FORBIDDEN');
  await call(brokerB, sessionB, '/v1/tasks/complete', 'complete_task', { ...task, version: detail.version, resultText: 'too early' }, 'TASK_STATE_CONFLICT');
  const accepted = await call(brokerB, sessionB, '/v1/tasks/status', 'update_task_status', acceptArgs);
  assert.equal(accepted.requestKey, task.requestKey); assert.equal(accepted.data.status, 'IN_PROGRESS');
  await call(brokerB, sessionB, '/v1/tasks/status', 'update_task_status', acceptArgs, 'TASK_VERSION_CONFLICT');
  detail = await result(brokerA, sessionA, task.taskId);
  assert.equal(detail.version, accepted.data.version);
  const files = [
    { relativePath: 'src/없는파일.rs', changeType: 'MODIFIED', diffText: '- old\n+ 새 값\n' },
    { relativePath: 'renamed.rs', changeType: 'RENAMED', diffText: null }
  ];
  const resultText = '  Summary\nVerification: cargo test passed\n검증 원문  ';
  const completeArgs = { ...task, version: detail.version, resultText, taskFiles: files };
  await call(brokerA, sessionA, '/v1/tasks/complete', 'complete_task', completeArgs, 'FORBIDDEN');
  const completed = await call(brokerB, sessionB, '/v1/tasks/complete', 'complete_task', completeArgs);
  assert.equal(completed.data.status, 'COMPLETED'); assert.equal(completed.requestKey, task.requestKey);
  for (const [broker, session] of [[brokerA, sessionA], [brokerB, sessionB]]) {
    const saved = await result(broker, session, task.taskId);
    assert.equal(saved.resultText, resultText); assert.equal(saved.version, completed.data.version);
    assert.equal(saved.status, 'COMPLETED'); assert(saved.completedAt && saved.startedAt);
    assert.deepEqual(saved.taskFiles.map(({ fileId, ...file }) => { assert(/^\d+$/.test(fileId)); return file; }), files);
    assert(saved.events.some(event => event.eventType === 'STATUS_CHANGED'));
  }
  await call(brokerC, sessionC, '/v1/tasks/result', 'get_task_result', task, 'FORBIDDEN');
  await call(brokerB, sessionB, '/v1/tasks/complete', 'complete_task', { ...completeArgs, version: completed.data.version }, 'TASK_STATE_CONFLICT');
  assert.equal(sql(`SELECT count(*) FROM task_files WHERE task_id=${task.taskId}`), '2');
  assert.equal(sql(`SELECT count(*) FROM task_events WHERE task_id=${task.taskId} AND event_type='STATUS_CHANGED'`), '2');
  assert.equal(sql(`SELECT count(*) FROM outbox_events WHERE aggregate_id='${task.taskId}' AND event_type IN ('TASK_STATUS_CHANGED','TASK_COMPLETED')`), '2');
  assert.equal(sql(`SELECT encode(convert_to(result_text,'UTF8'),'hex') FROM tasks WHERE id=${task.taskId}`), Buffer.from(resultText).toString('hex'));

  const failed = await create('failure probe');
  detail = await result(brokerB, sessionB, failed.taskId);
  const inProgress = await call(brokerB, sessionB, '/v1/tasks/status', 'update_task_status', { ...failed, version: detail.version, status: 'IN_PROGRESS' });
  const reason = '  build failure\n일부 수정됨  ';
  const failure = await call(brokerB, sessionB, '/v1/tasks/status', 'update_task_status', {
    ...failed, version: inProgress.data.version, status: 'FAILED', failureReason: reason, partiallyModified: true
  });
  assert.equal(failure.data.failureReason, reason); assert.equal(failure.data.partiallyModified, true);
  detail = await result(brokerA, sessionA, failed.taskId);
  assert.equal(detail.failureReason, reason); assert.equal(detail.partiallyModified, true);
  assert.equal(detail.resultText, null); assert.equal(detail.status, 'FAILED');
  await call(brokerB, sessionB, '/v1/tasks/status', 'update_task_status', {
    ...failed, version: detail.version, status: 'IN_PROGRESS'
  }, 'TASK_STATE_CONFLICT');
  const nonexistent = { taskId: '9223372036854775807' };
  await call(brokerA, sessionA, '/v1/tasks/result', 'get_task_result', nonexistent, 'RESOURCE_NOT_FOUND');
  for (const [broker, session, direction] of [[brokerA, sessionA, 'sent'], [brokerB, sessionB, 'received']]) {
    const first = (await call(broker, session, '/v1/tasks/list', 'get_tasks', { direction, limit: 1 })).data;
    assert.equal(first.totalElements, 2); assert(first.nextCursor); assert.equal(first.items.length, 1);
    const second = (await call(broker, session, '/v1/tasks/list', 'get_tasks', { direction, limit: 1, cursor: first.nextCursor })).data;
    assert.equal(second.nextCursor, null); assert.notEqual(first.items[0].taskId, second.items[0].taskId);
    const filtered = (await call(broker, session, '/v1/tasks/list', 'get_tasks', { direction, status: 'FAILED' })).data;
    assert.equal(filtered.totalElements, 1); assert.equal(filtered.items[0].taskId, failed.taskId);
  }
  const spoof = business(sessionA, 'get_tasks', {}); spoof.context.teamId = String(otherTeam.teamId);
  await local(brokerA, '/v1/tasks/list', spoof, 'FORBIDDEN');
  assert(children.every(child => secrets.every(secret => !child.errors().includes(secret))), 'secret in stderr');
  report = { passed: true, actualRustHttp: true, actualCentral: true, actualPostgres: true,
    centralCommit: process.env.DEVLINK_TEST_CENTRAL_COMMIT,
    checks: ['receiver ownership', 'accept', 'stale version conflict', 'state conflict',
      'completion summary/files/diffs and DB rows', 'repeat completion rejected without duplicate files/events',
      'status/completion outbox rows', 'FAILED reason and partiallyModified',
      'sent/received/filter/pagination', 'task detail/result for both users', 'cross-team denial',
      'missing task', 'spoofed context denial', 'known requestKey', 'no secret output'],
    limits: ['synthetic accounts and isolated DB', 'DDL auto update; migrations and rollback injection not tested',
      'verification text inside resultText; no dedicated structured verification DTO', 'no MCP/Host calls',
      '-67/-71/-68 policy probes belong to story93 and were not rerun'] };
} finally {
  for (const { child } of children) if (child.exitCode === null) {
    child.kill(); await Promise.race([new Promise(r => child.once('exit', r)), pause(5000)]);
  }
  assert.equal(dirname(resolve(temp)), resolve(tmpdir())); await rm(temp, { recursive: true, force: true });
}
console.log(JSON.stringify(report));

