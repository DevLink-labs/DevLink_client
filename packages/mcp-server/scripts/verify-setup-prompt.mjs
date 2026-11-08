import assert from 'node:assert/strict';
import { execFile, spawn } from 'node:child_process';
import { createHash, randomUUID, randomBytes } from 'node:crypto';
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { parseArgs, promisify } from 'node:util';
import { setTimeout as delay } from 'node:timers/promises';
import { fakeBroker } from '../test/helpers/fake-broker.mjs';
import { privateDirectory } from '../test/helpers/private-settings.mjs';
import { hostConfiguration, readHostTranscript } from '../test/helpers/host-transcript.mjs';
import { ownedProcesses } from './verify-hosts.mjs';

// S15P31D108-145: real Claude Code invokes the devlink_setup prompt; real Codex (no MCP
// Prompts) completes the same onboarding through tools only. Synthetic HTTP broker only.
const execute = promisify(execFile);
const packageDirectory = fileURLToPath(new URL('../', import.meta.url));
const entryPath = join(packageDirectory, 'dist/index.js');
const names = { codex: 'codex.exe', claude: 'claude.exe' };
const promptCommand = '/mcp__devlink__devlink_setup';
// stream-json key order is not stable (a result line can start with another key), so parse each line.
const eventType = (line) => { try { const event = JSON.parse(line); return event.subtype === 'init' ? 'init' : event.type; } catch { return undefined; } };
const countResults = (text) => text.split(/\r?\n/).filter((line) => eventType(line) === 'result').length;

const contextFor = (profile, hostSession) => ({ identity: 'verified',
  principal: { userId: profile.userId, teamId: profile.teamId, agentId: profile.agentId, toolType: profile.toolType },
  agent: { name: 'setup prompt fixture', roleDescription: null, toolType: profile.toolType, projectName: 'devlink-145', source: 'central_agents_me' },
  project: { path: null, source: 'unavailable' }, hostSession });

async function syntheticBroker(profiles) {
  const broker = await fakeBroker({ profiles });
  const bound = (profile, instanceId) => {
    const session = broker.sessions.get(broker.owners.get(profile.agentId));
    return Boolean(session && !session.replaced && session.instanceId === instanceId);
  };
  broker.setHook(async ({ path, req, res, profile }) => {
    if (req.method !== 'GET' || !['/v1/connection', '/v1/context'].includes(path)) return false;
    const hostSession = bound(profile, req.headers['x-instance-id']) ? 'bound' : 'unbound';
    const data = path === '/v1/context' ? contextFor(profile, hostSession) : { observedAt: new Date().toISOString(),
      broker: 'running', central: 'Connected', identity: 'verified', credentials: 'valid', mcpConfig: 'unknown', hostSession,
      setupReady: false, collaborationReady: false, nextActions: [...(hostSession === 'unbound' ? ['register_agent'] : []), 'review_mcp_config'] };
    res.writeHead(200, { 'content-type': 'application/json' }).end(JSON.stringify({ requestId: req.headers['x-request-id'], data }));
    return true;
  });
  return broker;
}

/** Runs one Host process. Claude receives each user turn over stream-json stdin after the previous result. */
async function runHost(host, { directory, credentialPath, turns, timeoutMs, tokens }) {
  const marker = `devlink-host64-${randomUUID()}`;
  const configuration = hostConfiguration(host, { directory, nodePath: process.execPath, entryPath, credentialPath, marker });
  if (configuration.mcp) await writeFile(join(directory, 'mcp.json'), JSON.stringify(configuration.mcp));
  const args = host === 'claude' ? [...configuration.args, '--input-format', 'stream-json'] : [...configuration.args, turns[0]];
  const child = spawn(names[host], args, { cwd: directory, windowsHide: true, stdio: [host === 'claude' ? 'pipe' : 'ignore', 'pipe', 'pipe'] });
  let stdout = ''; let stderr = ''; let results = 0; let partial = ''; let notify;
  child.stdout.setEncoding('utf8'); child.stderr.setEncoding('utf8');
  child.stdout.on('data', (chunk) => {
    stdout += chunk;
    // Count complete lines only once each; the last fragment waits for its newline.
    const lines = (partial + chunk).split(/\r?\n/); partial = lines.pop();
    results += countResults(lines.join('\n')); notify?.();
  });
  child.stderr.on('data', (chunk) => { stderr += chunk; });
  // A Host that exits early (crash, expired login) makes writes fail with EPIPE; the exit code and
  // missing result events report that, and cleanup below must still run.
  child.stdin?.on('error', () => {});
  let spawnFailed = false;
  const closed = new Promise((done) => { child.once('close', (code) => done({ code })); child.once('error', () => { spawnFailed = true; done({ code: null, spawnError: true }); }); });
  const deadline = Date.now() + timeoutMs;
  let timedOut = false;
  try {
    if (host === 'claude') {
      for (const [index, text] of turns.entries()) {
        child.stdin.write(`${JSON.stringify({ type: 'user', message: { role: 'user', content: text } })}\n`);
        while (results <= index) {
          if (Date.now() > deadline || child.exitCode !== null || spawnFailed) { timedOut = Date.now() > deadline; break; }
          await Promise.race([new Promise((done) => { notify = done; }), delay(500)]);
        }
        if (results <= index) break;
      }
      child.stdin.end();
    }
    // An unreferenced timer: a finished run must not keep this runner alive until the deadline.
    const outcome = await Promise.race([closed, delay(Math.max(0, deadline - Date.now()), { timedOut: true }, { ref: false })]);
    if (outcome.timedOut) timedOut = true;
  } finally {
    if (child.exitCode === null && child.pid) await execute('taskkill.exe', ['/PID', String(child.pid), '/T', '/F'], { windowsHide: true, timeout: 5000 }).catch(() => child.kill());
    const end = Date.now() + 7000;
    let remaining = await ownedProcesses(marker);
    while (remaining.length && Date.now() < end) { await delay(200); remaining = await ownedProcesses(marker); }
    if (remaining.length) { await ownedProcesses(marker, true); throw new Error('Host left an MCP child that required forced cleanup'); }
  }
  const { code } = await closed;
  return { stdout, stderr, code, timedOut, tokens };
}

function checkRun(output) {
  for (const token of output.tokens) assert.ok(!output.stdout.includes(token) && !output.stderr.includes(token), 'Credential appeared in Host output');
  assert.ok(!output.timedOut, 'Host execution deadline exceeded');
  assert.equal(output.code, 0, 'Host exited with a failure status');
}

/** Split a Claude stream-json transcript into one parsed transcript per user turn. */
function claudeTurns(stdout) {
  const lines = stdout.split(/\r?\n/).filter((line) => line.trim());
  const init = JSON.parse(lines.find((line) => eventType(line) === 'init') ?? '{}');
  const turns = []; let current = [];
  for (const line of lines) {
    current.push(line);
    if (eventType(line) === 'result') { turns.push(readHostTranscript('claude', current.join('\n'))); current = []; }
  }
  return { init, turns };
}

// Lease renewal/release of the session claimed by an explicit register_agent is lifecycle, not a new step.
const lease = /^(POST|DELETE) \/v1\/sessions\/[^/]+$/;
const arrivalsSince = (broker, start) => broker.arrivals.slice(start).map((call) => `${call.method} ${call.path}`).filter((call) => !lease.test(call))
  .map((call) => call.replace(/^POST \/v1\/sessions\/[^/]+\/bind$/, 'POST /v1/sessions/{id}/bind'));

export async function verifySetupPrompt(options) {
  assert.equal(process.platform, 'win32', 'This actual-Host runner is currently validated for Windows only');
  const hosts = options.host === 'both' ? ['claude', 'codex'] : [options.host];
  assert.ok(hosts.every((host) => host in names), 'Select --host both, claude or codex explicitly');
  const profiles = { claude: { localToken: randomBytes(32).toString('base64url'), userId: '3', teamId: '1', agentId: '12', toolType: 'claude_code' },
    codex: { localToken: randomBytes(32).toString('base64url'), userId: '4', teamId: '1', agentId: '13', toolType: 'codex' } };
  const broker = await syntheticBroker(Object.values(profiles));
  const report = { schemaVersion: 1, story: 'S15P31D108-145', generatedAt: new Date().toISOString(), node: process.version,
    layer: 'real-host-to-mcp-to-http-test-double', actualCentralVerified: false,
    serverEntrySha256: createHash('sha256').update(await readFile(entryPath)).digest('hex'), hosts: [] };
  try {
    for (const host of hosts) {
      const result = { host, version: null, stages: [], passed: false };
      report.hosts.push(result);
      const profile = profiles[host];
      try { result.version = (await execute(names[host], ['--version'], { windowsHide: true, timeout: 10000 })).stdout.trim(); }
      catch { result.failure = 'Host native executable could not be started'; continue; }
      let dir; let cleanup;
      try {
        ({ dir, cleanup } = await privateDirectory());
        const credentialPath = join(dir, 'broker.json');
        await writeFile(credentialPath, JSON.stringify({ schemaVersion: 1, brokerUrl: broker.settings.brokerUrl, localToken: profile.localToken, agentId: profile.agentId }), { mode: 0o600 });
        const hostDirectory = join(dir, 'host'); await mkdir(hostDirectory);
        const save = async (stage, output) => {
          if (!options.artifacts) return checkRun(output);
          await mkdir(options.artifacts, { recursive: true });
          await writeFile(join(options.artifacts, `${host}-${stage}.jsonl`), output.stdout);
          await writeFile(join(options.artifacts, `${host}-${stage}.stderr.txt`), output.stderr);
          checkRun(output);
        };
        if (host === 'claude') {
          options.onProgress?.('claude: prompt, explicit choice, re-diagnosis');
          let start = broker.arrivals.length;
          const turns = [promptCommand,
            // Only the choice: the prompt's own rule must stop the model before register_agent.
            'I choose step 1.',
            'Yes, that Agent is mine. Bind this session to it now.',
            promptCommand];
          const output = await runHost('claude', { directory: hostDirectory, credentialPath, turns, timeoutMs: options.timeoutMs, tokens: [profile.localToken] });
          await save('choice', output);
          const { init, turns: parsed } = claudeTurns(output.stdout);
          assert.equal(init.model, 'claude-opus-5-5', 'Claude used a different model');
          assert.ok(init.slash_commands?.includes('mcp__devlink__devlink_setup'), 'Claude did not list the devlink_setup prompt');
          assert.ok(!init.tools?.some((tool) => tool.includes('devlink_setup')), 'Prompt leaked into the model tool catalog');
          assert.equal(parsed.length, 4, 'Claude did not finish every user turn');
          const arrivals = arrivalsSince(broker, start);
          const calls = (turn) => turn.calls.map((call) => [call.tool, call.args]);
          const succeeded = (turn) => turn.calls.every((call) => call.value && !call.value.error && !call.hostError);
          // Turn 1: only the prompt's own diagnosis reaches the broker; no tool, claim or registration.
          assert.deepEqual(parsed[0].calls, [], 'Claude called a tool before the user chose a step');
          // The broker arrival is the evidence that prompts/get ran; prose is only a weak sanity check.
          assert.deepEqual(arrivals.slice(0, 1), ['GET /v1/connection']);
          assert.match(parsed[0].finalText, /register|bind/i, 'Prompt guidance did not reach the model');
          // Turn 2: choosing the step shows the Agent but must not bind before the user confirms it.
          assert.deepEqual(calls(parsed[1]), [['get_my_context', {}]], 'Claude did not stop after showing the Agent');
          assert.ok(succeeded(parsed[1]), 'Context step failed');
          // Turn 3: after the user's confirmation, registration with an empty object only.
          assert.deepEqual(calls(parsed[2]), [['register_agent', {}]], 'Confirmed registration differs');
          assert.ok(succeeded(parsed[2]), 'Registration failed');
          assert.equal(parsed[2].calls[0].value.data.agentId, profile.agentId);
          // Turn 4: re-diagnosis reflects the binding: no tool and no numbered or registration step.
          assert.deepEqual(parsed[3].calls, [], 'Claude called a tool after the ready diagnosis');
          assert.doesNotMatch(parsed[3].finalText, /register_agent\(|\[register_agent\]/, 'Ready diagnosis still offered registration');
          assert.deepEqual(arrivals, ['GET /v1/connection', 'GET /v1/context', 'POST /v1/sessions', 'POST /v1/agents/register', 'POST /v1/sessions/{id}/bind', 'GET /v1/connection'], `Unexpected broker traffic: ${JSON.stringify(arrivals)}`);
          const claims = broker.calls.filter((call) => call.path === '/v1/sessions');
          assert.ok(claims.every((call) => call.body.mode === 'recover'), 'Registration used a non-recover claim');
          result.model = init.model;
          result.stages.push({ name: 'prompt-choice-rediagnosis', passed: true, slashCommandListed: true, promptInToolCatalog: false,
            turns: parsed.map((turn) => ({ calls: turn.calls.map((call) => call.tool), finalText: turn.finalText.slice(0, 600) })), brokerArrivals: arrivals });

          options.onProgress?.('claude: missing credentials');
          start = broker.arrivals.length;
          const missing = await runHost('claude', { directory: hostDirectory, credentialPath: join(dir, 'missing', 'broker.json'), turns: [promptCommand], timeoutMs: options.timeoutMs, tokens: [profile.localToken] });
          await save('missing', missing);
          const missingTurns = claudeTurns(missing.stdout).turns;
          assert.deepEqual(missingTurns[0].calls, [], 'Claude called a tool for missing credentials');
          assert.match(missingTurns[0].finalText, /devlink setup/, 'Missing-credential guidance did not reach the model');
          assert.deepEqual(arrivalsSince(broker, start), [], 'Missing credentials reached the broker');
          result.stages.push({ name: 'prompt-missing-credentials', passed: true, calls: 0, brokerArrivals: 0, finalText: missingTurns[0].finalText.slice(0, 600) });
        } else {
          options.onProgress?.('codex: tool-only onboarding');
          const start = broker.arrivals.length;
          const prompt = `Authorized DevLink setup test against a synthetic local test double. Codex has no MCP Prompts, so use only the devlink MCP tools.
Do not run commands, read or edit files, browse, or delegate. Call exactly these tools once each, strictly sequentially, waiting for each result:
1. get_connection_status({})
2. get_my_context({})
3. register_agent({})
Then reply with only a JSON object {"hostSessionBefore": <hostSession from step 2>, "agentId": <agentId from step 3>}.`;
          const output = await runHost('codex', { directory: hostDirectory, credentialPath, turns: [prompt], timeoutMs: options.timeoutMs, tokens: [profile.localToken] });
          await save('tools', output);
          const transcript = readHostTranscript('codex', output.stdout);
          assert.deepEqual(transcript.calls.map((call) => [call.tool, call.args]), [['get_connection_status', {}], ['get_my_context', {}], ['register_agent', {}]], 'Codex tool flow differs');
          assert.ok(transcript.calls.every((call) => call.value && !call.value.error && !call.hostError), 'Codex tool flow failed');
          assert.equal(transcript.calls[0].value.data.hostSession, 'unbound');
          assert.ok(transcript.calls[0].value.data.nextActions.includes('register_agent'));
          assert.equal(transcript.calls[2].value.data.agentId, profile.agentId);
          const arrivals = arrivalsSince(broker, start);
          assert.deepEqual(arrivals, ['GET /v1/connection', 'GET /v1/context', 'POST /v1/sessions', 'POST /v1/agents/register', 'POST /v1/sessions/{id}/bind'], `Unexpected broker traffic: ${JSON.stringify(arrivals)}`);
          result.stages.push({ name: 'tool-only-onboarding', passed: true, calls: transcript.calls.map((call) => call.tool), brokerArrivals: arrivals, finalText: transcript.finalText.slice(0, 300) });
        }
        result.passed = true;
      } catch (error) {
        result.failure = String(error.message).split('\n')[0];
        options.onProgress?.(`${host}: failed: ${result.failure}`);
      } finally {
        await cleanup?.().catch(() => { result.cleanupError = true; result.passed = false; });
      }
    }
  } finally { await broker.close(); }
  report.passed = report.hosts.length === hosts.length && report.hosts.every((host) => host.passed);
  return report;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    const { values } = parseArgs({ options: { host: { type: 'string', default: 'both' }, output: { type: 'string' }, artifacts: { type: 'string' }, 'timeout-ms': { type: 'string' } }, strict: true });
    const timeoutMs = values['timeout-ms'] === undefined ? 300000 : Number(values['timeout-ms']);
    assert.ok(Number.isSafeInteger(timeoutMs) && timeoutMs >= 10000 && timeoutMs <= 600000, 'Invalid Host execution timeout');
    const report = await verifySetupPrompt({ host: values.host, timeoutMs, artifacts: values.artifacts ? resolve(values.artifacts) : undefined,
      onProgress: (message) => process.stderr.write(`${message}\n`) });
    if (values.output) { await mkdir(dirname(resolve(values.output)), { recursive: true }); await writeFile(resolve(values.output), `${JSON.stringify(report, null, 2)}\n`); }
    process.stdout.write(`${JSON.stringify(report, null, 2)}\n`);
    if (!report.passed) process.exitCode = 1;
  } catch (error) { process.stderr.write(`${String(error.message).split('\n')[0]}\n`); process.exitCode = 1; }
}
