import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { once } from 'node:events';
import { spawnSync } from 'node:child_process';
import { symlink } from 'node:fs/promises';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { test } from 'node:test';
import { checkIntegration, inspectBroker, requiredRoutes } from '../scripts/integration-readiness.mjs';
import { privateDirectory, privateSettings } from './helpers/private-settings.mjs';

const token = 'SECRET_readiness_token_66';
const health = { service: 'devlink-broker', apiVersion: 1, pid: 1, instanceId: 'synthetic', centralServer: 'Connected' };
async function fixture(t, handler) {
  const calls = [];
  const server = createServer((req, res) => {
    calls.push({ method: req.method, path: req.url, auth: req.headers.authorization });
    handler(req, res);
  });
  server.listen(0, '127.0.0.1');
  await once(server, 'listening');
  t.after(() => { server.closeAllConnections(); return new Promise((resolve) => server.close(resolve)); });
  return { calls, settings: { schemaVersion: 1, brokerUrl: `http://127.0.0.1:${server.address().port}`, localToken: token } };
}
function respond(res, status, body = '') { res.writeHead(status); res.end(body); }

test('real credential reader + health-only Rust shape blocks all 11 routes without mutations or secret output', async (t) => {
  const f = await fixture(t, (req, res) => respond(res, req.url === '/health' ? 200 : 404,
    req.url === '/health' ? JSON.stringify({ ...health, centralServer: 'Disconnected',
      inbox: { rejectedEvents: 1, lastError: 'STORE_FAILED' }, secret: token }) : token));
  const file = await privateSettings(f.settings);
  t.after(file.cleanup);
  const result = await checkIntegration(file.path);
  assert.equal(result.status, 'blocked');
  assert.equal(result.actualMvpVerified, false);
  assert.equal(result.routes.length, 11);
  assert.ok(result.routes.every((r) => r.observation === 'NOT_FOUND'));
  assert.ok(result.blockers.includes('CENTRAL_NOT_CONNECTED'));
  assert.deepEqual(result.health.health, { centralServer: 'Disconnected' });
  assert.ok(!JSON.stringify(result).includes('STORE_FAILED'));
  assert.equal(f.calls[0].method, 'GET');
  assert.deepEqual(f.calls.slice(1).map((c) => c.path), requiredRoutes);
  assert.ok(f.calls.slice(1).every((c) => c.method === 'OPTIONS'));
  assert.ok(f.calls.every((c) => c.auth === `Bearer ${token}`));
  assert.ok(!JSON.stringify(result).includes(token));
  assert.ok(!JSON.stringify(result).includes(file.path));
});

test('generic OPTIONS success and 405 never certify real MVP or route implementation', async (t) => {
  for (const status of [204, 405]) {
    const f = await fixture(t, (req, res) => respond(res, req.url === '/health' ? 200 : status,
      req.url === '/health' ? JSON.stringify(health) : ''));
    const result = await inspectBroker(f.settings);
    assert.equal(result.status, 'unverified');
    assert.equal(result.actualMvpVerified, false);
    assert.ok(result.routes.every((r) => r.observation === 'INCONCLUSIVE'));
  }
});

test('auth rejection and malformed health stop probes without disclosing response or paths', async (t) => {
  for (const body of [token, JSON.stringify({ ...health, service: token }), JSON.stringify({ ...health, centralServer: token })]) {
    const f = await fixture(t, (_req, res) => respond(res, 200, body));
    const result = await inspectBroker(f.settings);
    assert.deepEqual(result.blockers, ['BROKER_HEALTH_UNVERIFIED']);
    assert.equal(f.calls.length, 1);
    assert.ok(!JSON.stringify(result).includes(token));
  }
  const f = await fixture(t, (_req, res) => respond(res, 401, token));
  assert.equal((await inspectBroker(f.settings)).health.httpStatus, 401);
  assert.equal(f.calls.length, 1);
});

test('redirects never forward local credentials; route auth rejection stays blocked', async (t) => {
  const target = await fixture(t, (_req, res) => respond(res, 200));
  const f = await fixture(t, (req, res) => {
    if (req.url === '/health') return respond(res, 200, JSON.stringify(health));
    res.setHeader('Location', target.settings.brokerUrl);
    respond(res, req.url === '/v1/sessions' ? 302 : 403, token);
  });
  const result = await inspectBroker(f.settings);
  assert.equal(target.calls.length, 0);
  assert.equal(result.status, 'blocked');
  assert.ok(result.blockers.includes('ROUTE_AUTH_REJECTED'));
  assert.ok(!JSON.stringify(result).includes(token));
});

test('absolute deadline and size cap bound hostile health responses', async (t) => {
  const slow = await fixture(t, (_req, res) => { res.writeHead(200); res.write('{'); });
  const start = Date.now();
  const timed = await inspectBroker(slow.settings, 40);
  assert.equal(timed.health.outcome, 'TIMEOUT');
  assert.ok(Date.now() - start < 2000);
  const huge = await fixture(t, (_req, res) => respond(res, 200, token.repeat(2000)));
  assert.equal((await inspectBroker(huge.settings)).health.outcome, 'RESPONSE_TOO_LARGE');
});

test('Connected health cannot hide route server failure or timeout', async (t) => {
  for (const status of [500, 'timeout']) {
    const f = await fixture(t, (req, res) => {
      if (req.url === '/health') return respond(res, 200, JSON.stringify(health));
      if (status === 500) respond(res, 500);
    });
    const result = await inspectBroker(f.settings, 40);
    assert.equal(result.status, 'blocked');
    assert.equal(result.actualMvpVerified, false);
    assert.ok(result.blockers.includes('ROUTE_PROBE_UNVERIFIED'));
  }
});

test('405 routes cannot hide a central authentication failure', async (t) => {
  const f = await fixture(t, (req, res) => respond(res, req.url === '/health' ? 200 : 405,
    req.url === '/health' ? JSON.stringify({ ...health, centralServer: 'AuthFailed' }) : ''));
  const result = await inspectBroker(f.settings);
  assert.equal(result.status, 'blocked');
  assert.deepEqual(result.blockers, ['CENTRAL_NOT_CONNECTED']);
});

test('missing credentials, external URLs and CLI failure exit codes cannot pass silently', async () => {
  assert.deepEqual((await checkIntegration('')).blockers, ['CREDENTIAL_FILE_INVALID']);
  await assert.rejects(inspectBroker({ schemaVersion: 1, brokerUrl: 'https://example.org', localToken: token }));
  const cli = (args) => spawnSync(process.execPath, ['scripts/integration-readiness.mjs', ...args], {
    cwd: new URL('../', import.meta.url), encoding: 'utf8', env: { ...process.env, DEVLINK_BROKER_FILE: '' },
  });
  const missing = cli([]);
  assert.equal(missing.status, 2);
  assert.equal(JSON.parse(missing.stdout).actualMvpVerified, false);
  const invalid = cli(['--timeout-ms', token]);
  assert.equal(invalid.status, 1);
  assert.ok(!invalid.stdout.includes(token));
});

test('CLI invoked through a directory junction/symlink still emits blocked JSON and exit 2', async (t) => {
  const temp = await privateDirectory();
  t.after(temp.cleanup);
  const linked = join(temp.dir, 'checkout');
  await symlink(fileURLToPath(new URL('../', import.meta.url)), linked, process.platform === 'win32' ? 'junction' : 'dir');
  const result = spawnSync(process.execPath, [join(linked, 'scripts/integration-readiness.mjs')], {
    encoding: 'utf8', env: { ...process.env, DEVLINK_BROKER_FILE: '' },
  });
  assert.equal(result.status, 2);
  assert.deepEqual(JSON.parse(result.stdout).blockers, ['CREDENTIAL_FILE_INVALID']);
});
