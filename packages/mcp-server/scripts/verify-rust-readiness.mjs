import { spawn } from 'node:child_process';
import { randomUUID } from 'node:crypto';
import { createServer } from 'node:http';
import { writeFile } from 'node:fs/promises';
import { isAbsolute, join } from 'node:path';
import { parseArgs } from 'node:util';
import { setTimeout as pause } from 'node:timers/promises';
import { inspectBroker } from './integration-readiness.mjs';
import { readBrokerSettings } from '../dist/broker/settings.js';
import { privateDirectory } from '../test/helpers/private-settings.mjs';

// Opt-in real Rust reproduction. Owns only a temporary config and foreground
// child, with a synthetic token and a deliberately unavailable loopback central.
async function run() {
  const { values } = parseArgs({ options: { cli: { type: 'string' } }, allowPositionals: false });
  if (!values.cli || !isAbsolute(values.cli)) throw new Error('ABSOLUTE_CLI_REQUIRED');
  const temp = await privateDirectory();
  const unavailable = createServer((_req, res) => { res.writeHead(503); res.end(); });
  let child;
  let exited;
  try {
    await new Promise((resolve, reject) => {
      unavailable.once('error', reject);
      unavailable.listen(0, '127.0.0.1', resolve);
    });
    const config = join(temp.dir, 'config.json');
    await writeFile(config, JSON.stringify({
      serverUrl: `ws://127.0.0.1:${unavailable.address().port}/unavailable`,
      agent: 'integration-readiness-66', autoReconnect: false, localPort: 0,
    }), { mode: 0o600 });
    const settingsPath = join(temp.dir, '.devlink-runtime', 'broker.json');
    let ended = false;
    const isolatedEnv = Object.fromEntries(Object.entries(process.env)
      .filter(([key]) => !/^DEVLINK_/i.test(key)));
    child = spawn(values.cli, ['--config', config, 'broker'], {
      env: { ...isolatedEnv, DEVLINK_TOKEN: randomUUID() }, stdio: 'ignore', windowsHide: true,
    });
    exited = new Promise((resolve) => {
      child.once('error', () => { ended = true; resolve(); });
      child.once('exit', () => { ended = true; resolve(); });
    });
    const deadline = Date.now() + 30_000;
    let report;
    while (true) {
      if (ended) throw new Error('RUST_START_FAILED');
      try {
        const settings = await readBrokerSettings(settingsPath);
        report = await inspectBroker(settings);
        if (report.health?.health) break;
      } catch { /* wait for validated private credentials and a healthy owned broker */ }
      if (Date.now() >= deadline) throw new Error('RUST_START_TIMEOUT');
      await pause(100);
    }
    return { ...report, environment: 'isolated-rust-unavailable-central' };
  } finally {
    let stopped = true;
    if (child && exited) {
      child.kill();
      stopped = await Promise.race([exited.then(() => true), pause(5000, undefined, { ref: false }).then(() => false)]);
    }
    unavailable.closeAllConnections();
    await new Promise((resolve) => unavailable.close(resolve));
    // Keep credentials/config in place if the owned process could still be using them.
    if (!stopped) throw new Error('OWNED_RUST_CLEANUP_FAILED');
    await temp.cleanup();
  }
}

try {
  const report = await run();
  console.log(JSON.stringify(report, null, 2));
  process.exitCode = report.status === 'blocked' ? 2 : 3;
} catch {
  // Paths, child diagnostics and tokens must not appear in shareable output.
  console.log(JSON.stringify({ error: 'ISOLATED_RUST_CHECK_FAILED', actualMvpVerified: false }));
  process.exitCode = 1;
}
