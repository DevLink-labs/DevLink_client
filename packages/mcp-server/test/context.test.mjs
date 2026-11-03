import assert from 'node:assert/strict';
import { mkdir, readFile, symlink, writeFile } from 'node:fs/promises';
import { dirname, join } from 'node:path';
import { PassThrough } from 'node:stream';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { createServer } from 'node:http';
import test from 'node:test';
import { setTimeout as delay } from 'node:timers/promises';
import { BrokerClient } from '../dist/broker/client.js';
import { credentialSchema, principalSchema, sessionSchema } from '../dist/broker/schemas.js';
import { readBrokerSettings, validateSettings } from '../dist/broker/settings.js';
import { createLogger } from '../dist/logger.js';
import { fakeBroker } from './helpers/fake-broker.mjs';
import { privateSettings } from './helpers/private-settings.mjs';

const code = (expected) => (error) => error.code === expected;
const gate = () => { let resolve; const promise = new Promise((r) => { resolve = r; }); return { promise, resolve }; };
async function fixture(t, settings = {}, options = {}) {
  const broker = await fakeBroker();
  const client = new BrokerClient({ settings: { ...broker.settings, ...settings }, heartbeatMs: 60_000, retryBaseMs: 60_000, ...options });
  t.after(async () => { await client.close(); await broker.close(); });
  return { broker, client };
}

test('shared wire fixtures distinguish principal, session and profile credentials', async () => {
  const fixtures = JSON.parse(await readFile(new URL('../../../contracts/local-broker/v1/identity-fixtures.json', import.meta.url), 'utf8'));
  const schemas = { principal: principalSchema, session: sessionSchema, credentials: credentialSchema };
  for (const fixture of fixtures) assert.equal(schemas[fixture.type].safeParse(fixture.value).success, fixture.valid, fixture.name);
  assert.equal(validateSettings({ schemaVersion: 1, brokerUrl: 'http://127.0.0.1:80/', localToken: 'example-token-123456' }).brokerUrl, 'http://127.0.0.1:80');
});

test('missing/malformed authenticated identity is terminal before any business request', async (t) => {
  for (const principal of [{ userId: 'u' }, { teamId: 't' }, { userId: ' ', teamId: 't' }, { userId: 'u', teamId: 't', token: 'private-detail' }, { userId: 'u', teamId: 't', toolType: 'fake-tool' }]) {
    const { broker, client } = await fixture(t);
    broker.setHook(({ path, send }) => {
      if (path !== '/v1/sessions') return false;
      send({ data: { sessionId: 'session', principal } }); return true;
    });
    await assert.rejects(client.execute('register_agent', { name: 'worker', toolType: 'codex' }), code('BROKER_CONTEXT_INVALID'));
    await assert.rejects(client.execute('get_tasks', {}), code('BROKER_CONTEXT_INVALID'));
    assert.equal(broker.calls.length, 1);
  }
});

test('even direct adapter callers cannot inject actor fields or mutate queued inputs', async (t) => {
  const { broker, client } = await fixture(t, { agentId: 'agent-a' });
  for (const key of ['userId', 'teamId', 'agentId', 'sessionId', 'principal', 'context']) {
    await assert.rejects(client.execute('get_tasks', { [key]: 'forged' }), code('CALLER_CONTEXT_OVERRIDE'));
  }
  assert.equal(broker.calls.length, 0);
  const args = { recipientAgentId: 'agent-b', description: 'original' };
  const options = { mcpRequestId: 7 };
  const result = client.execute('create_task', args, options);
  args.userId = 'forged-late'; args.description = 'changed-late'; options.mcpRequestId = 99;
  await result;
  const request = broker.calls.at(-1).body;
  assert.deepEqual(request.arguments, { recipientAgentId: 'agent-b', description: 'original' });
  assert.equal(request.mcpRequestId, 7);
  assert.equal(request.context.userId, 'trusted-user');
});

test('binding and re-registration forward verified tool metadata rather than tool arguments', async (t) => {
  const { broker, client } = await fixture(t);
  let verifiedType = 'claude_code';
  broker.setHook(({ path, body, send }) => {
    if (!path.endsWith('/bind')) return false;
    const session = broker.sessions.get(path.split('/')[3]);
    session.view.principal.agentId = body.agentId;
    session.view.principal.toolType = verifiedType;
    send({ data: session.view }); return true;
  });
  const registered = await client.execute('register_agent', { name: 'worker', toolType: 'codex' });
  registered.data.agentId = 'mutated-return-value';
  await client.execute('get_tasks', {});
  assert.equal(broker.calls.at(-1).body.context.toolType, 'claude_code');
  verifiedType = 'codex';
  await client.execute('register_agent', { name: 'worker', toolType: 'claude_code' });
  await client.execute('get_tasks', {});
  assert.equal(broker.calls.at(-1).body.context.toolType, 'codex');
  assert.equal(broker.calls.at(-1).body.context.agentId, 'registered-agent');
});

test('bind cannot switch user, team, Agent or local session', async (t) => {
  for (const field of ['userId', 'teamId', 'agentId', 'sessionId']) {
    const { broker, client } = await fixture(t);
    broker.setHook(({ path, body, send }) => {
      if (!path.endsWith('/bind')) return false;
      const data = structuredClone(broker.sessions.get(path.split('/')[3]).view);
      data.principal.agentId = body.agentId;
      if (field === 'sessionId') data.sessionId = 'different-session';
      else data.principal[field] = 'different-identity';
      send({ data }); return true;
    });
    await assert.rejects(client.execute('register_agent', { name: 'worker' }), code('BROKER_CONTEXT_INVALID'));
    const count = broker.calls.length;
    await assert.rejects(client.execute('get_tasks', {}), code('BROKER_CONTEXT_INVALID'));
    assert.equal(broker.calls.length, count);
  }
});

test('concurrent registration is serialized and business calls wait for the newest binding', async (t) => {
  const { broker, client } = await fixture(t, { agentId: 'agent-a' });
  const entered = gate(); const release = gate(); let bindings = 0;
  t.after(() => release.resolve());
  broker.setHook(async ({ path, send }) => {
    if (!path.endsWith('/bind')) return false;
    const serial = ++bindings;
    if (serial === 1) { entered.resolve(); await release.promise; }
    const session = broker.sessions.get(path.split('/')[3]);
    session.view.principal.toolType = serial === 1 ? 'codex' : 'claude_code';
    send({ data: session.view }); return true;
  });
  const first = client.execute('register_agent', { name: 'first' });
  const second = client.execute('register_agent', { name: 'second' });
  const business = client.execute('get_tasks', {});
  await entered.promise;
  assert.equal(broker.calls.filter((c) => c.body.operation === 'register_agent').length, 1);
  assert.equal(broker.calls.filter((c) => c.body.operation === 'get_tasks').length, 0);
  release.resolve();
  await Promise.all([first, second, business]);
  assert.equal(bindings, 2);
  assert.equal(broker.calls.at(-1).body.context.toolType, 'claude_code');
});

test('a queued business call can be cancelled without waiting for registration', { timeout: 5000 }, async (t) => {
  const { broker, client } = await fixture(t, { agentId: 'agent-a' });
  const entered = gate(); const release = gate();
  t.after(() => release.resolve());
  broker.setHook(async ({ path }) => {
    if (!path.endsWith('/bind')) return false;
    entered.resolve(); await release.promise; return false;
  });
  const registration = client.execute('register_agent', { name: 'worker' });
  await entered.promise;
  const controller = new AbortController();
  const cancelled = assert.rejects(client.execute('get_tasks', {}, { signal: controller.signal }), code('REQUEST_ABORTED'));
  controller.abort(); await cancelled;
  assert.equal(broker.calls.filter((call) => call.body.operation === 'get_tasks').length, 0);
  release.resolve(); await registration;
});

test('cancelled or timed-out bind can recover a still-live unbound lease without terminal authentication failure', { timeout: 5000 }, async (t) => {
  for (const mode of ['abort', 'timeout']) {
    const { broker, client } = await fixture(t, {}, { requestTimeoutMs: mode === 'timeout' ? 80 : 1000 });
    const entered = gate(); const release = gate();
    t.after(() => release.resolve());
    broker.setHook(async ({ path, send }) => {
      if (!path.endsWith('/bind')) return false;
      entered.resolve(); await release.promise;
      send({ data: broker.sessions.get(path.split('/')[3]).view }); return true;
    });
    const controller = new AbortController();
    const rejected = assert.rejects(client.execute('register_agent', { name: 'worker' }, { signal: controller.signal }), code(mode === 'abort' ? 'REQUEST_ABORTED' : 'BROKER_TIMEOUT'));
    await entered.promise;
    if (mode === 'abort') controller.abort();
    await rejected;
    broker.setHook(undefined); release.resolve();
    await assert.rejects(client.execute('get_tasks', {}), code('AGENT_UNREGISTERED'));
    assert.equal(client.state, 'unbound');
    const recovery = broker.calls.findLast((call) => call.path === '/v1/sessions');
    assert.equal(recovery.body.mode, 'recover');
    assert.equal(recovery.body.agentId, 'registered-agent');
    assert.equal(broker.calls.filter((call) => call.body.operation === 'register_agent').length, 1);
    await client.execute('register_agent', { name: 'retry-binding' });
    await client.execute('get_tasks', {});
    assert.equal(client.state, 'bound');
  }
});

test('a central registration ID survives lease loss before its response arrives', { timeout: 5000 }, async (t) => {
  const { broker, client } = await fixture(t);
  const entered = gate(); const release = gate();
  t.after(() => release.resolve());
  broker.setHook(async ({ body, send }) => {
    if (body.operation !== 'register_agent') return false;
    entered.resolve(); await release.promise;
    send({ data: { agentId: 'issued-before-lease-loss' } }); return true;
  });
  const rejected = assert.rejects(client.execute('register_agent', { name: 'worker' }), code('SESSION_CONTEXT_CHANGED'));
  await entered.promise;
  broker.advance(90_000);
  await assert.rejects(client.heartbeat(), code('SESSION_EXPIRED'));
  release.resolve(); await rejected;
  broker.setHook(undefined);
  await client.execute('get_tasks', {});
  assert.equal(broker.calls.findLast((call) => call.path === '/v1/sessions').body.agentId, 'issued-before-lease-loss');
  assert.equal(broker.calls.filter((call) => call.body.operation === 'register_agent').length, 1);
});

test('a shared failed session claim consumes one recovery attempt for concurrent callers', { timeout: 5000 }, async (t) => {
  const { broker, client } = await fixture(t, { agentId: 'agent-a' }, { retryBaseMs: 20, retryLimit: 1 });
  let claims = 0;
  broker.setHook(({ path, req }) => {
    if (path === '/v1/sessions' && ++claims === 1) { req.socket.destroy(); return true; }
    return false;
  });
  const outcomes = await Promise.allSettled(Array.from({ length: 16 }, () => client.execute('get_tasks', {})));
  assert.ok(outcomes.every((outcome) => outcome.status === 'rejected' && outcome.reason.code === 'BROKER_CONNECTION_CLOSED'));
  const deadline = Date.now() + 1000;
  while (client.state !== 'bound' && Date.now() < deadline) await delay(10);
  assert.equal(client.state, 'bound');
  assert.equal(claims, 2);
  assert.equal(broker.calls.filter((call) => call.body.operation === 'get_tasks').length, 0);
});

test('cancelling a queued registration returns immediately and never registers it later', { timeout: 5000 }, async (t) => {
  const { broker, client } = await fixture(t, { agentId: 'agent-a' });
  const entered = gate(); const release = gate();
  t.after(() => release.resolve());
  broker.setHook(async ({ path }) => {
    if (!path.endsWith('/bind')) return false;
    entered.resolve(); await release.promise; return false;
  });
  const first = client.execute('register_agent', { name: 'first' });
  await entered.promise;
  const controller = new AbortController();
  const second = assert.rejects(client.execute('register_agent', { name: 'cancelled' }, { signal: controller.signal }), code('REQUEST_ABORTED'));
  controller.abort(); await second;
  assert.equal(broker.calls.filter((call) => call.body.operation === 'register_agent').length, 1);
  release.resolve(); await first;
  await client.execute('get_tasks', {});
  assert.equal(broker.calls.filter((call) => call.body.operation === 'register_agent').length, 1);
});

test('a different Agent returned on explicit registration retry is never bound or used', { timeout: 5000 }, async (t) => {
  const { broker, client } = await fixture(t);
  const entered = gate(); const release = gate();
  t.after(() => release.resolve());
  broker.setHook(async ({ path, send }) => {
    if (!path.endsWith('/bind')) return false;
    entered.resolve(); await release.promise;
    send({ data: broker.sessions.get(path.split('/')[3]).view }); return true;
  });
  const controller = new AbortController();
  const first = assert.rejects(client.execute('register_agent', { name: 'worker' }, { signal: controller.signal }), code('REQUEST_ABORTED'));
  await entered.promise; controller.abort(); await first;
  broker.setHook(undefined); release.resolve();
  await assert.rejects(client.execute('get_tasks', {}), code('AGENT_UNREGISTERED'));
  broker.setHook(({ body, send }) => {
    if (body.operation !== 'register_agent') return false;
    send({ data: { agentId: 'different-new-agent' } }); return true;
  });
  await assert.rejects(client.execute('register_agent', { name: 'retry' }), code('BROKER_CONTEXT_INVALID'));
  assert.equal(client.state, 'BROKER_CONTEXT_INVALID');
  await assert.rejects(client.execute('get_tasks', {}), code('BROKER_CONTEXT_INVALID'));
  assert.equal(broker.calls.filter((call) => call.path.endsWith('/bind') && call.body.agentId === 'different-new-agent').length, 0);
  assert.equal(broker.calls.filter((call) => call.body.operation === 'get_tasks').length, 0);
});

test('a committed bind with an aborted response recovers automatically without replaying registration', { timeout: 5000 }, async (t) => {
  const { broker, client } = await fixture(t, {}, { retryBaseMs: 20 });
  const entered = gate(); const release = gate();
  t.after(() => release.resolve());
  broker.setHook(async ({ path, body, send }) => {
    if (!path.endsWith('/bind')) return false;
    const id = path.split('/')[3];
    const session = broker.sessions.get(id);
    session.view.principal.agentId = body.agentId;
    broker.owners.set(body.agentId, id);
    entered.resolve(); await release.promise;
    send({ data: session.view }); return true;
  });
  const controller = new AbortController();
  const first = assert.rejects(client.execute('register_agent', { name: 'worker' }, { signal: controller.signal }), code('REQUEST_ABORTED'));
  await entered.promise; controller.abort(); await first;
  const deadline = Date.now() + 1000;
  while ((broker.calls.filter((call) => call.path === '/v1/sessions').length < 2 || client.state !== 'bound') && Date.now() < deadline) await delay(10);
  assert.equal(client.state, 'bound');
  assert.equal(broker.calls.filter((call) => call.path === '/v1/sessions').length, 2);
  await client.execute('get_tasks', {});
  assert.equal(broker.calls.filter((call) => call.body.operation === 'register_agent').length, 1);
  release.resolve();
});

test('late errors from an older business call cannot revoke a refreshed context', async (t) => {
  const { broker, client } = await fixture(t, { agentId: 'agent-a' });
  await client.execute('get_tasks', {});
  const entered = gate(); const release = gate(); let delayed = false;
  t.after(() => release.resolve());
  broker.setHook(async ({ path, body, send }) => {
    if (body.operation !== 'get_tasks' || delayed) return false;
    delayed = true; entered.resolve(); await release.promise;
    send({ error: { code: 'SESSION_REPLACED' } }, 409); return true;
  });
  const oldCall = client.execute('get_tasks', {});
  const rejected = assert.rejects(oldCall, code('SESSION_REPLACED'));
  await entered.promise;
  await client.execute('register_agent', { name: 'refresh' });
  release.resolve(); await rejected;
  assert.equal(client.state, 'bound');
  await client.execute('get_tasks', {});
});

test('late heartbeat cannot invalidate a newly committed binding', async (t) => {
  const { broker, client } = await fixture(t, { agentId: 'agent-a' });
  await client.execute('get_tasks', {});
  const entered = gate(); const release = gate();
  t.after(() => release.resolve());
  broker.setHook(async ({ path, send }) => {
    if (!path.endsWith('/heartbeat')) return false;
    entered.resolve(); await release.promise; send({ error: { code: 'SESSION_EXPIRED' } }, 409); return true;
  });
  const heartbeat = assert.rejects(client.heartbeat(), code('SESSION_EXPIRED'));
  await entered.promise;
  await client.execute('register_agent', { name: 'refresh' });
  release.resolve(); await heartbeat;
  assert.equal(client.state, 'bound');
  await client.execute('get_tasks', {});
});

test('a bind received after lease loss cannot resurrect context; recovery retains the registered ID', async (t) => {
  const { broker, client } = await fixture(t);
  const entered = gate(); const release = gate();
  t.after(() => release.resolve());
  broker.setHook(async ({ path, body, send }) => {
    if (!path.endsWith('/bind')) return false;
    const session = structuredClone(broker.sessions.get(path.split('/')[3]).view);
    session.principal.agentId = body.agentId;
    entered.resolve(); await release.promise; send({ data: session }); return true;
  });
  const registration = assert.rejects(client.execute('register_agent', { name: 'worker' }), code('SESSION_CONTEXT_CHANGED'));
  await entered.promise;
  broker.advance(90_000);
  await assert.rejects(client.heartbeat(), code('SESSION_EXPIRED'));
  release.resolve(); await registration;
  assert.equal(client.state, 'inactive');
  await client.heartbeat();
  assert.equal(client.state, 'bound');
  assert.equal(broker.calls.filter((c) => c.body.operation === 'register_agent').length, 1);
  const recovery = broker.calls.findLast((c) => c.path === '/v1/sessions');
  assert.equal(recovery.body.agentId, 'registered-agent');
  assert.equal(recovery.body.mode, 'recover');
});

test('recovery rejects user/team changes or verified metadata loss', async (t) => {
  for (const field of ['userId', 'teamId', 'toolType']) {
    const { broker, client } = await fixture(t, { agentId: 'agent-a' });
    broker.setHook(({ path, send }) => {
      if (path !== '/v1/sessions') return false;
      send({ data: { sessionId: 'old-session', principal: { userId: 'u', teamId: 't', agentId: 'agent-a', toolType: 'codex' } } }); return true;
    });
    // Only establish identity here; the controlled heartbeat fails without a server-side lease.
    await assert.rejects(client.heartbeat(), code('SESSION_EXPIRED'));
    broker.setHook(({ path, send }) => {
      if (path !== '/v1/sessions') return false;
      const principal = { userId: 'u', teamId: 't', agentId: 'agent-a', toolType: 'codex' };
      if (field === 'toolType') delete principal.toolType; else principal[field] = 'switched';
      send({ data: { sessionId: 'recovered', principal } }); return true;
    });
    await assert.rejects(client.heartbeat(), code('BROKER_CONTEXT_INVALID'));
    assert.equal(client.state, 'BROKER_CONTEXT_INVALID');
  }
});

test('separate private profile files keep two Hosts identities isolated on the same broker', { timeout: 15000 }, async (t) => {
  const profiles = [
    { localToken: 'codex-profile-private-token', userId: 'user-a', teamId: 'team-a', agentId: 'agent-a', toolType: 'codex' },
    { localToken: 'claude-profile-private-token', userId: 'user-b', teamId: 'team-b', agentId: 'agent-b', toolType: 'claude_code' },
  ];
  const broker = await fakeBroker({ profiles });
  const aFile = await privateSettings({ ...broker.settings, agentId: 'agent-a' }, 'codex-main');
  const bPath = join(dirname(dirname(aFile.path)), 'claude-main', 'broker.json');
  await mkdir(dirname(bPath), { mode: 0o700 });
  await writeFile(bPath, JSON.stringify({ ...broker.settings, localToken: profiles[1].localToken, agentId: 'agent-b' }), { mode: 0o600 });
  const a = new BrokerClient({ settingsPath: aFile.path }); const b = new BrokerClient({ settingsPath: bPath });
  t.after(async () => { await Promise.all([a.close(), b.close()]); await broker.close(); await aFile.cleanup(); });
  await Promise.all([a.execute('get_tasks', {}), b.execute('get_tasks', {})]);
  const contexts = broker.calls.filter((c) => c.body.operation === 'get_tasks').map((c) => c.body.context);
  assert.deepEqual(new Set(contexts.map((c) => `${c.userId}/${c.teamId}/${c.agentId}/${c.toolType}`)), new Set(['user-a/team-a/agent-a/codex', 'user-b/team-b/agent-b/claude_code']));
  assert.equal(broker.owners.size, 2);
});

test('credential rotation never silently reloads a running client', { timeout: 15000 }, async (t) => {
  const { broker } = await fixture(t);
  const file = await privateSettings({ ...broker.settings, agentId: 'agent-a' }, 'codex-main');
  const old = new BrokerClient({ settingsPath: file.path });
  const fresh = new BrokerClient({ settingsPath: file.path });
  t.after(async () => { await Promise.all([old.close(), fresh.close()]); await file.cleanup(); });
  await old.execute('get_tasks', {});
  const newToken = 'rotated-profile-private-token';
  broker.revokeToken(broker.settings.localToken);
  broker.addProfile({ localToken: newToken, userId: 'trusted-user', teamId: 'trusted-team', agentId: 'agent-a', toolType: 'codex' });
  await writeFile(file.path, JSON.stringify({ ...broker.settings, localToken: newToken, agentId: 'agent-a' }));
  await assert.rejects(old.execute('get_tasks', {}), code('BROKER_UNAUTHORIZED'));
  const count = broker.calls.length;
  await assert.rejects(old.execute('get_tasks', {}), code('BROKER_UNAUTHORIZED'));
  assert.equal(broker.calls.length, count);
  broker.restart(); // Explicit rotation restarts the broker and its volatile ownership registry.
  await fresh.execute('get_tasks', {});
  assert.equal(fresh.state, 'bound');
});

test('management JSON, incomplete/oversized credentials and a linked profile directory are refused', { timeout: 15000 }, async (t) => {
  const file = await privateSettings({ schemaVersion: 1, brokerUrl: 'http://127.0.0.1:43127', localToken: 'private-local-token-1234' }, 'codex-main');
  t.after(() => file.cleanup());
  for (const value of ['{"schemaVersion":', JSON.stringify({ apiVersion: 1, pid: 123, controlToken: 'private-control-token' }), ' '.repeat(16_385)]) {
    await writeFile(file.path, value);
    await assert.rejects(readBrokerSettings(file.path), code('CREDENTIAL_FILE_INVALID'));
  }
  await writeFile(file.path, JSON.stringify({ schemaVersion: 1, brokerUrl: 'http://127.0.0.1:43127', localToken: 'private-local-token-1234' }));
  await readBrokerSettings(file.path); // Prove the link, rather than invalid contents, causes rejection.
  const link = join(dirname(dirname(file.path)), 'linked-profile');
  await symlink(dirname(file.path), link, process.platform === 'win32' ? 'junction' : 'dir');
  await assert.rejects(readBrokerSettings(join(link, 'broker.json')), code('CREDENTIAL_FILE_INVALID'));
});

test('ambient proxy configuration cannot receive local broker credentials', { timeout: 10000 }, async (t) => {
  const { broker } = await fixture(t);
  let proxyCalls = 0;
  const proxy = createServer((req, res) => { proxyCalls++; res.writeHead(502).end(); });
  proxy.listen(0, '127.0.0.1'); await once(proxy, 'listening');
  t.after(() => new Promise((resolve) => { proxy.closeAllConnections(); proxy.close(resolve); }));
  const script = `import { BrokerClient } from './dist/broker/client.js';
const client = new BrokerClient({ settings: JSON.parse(process.env.DEVLINK_TEST_SETTINGS) });
try { await client.execute('get_tasks', {}); } finally { await client.close(); }`;
  const proxyUrl = `http://127.0.0.1:${proxy.address().port}`;
  const child = spawn(process.execPath, ['--input-type=module', '-e', script], {
    cwd: new URL('../', import.meta.url), windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'],
    env: { ...process.env, NODE_USE_ENV_PROXY: '1', HTTP_PROXY: proxyUrl, http_proxy: proxyUrl,
      HTTPS_PROXY: proxyUrl, https_proxy: proxyUrl, NO_PROXY: '', no_proxy: '',
      DEVLINK_TEST_SETTINGS: JSON.stringify({ ...broker.settings, agentId: 'agent-a' }) },
  });
  t.after(() => { if (child.exitCode === null) child.kill(); });
  let error = ''; child.stderr.on('data', (chunk) => { error += chunk; }); child.stdout.resume();
  const [exitCode] = await once(child, 'exit');
  assert.equal(exitCode, 0, error);
  assert.equal(proxyCalls, 0);
  assert.ok(broker.calls.some((call) => call.body.operation === 'get_tasks'));
});

test('correlation logs contain call IDs and omit profile tokens, identity and payload', async (t) => {
  const output = new PassThrough(); let logs = '';
  output.on('data', (chunk) => { logs += chunk; });
  const logger = createLogger(output, 'debug');
  const { broker, client } = await fixture(t, {}, { logger });
  const result = await client.execute('register_agent', { name: 'sensitive-agent-name', toolType: 'codex' });
  await client.execute('create_task', { description: 'sensitive-task-payload' });
  await assert.rejects(client.execute('get_tasks', { userId: 'sensitive-forgery' }), code('CALLER_CONTEXT_OVERRIDE'));
  await client.close();
  logger.debug('broker.call_started', { requestId: result.requestId, operation: 'sensitive-task-payload', localToken: broker.settings.localToken });
  const records = logs.trim().split('\n').map(JSON.parse);
  assert.ok(records.some((r) => r.event === 'broker.call_started' && r.requestId === result.requestId));
  assert.ok(records.some((r) => r.event === 'broker.call_succeeded' && r.requestId === result.requestId));
  assert.ok(records.some((r) => r.event === 'broker.call_failed' && r.code === 'CALLER_CONTEXT_OVERRIDE'));
  for (const privateValue of [broker.settings.localToken, 'trusted-user', 'trusted-team', 'sensitive-agent-name', 'sensitive-task-payload', 'sensitive-forgery']) assert.ok(!logs.includes(privateValue));
  output.destroy();
});
