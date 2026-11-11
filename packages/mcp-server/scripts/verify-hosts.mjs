import assert from 'node:assert/strict';
import { execFile, spawn } from 'node:child_process';
import { randomUUID, createHash } from 'node:crypto';
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { dirname, join, resolve, isAbsolute } from 'node:path';
import { tmpdir } from 'node:os';
import { fileURLToPath } from 'node:url';
import { promisify, parseArgs } from 'node:util';
import { setTimeout as delay } from 'node:timers/promises';
import { privateSettings } from '../test/helpers/private-settings.mjs';
import { captureHost } from '../test/helpers/host-process.mjs';
import { hostFixture, matrixSteps } from '../test/helpers/host-fixture.mjs';
import { hostTools, hostConfiguration, readHostTranscript, parseHostAnswer } from '../test/helpers/host-transcript.mjs';

const execute = promisify(execFile);
const packageDirectory = fileURLToPath(new URL('../', import.meta.url));
const entryPath = join(packageDirectory, 'dist/index.js');
const names = { codex: 'codex.exe', claude: 'claude.exe' };

export async function ownedProcesses(marker, kill = false) {
  // Only a marker generated for this invocation is used; never kill by executable name alone.
  assert.match(marker, /^devlink-host64-[0-9a-f-]{36}$/);
  const script = `$ErrorActionPreference='Stop'
$items=@(Get-CimInstance Win32_Process -Filter "Name = 'node.exe'" | Where-Object { $_.CommandLine -and $_.CommandLine.Contains($env:DEVLINK_HOST_MARKER) })
if ($env:DEVLINK_HOST_KILL -eq '1') { foreach ($item in $items) { Stop-Process -Id $item.ProcessId -Force -ErrorAction SilentlyContinue } }
ConvertTo-Json -Compress -InputObject @($items | ForEach-Object { $_.ProcessId })`;
  const { stdout } = await execute('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', script], {
    env: { ...process.env, DEVLINK_HOST_MARKER: marker, DEVLINK_HOST_KILL: kill ? '1' : '0' }, windowsHide: true, timeout: 10000, maxBuffer: 4096,
  });
  return JSON.parse(stdout || '[]');
}

async function verifyProcessQuery() {
  assert.match(process.execPath, /[\\/]node\.exe$/i, 'This Windows runner requires native node.exe');
  const marker = `devlink-host64-${randomUUID()}`;
  const probe = spawn(process.execPath, [`--title=${marker}`, '-e', 'setInterval(()=>{},1000)'], { windowsHide: true, stdio: 'ignore' });
  const ended = new Promise((resolveExit) => { probe.once('exit', resolveExit); probe.once('error', resolveExit); });
  try { assert.ok((await ownedProcesses(marker)).includes(probe.pid), 'Process query failed its positive control'); }
  finally { probe.kill(); await ended; }
}

async function runHost(host, stage, file, prompt, tokens, options) {
  if (options.signal?.aborted) throw Object.assign(new Error('Host run interrupted'), { code: 'HOST_INTERRUPTED' });
  const marker = `devlink-host64-${randomUUID()}`;
  const directory = join(dirname(file.path), 'host');
  await mkdir(directory, { recursive: true });
  const configuration = hostConfiguration(host, { directory, nodePath: process.execPath, entryPath, credentialPath: file.path, marker });
  if (configuration.mcp) await writeFile(join(directory, 'mcp.json'), JSON.stringify(configuration.mcp));
  let captured;
  let forcedCleanup = false;
  let processCheckError = false;
  try {
    captured = await captureHost(names[host], [...configuration.args, prompt], { cwd: directory, timeoutMs: options.timeoutMs, signal: options.signal });
  } finally {
    try {
      const deadline = Date.now() + 7000;
      let remaining = await ownedProcesses(marker);
      while (remaining.length && Date.now() < deadline) { await delay(200); remaining = await ownedProcesses(marker); }
      if (remaining.length) { forcedCleanup = true; await ownedProcesses(marker, true); }
    } catch {
      processCheckError = true;
      await ownedProcesses(marker, true).catch(() => {});
    }
  }
  const { stdout, stderr, code, timedOut, overflow, interrupted, spawnError } = captured;
  if (interrupted || options.signal?.aborted) throw Object.assign(new Error('Host run interrupted'), { code: 'HOST_INTERRUPTED' });
  assert.ok(!processCheckError, 'Owned process inspection failed');
  assert.ok(!spawnError, 'Host native executable could not be started');
  assert.ok(!timedOut, 'Host execution deadline exceeded');
  assert.ok(!overflow, 'Host output exceeded the capture limit');
  for (const token of tokens) assert.ok(!stdout.includes(token) && !stderr.includes(token), 'Credential appeared in Host output');
  if (options.artifacts) {
    await mkdir(options.artifacts, { recursive: true });
    await writeFile(join(options.artifacts, `${host}-${stage}.jsonl`), stdout);
    await writeFile(join(options.artifacts, `${host}-${stage}.stderr.txt`), stderr);
  }
  assert.equal(code, 0, 'Host exited with a failure status');
  assert.equal(forcedCleanup, false, 'Host left an MCP child that required forced cleanup');
  const transcript = readHostTranscript(host, stdout);
  if (host === 'claude') assert.equal(transcript.model, 'claude-opus-5-5', 'Claude used a different model');
  return { ...transcript, exitCode: code, ownedMcpProcessesAfterExit: 0, forcedCleanup };
}

function promptFor(steps, answer) {
  return `Authorized DevLink Host compatibility test. Every record and recipient below is synthetic and belongs to an isolated local test double, not the real central service.
Use only the devlink MCP tools. Do not run commands, read or edit files, browse, use other servers, or delegate.
Execute each numbered step exactly once, strictly sequentially, with only one tool_use per assistant turn. Wait for its result before emitting the next call, including the final two negative calls. Continue to the next numbered step even after an expected error. Never retry or fix a negative test, and never add extra calls.
The FAILED call without reason is an intentional input-validation test: omit reason exactly as shown, even though the description normally requires it. Do not correct the test.
${steps.map((step, index) => `${index + 1}. ${step.tool}(${JSON.stringify(step.args)}) expected: ${step.expect}`).join('\n')}
After the real tool calls, return only a JSON object with these keys and infer their values from the actual responses: ${JSON.stringify(Object.keys(answer))}.
For createStatus/deliveryStatus refer to the create_task response, not the task after later explicit transitions. Do not invent calls or results.`;
}

export function verifyCalls(transcript, steps) {
  assert.equal(transcript.calls.length, steps.length, 'Host did not execute exactly the requested calls');
  return transcript.calls.map((call, index) => {
    const step = steps[index];
    assert.equal(call.tool, step.tool, `Unexpected tool at step ${index + 1}`);
    assert.deepEqual(call.args, step.args, `Wrong arguments at step ${index + 1}`);
    if (step.expect === 'input-validation') assert.ok(call.inputError, 'Host did not expose the SDK input-validation error');
    else if (step.expect.startsWith('BROKER_')) {
      assert.equal(call.value?.error?.code, step.expect, `Wrong error at step ${index + 1}`);
      const outcome = ['BROKER_FORBIDDEN', 'BROKER_UNAUTHORIZED'].includes(step.expect) ? 'rejected' : 'unknown';
      assert.equal(call.value.error.outcome, outcome, `Wrong outcome at step ${index + 1}`);
    }
    else {
      assert.ok(call.value && !call.value.error && !call.hostError, `Missing successful tool result at step ${index + 1}`);
      if (step.expect === 'PENDING') {
        assert.equal(call.value.data.status, 'PENDING'); assert.equal(call.value.data.delivery.status, 'UNKNOWN');
      }
    }
    return { step: index + 1, tool: call.tool, expected: step.expect, representation: call.representation,
      ...(call.value?.error ? { code: call.value.error.code, outcome: call.value.error.outcome } : {}), passed: true };
  });
}

export function verifyWire(wire, transcript, profile) {
  const requestIds = new Set(); const rpcIds = new Set();
  for (const call of wire.filter((call) => call.body.operation)) {
    assert.ok(typeof call.body.requestId === 'string' && call.body.requestId.length > 0, 'Missing HTTP requestId');
    const rpcId = call.body.mcpRequestId;
    assert.ok(typeof rpcId === 'string' || (typeof rpcId === 'number' && Number.isFinite(rpcId)), 'Missing or invalid mcpRequestId');
    assert.ok(!requestIds.has(call.body.requestId) && !rpcIds.has(rpcId), 'Repeated business request correlation');
    requestIds.add(call.body.requestId); rpcIds.add(rpcId);
    for (const key of ['userId', 'teamId', 'agentId', 'toolType']) assert.equal(call.body.context[key], profile[key], 'Wrong authenticated Host context');
    const matching = transcript.calls.find((event) => (event.value?.error ?? event.value)?.requestId === call.body.requestId);
    assert.ok(matching, 'HTTP call has no matching Host tool result');
    assert.equal((matching.value.error ?? matching.value).mcpRequestId, rpcId, 'RPC correlation changed');
  }
  assert.ok(requestIds.size > 0, 'No business HTTP requests were observed');
}

export async function verifyHosts(options) {
  assert.equal(process.platform, 'win32', 'This actual-Host runner is currently validated for Windows only');
  assert.ok(['both', 'codex', 'claude'].includes(options.host), 'Select --host both, codex or claude explicitly');
  assert.ok(Number.isSafeInteger(options.timeoutMs) && options.timeoutMs >= 10000 && options.timeoutMs <= 300000, 'Invalid Host execution timeout');
  const hosts = options.host === 'both' ? ['codex', 'claude'] : [options.host];
  const entryHash = createHash('sha256').update(await readFile(entryPath)).digest('hex');
  await verifyProcessQuery();
  const fixture = await hostFixture();
  const interruption = new AbortController();
  const onInterrupt = () => interruption.abort();
  process.on('SIGINT', onInterrupt);
  const hostOptions = { ...options, signal: interruption.signal };
  const report = { schemaVersion: 1, generatedAt: new Date().toISOString(), platform: process.platform, node: process.version,
    transport: 'stdio', layer: 'real-host-to-mcp-to-http-test-double', sharedFakeBroker: true,
    actualRustIdentityVerified: false, actualCentralBusinessVerified: false, hosts: [],
    serverEntrySha256: entryHash };
  try {
    for (const host of hosts) {
      if (interruption.signal.aborted) { report.interrupted = true; break; }
      let profile = fixture.profile(host);
      const settings = () => ({ schemaVersion: 1, brokerUrl: fixture.broker.settings.brokerUrl, localToken: profile.localToken, agentId: profile.agentId });
      const tokens = [profile.localToken];
      const result = { host, version: null, model: null, stages: [], passed: false };
      report.hosts.push(result);
      let file;
      try {
        result.version = (await execute(names[host], ['--version'], { windowsHide: true, timeout: 10000 })).stdout.trim();
        file = await privateSettings(settings());
        const directory = dirname(file.path);
        assert.ok(isAbsolute(directory) && directory.startsWith(join(tmpdir(), 'devlink-test-')), 'Not an owned temporary directory');
        options.onProgress?.(`${host}: ping`);
        let before = fixture.broker.arrivals.length;
        const ping = await runHost(host, 'ping', file, promptFor([{ tool: 'devlink_ping', args: {}, expect: 'success' }], { status: 'ok', version: '0.1.0' }), tokens, hostOptions);
        verifyCalls(ping, [{ tool: 'devlink_ping', args: {}, expect: 'success' }]);
        assert.equal(ping.calls[0].value.status, 'ok');
        await fixture.broker.drain();
        assert.equal(fixture.broker.arrivals.length, before, 'Catalog/ping activated the broker');
        // The server may expose tools added after this harness was written (`--allowedTools` limits
        // calls, not the catalog). Every tool the harness allows must be offered.
        if (host === 'claude') assert.deepEqual(hostTools.filter((tool) => !ping.catalog.includes(tool)), [], 'Claude catalog lacks an allowed DevLink tool');
        result.model = ping.model;
        result.stages.push({ name: 'initialize-catalog-ping', passed: true, calls: 1, brokerCalls: 0, ownedMcpProcessesAfterExit: 0 });

        options.onProgress?.(`${host}: matrix`);
        before = fixture.broker.calls.length;
        const steps = matrixSteps(host);
        const interpretation = { createTaskId: `task-64-${host}`, createStatus: 'PENDING', deliveryStatus: 'UNKNOWN', timeoutOutcome: 'unknown', recoveredAfterRestart: true, authCode: 'BROKER_UNAUTHORIZED' };
        const matrix = await runHost(host, 'matrix', file, promptFor(steps, interpretation), tokens, hostOptions);
        const checks = verifyCalls(matrix, steps);
        assert.deepEqual(parseHostAnswer(matrix.finalText), interpretation, 'Host interpretation differs');
        const wire = fixture.broker.calls.slice(before);
        verifyWire(wire, matrix, profile);
        assert.equal(wire.filter((call) => call.body.operation === 'create_task').length, 1, 'Host replayed task creation');
        assert.deepEqual(wire.filter((call) => call.body.operation === 'update_task_status').map((call) => call.body.arguments.status), ['IN_PROGRESS'], 'Negative input or automatic FAILED reached the broker');
        const claims = wire.filter((call) => call.path === '/v1/sessions');
        assert.equal(claims[0].body.mode, 'recover');
        assert.ok(claims.slice(1).some((call) => call.body.mode === 'recover'), 'No session recovery after the initial claim was observed');
        assert.equal(new Set(claims.map((call) => call.body.instanceId)).size, 1, 'Recovery replaced the MCP instance');
        result.stages.push({ name: 'business-errors-and-recovery', passed: true, checks, contextVerifiedAgainst: 'HTTP test double',
          rpcIdEchoConsistent: true, hostWireRpcIdObserved: false, interpretation, ownedMcpProcessesAfterExit: 0 });

        options.onProgress?.(`${host}: rotated restart`);
        profile = fixture.rotate(host); tokens.push(profile.localToken);
        await writeFile(file.path, JSON.stringify(settings()));
        before = fixture.broker.calls.length;
        const arrivalStart = fixture.broker.arrivals.length;
        const restarted = await runHost(host, 'rotated', file, promptFor([{ tool: 'get_tasks', args: {}, expect: 'success' }], { ok: true }), tokens, hostOptions);
        verifyCalls(restarted, [{ tool: 'get_tasks', args: {}, expect: 'success' }]);
        await fixture.broker.drain();
        const restartedWire = fixture.broker.calls.slice(before);
        assert.ok(restartedWire.some((call) => call.path === '/v1/sessions' && call.body.mode === 'recover'));
        const sessionReleaseObserved = fixture.broker.arrivals.slice(arrivalStart).some((call) => call.method === 'DELETE');
        // Host process termination can preempt best-effort DELETE. Report that limit
        // explicitly; ownership expiry belongs to the broker, not a fabricated release.
        result.stages.push({ name: 'credential-rotation-and-process-restart', passed: true, newMcpProcess: true, sessionReleaseObserved,
          ...(sessionReleaseObserved ? {} : { limitation: 'Host exit did not deliver a session DELETE. This records exit behavior; fixture rotation restarts the broker and clears prior ownership.' }), ownedMcpProcessesAfterExit: 0 });

        options.onProgress?.(`${host}: unavailable broker`);
        // Reserve and then close an ephemeral loopback port; this stage performs read-only get_tasks only.
        const { createServer } = await import('node:net');
        const reservation = createServer(); reservation.listen(0, '127.0.0.1');
        await new Promise((ready) => reservation.once('listening', ready));
        const port = reservation.address().port;
        await new Promise((closed) => reservation.close(closed));
        fixture.broker.revokeToken(profile.localToken);
        await writeFile(file.path, JSON.stringify({ ...settings(), brokerUrl: `http://127.0.0.1:${port}` }));
        const unavailable = await runHost(host, 'unavailable', file, promptFor([{ tool: 'get_tasks', args: {}, expect: 'BROKER_UNAVAILABLE' }], { errorCode: 'BROKER_UNAVAILABLE' }), tokens, hostOptions);
        verifyCalls(unavailable, [{ tool: 'get_tasks', args: {}, expect: 'BROKER_UNAVAILABLE' }]);
        result.stages.push({ name: 'broker-not-listening', passed: true, code: 'BROKER_UNAVAILABLE', outcome: 'unknown', ownedMcpProcessesAfterExit: 0 });
        result.passed = true;
      } catch (error) {
        result.failure = String(error.message).split('\n')[0];
        options.onProgress?.(`${host}: failed: ${result.failure}`);
        if (error.code === 'HOST_INTERRUPTED' || interruption.signal.aborted) { report.interrupted = true; break; }
      } finally {
        try { await file?.cleanup(); }
        catch { result.cleanupError = 'Temporary directory cleanup failed'; result.passed = false; }
      }
    }
  } finally {
    process.off('SIGINT', onInterrupt);
    if (interruption.signal.aborted) report.interrupted = true;
    await fixture.broker.close();
  }
  report.passed = !report.interrupted && report.hosts.length === hosts.length && report.hosts.every((host) => host.passed);
  return report;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    const { values } = parseArgs({ options: { host: { type: 'string' }, output: { type: 'string' }, artifacts: { type: 'string' }, 'timeout-ms': { type: 'string' } }, strict: true });
    const report = await verifyHosts({ host: values.host, timeoutMs: values['timeout-ms'] === undefined ? 180000 : Number(values['timeout-ms']),
      artifacts: values.artifacts ? resolve(values.artifacts) : undefined, onProgress: (message) => process.stderr.write(`${message}\n`) });
    if (values.output) {
      await mkdir(dirname(resolve(values.output)), { recursive: true });
      await writeFile(resolve(values.output), `${JSON.stringify(report, null, 2)}\n`);
    }
    process.stdout.write(`${JSON.stringify(report, null, 2)}\n`);
    if (!report.passed) process.exitCode = 1;
  } catch (error) { process.stderr.write(`${String(error.message).split('\n')[0]}\n`); process.exitCode = 1; }
}
