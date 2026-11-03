import { execFile, spawn } from 'node:child_process';
import { promisify } from 'node:util';
import { setTimeout as delay } from 'node:timers/promises';

const execute = promisify(execFile);

/** Bounded capture, including when descendants keep inherited stdio open. */
export async function captureHost(command, args, { cwd, timeoutMs, maxBytes = 8 * 1024 * 1024, signal, env }) {
  const child = spawn(command, args, { cwd, env, windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'] });
  child.stdout.setEncoding('utf8'); child.stderr.setEncoding('utf8');
  let stdout = ''; let stderr = ''; let bytes = 0; let stop;
  const stopped = new Promise((resolve) => { stop = resolve; });
  const collect = (target, chunk) => {
    bytes += Buffer.byteLength(chunk);
    if (bytes > maxBytes) { stop('overflow'); return; }
    if (target === 'stdout') stdout += chunk; else stderr += chunk;
  };
  child.stdout.on('data', (chunk) => collect('stdout', chunk)); child.stderr.on('data', (chunk) => collect('stderr', chunk));
  const completion = new Promise((resolve) => {
    child.once('error', () => resolve({ spawnError: true }));
    child.once('close', (code) => resolve({ code }));
  });
  let interrupted = false;
  const interrupt = () => { interrupted = true; stop('interrupted'); };
  if (signal) signal.addEventListener('abort', interrupt, { once: true });
  else process.once('SIGINT', interrupt);
  if (signal?.aborted) interrupt();
  const timer = setTimeout(() => stop('timedOut'), timeoutMs);
  let outcome;
  try {
    outcome = await Promise.race([completion, stopped]);
    if (interrupted) outcome = 'interrupted';
    if (typeof outcome === 'string') {
      // Never target a PID once the owned parent has exited: it could be reused.
      if (child.pid && child.exitCode === null && child.signalCode === null) {
        if (process.platform === 'win32') {
          await execute('taskkill.exe', ['/PID', String(child.pid), '/T', '/F'], { windowsHide: true, timeout: 5000 }).catch(() => { child.kill(); });
        } else child.kill('SIGKILL');
      }
      await Promise.race([completion, delay(1000)]);
    }
  } finally {
    clearTimeout(timer); process.off('SIGINT', interrupt); signal?.removeEventListener('abort', interrupt);
    child.stdout.destroy(); child.stderr.destroy();
  }
  return { stdout, stderr, ...(typeof outcome === 'string' ? { [outcome]: true } : outcome) };
}
