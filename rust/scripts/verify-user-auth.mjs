// Runs against an explicitly supplied, isolated real central server.
// Creates synthetic users only; never accepts or prints a real password/token.
import assert from 'node:assert/strict';
import { randomUUID } from 'node:crypto';
import { spawnSync } from 'node:child_process';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import { dirname, basename, isAbsolute, join, resolve } from 'node:path';
import { tmpdir } from 'node:os';

const base = new URL(process.env.DEVLINK_TEST_CENTRAL_URL ?? '');
assert(['127.0.0.1', 'localhost', '[::1]'].includes(base.hostname), 'Isolated loopback server required');
assert(base.protocol === 'http:' && base.pathname === '/' && !base.search && !base.username && !base.password);
const cli = process.env.DEVLINK_TEST_CLI;
assert(cli && isAbsolute(cli), 'Absolute DEVLINK_TEST_CLI required');
const idleExpiry = Number(process.env.DEVLINK_TEST_IDLE_EXPIRY_MS ?? '0');
assert(Number.isInteger(idleExpiry) && idleExpiry >= 0 && idleExpiry <= 120_000);
const temporaryParent = resolve(tmpdir());
const temporary = await mkdtemp(join(temporaryParent, 'devlink-central-auth-'));
const config = join(temporary, 'config.json');
const email = `cli89-${randomUUID()}@example.test`;
const password = `Test-${randomUUID()}`;
const env = Object.fromEntries(Object.entries(process.env).filter(([k]) => !/^DEVLINK_/i.test(k)));
const secrets = [password];
function run(args, input, ok = true) {
  const result = spawnSync(cli, ['--config', config, ...args], { env, input, encoding: 'utf8', timeout: 45_000, windowsHide: true });
  const text = `${result.stdout ?? ''}${result.stderr ?? ''}`;
  assert(secrets.every(s => !text.includes(s)), 'CLI exposed a secret');
  assert(!result.error, 'CLI did not complete');
  assert(ok ? result.status === 0 : result.status !== 0, 'Unexpected CLI outcome');
  return result;
}
async function request(path, options = {}) {
  const response = await fetch(new URL(path, base), { ...options, redirect: 'error', signal: AbortSignal.timeout(15_000) });
  const body = await response.json();
  assert(response.ok && body.success, `Central operation failed: ${response.status}`);
  return { response, data: body.data };
}
try {
  const csrf = await request('/api/v1/auth/csrf');
  const cookie = csrf.response.headers.getSetCookie().find(c => c.startsWith('JSESSIONID=')).split(';')[0];
  await request('/api/v1/auth/signup', { method: 'POST', headers: { Cookie: cookie, 'Content-Type': 'application/json', [csrf.data.headerName]: csrf.data.token }, body: JSON.stringify({ email, password, name: 'CLI integration' }) });
  run(['init']);
  run(['config', 'set', 'serverUrl', base.toString().replace(/^http:/, 'ws:')]);
  const login = () => run(['auth', 'login', '--email', email, '--password-stdin'], `${password}\n`);
  const first = JSON.parse(login().stdout);
  assert(first.user.userId > 0 && first.user.email === email);
  const snapshotPath = join(temporary, '.devlink-runtime', 'user-session.json');
  const snapshot = JSON.parse(await readFile(snapshotPath, 'utf8'));
  assert(snapshot.state.kind === 'authenticated');
  secrets.push(snapshot.state.cookie);
  const current = JSON.parse(run(['auth', 'status']).stdout);
  assert(current.authenticated && current.user.userId === first.user.userId);
  const rejected = run(['auth', 'login', '--email', email, '--password-stdin'], 'Wrong-only-test-password\n', false);
  assert(rejected.stderr.includes('INVALID_CREDENTIALS'));
  assert(JSON.parse(run(['auth', 'status']).stdout).authenticated);

  // Invalidate the actual server session independently to exercise server 401.
  const signedCookie = `JSESSIONID=${snapshot.state.cookie}`;
  const renewal = await request('/api/v1/auth/csrf', { headers: { Cookie: signedCookie } });
  await request('/api/v1/auth/logout', { method: 'POST', headers: { Cookie: signedCookie, [renewal.data.headerName]: renewal.data.token } });
  const expired = run(['auth', 'status'], undefined, false);
  assert(expired.stderr.includes('USER_SESSION_EXPIRED'));
  assert(JSON.parse(run(['auth', 'status']).stdout).state === 'expired');
  assert(!(await readFile(snapshotPath, 'utf8')).includes(snapshot.state.cookie));
  login();
  if (idleExpiry) {
    await new Promise(resolve => setTimeout(resolve, idleExpiry));
    const idle = run(['auth', 'status'], undefined, false);
    assert(idle.stderr.includes('USER_SESSION_EXPIRED'));
    assert(JSON.parse(run(['auth', 'status']).stdout).state === 'expired');
    login();
  }
  run(['auth', 'logout']);
  assert(JSON.parse(run(['auth', 'status']).stdout).state === 'logged_out');
  console.log(JSON.stringify({ story: 'S15P31D108-89', actualCentralApi: true, idleExpiryWaitMs: idleExpiry, checks: ['login', 'verified identity', 'bad password preserves session', 'remote invalidation 401', 'local expiry cleanup', 'logout'], secretOutput: false }));
} finally {
  assert(dirname(resolve(temporary)) === temporaryParent && basename(temporary).startsWith('devlink-central-auth-'));
  await rm(temporary, { recursive: true, force: true });
}
