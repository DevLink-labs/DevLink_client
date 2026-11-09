import { execFile, spawn, type ChildProcess } from "node:child_process";
import { createWriteStream } from "node:fs";
import { mkdir } from "node:fs/promises";
import { createServer } from "node:net";
import { join } from "node:path";
import { promisify } from "node:util";

export const execFileAsync = promisify(execFile);

/** Reserves an ephemeral loopback port. The port is released right away, so a tiny race remains. */
export async function freePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const server = createServer();
    server.unref();
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const address = server.address();
      server.close(() => (typeof address === "object" && address ? resolve(address.port) : reject(new Error("No port"))));
    });
  });
}

/** Removes inherited ACLs so only the current user (plus SYSTEM/Administrators via ownership) can write. */
export async function makePrivateDirectory(path: string): Promise<void> {
  await mkdir(path, { recursive: true });
  if (process.platform !== "win32") return;
  // Absolute System32 paths: Git Bash puts a POSIX whoami ahead of the Windows one on PATH.
  const system32 = join(process.env.SystemRoot ?? "C:\\Windows", "System32");
  const { stdout } = await execFileAsync(join(system32, "whoami.exe"), ["/user", "/fo", "csv", "/nh"], { windowsHide: true });
  const sid = stdout.trim().split(",").at(-1)?.replaceAll('"', "");
  if (!sid?.startsWith("S-1-")) throw new Error("Cannot resolve the current user SID");
  await execFileAsync(join(system32, "icacls.exe"), [path, "/inheritance:r", "/grant:r", `*${sid}:(OI)(CI)(F)`], { windowsHide: true });
}

/**
 * Polls `probe` until it returns a value, failing after `timeoutMs`. Used outside test context
 * (global setup) where expect.poll is unavailable; it waits on a condition, not a fixed time.
 */
export async function waitFor<T>(label: string, timeoutMs: number, probe: () => Promise<T | undefined>, alive?: () => boolean): Promise<T> {
  const deadline = Date.now() + timeoutMs;
  let last: unknown;
  while (Date.now() < deadline) {
    if (alive && !alive()) throw new Error(`${label}: process exited`);
    try {
      const value = await probe();
      if (value !== undefined) return value;
    } catch (error) {
      last = error;
    }
    await new Promise((resolve) => setTimeout(resolve, 250));
  }
  throw new Error(`${label}: timed out after ${timeoutMs} ms${last instanceof Error ? ` (${last.message})` : ""}`);
}

/** Starts a long-running child with stdout/stderr appended to `logFile`. */
const spawnFailures = new WeakMap<ChildProcess, Error>();

export function spawnLogged(command: string, args: string[], options: { env: NodeJS.ProcessEnv; cwd?: string; logFile: string }): ChildProcess {
  const log = createWriteStream(options.logFile, { flags: "a" });
  log.on("error", () => undefined); // a log write failure must not crash the runner
  const child = spawn(command, args, { env: options.env, cwd: options.cwd, windowsHide: true, stdio: ["ignore", "pipe", "pipe"] });
  // e.g. ENOENT for a wrong java path: remember it so isRunning() turns false right away.
  child.once("error", (error) => { spawnFailures.set(child, error); log.end(); });
  child.stdout?.pipe(log, { end: false });
  child.stderr?.pipe(log, { end: false });
  child.once("close", () => log.end());
  return child;
}

export function spawnFailure(child: ChildProcess): Error | undefined {
  return spawnFailures.get(child);
}

export function isRunning(child: ChildProcess): boolean {
  return !spawnFailures.has(child) && child.pid !== undefined && child.exitCode === null && child.signalCode === null;
}

/** Stops a child and its process tree; throws when it is still running after `timeoutMs`. */
export async function stopChild(child: ChildProcess | undefined, timeoutMs = 15_000): Promise<void> {
  if (!child || !isRunning(child)) return;
  const closed = new Promise<boolean>((resolve) => child.once("close", () => resolve(true)));
  const timeout = () => new Promise<boolean>((resolve) => setTimeout(() => resolve(false), timeoutMs).unref());
  if (process.platform === "win32" && child.pid) {
    // Kill the whole tree while the parent is still alive: a launcher (e.g. Oracle javapath\java.exe)
    // would otherwise die alone and leave the real JVM running with our stdio pipes.
    await execFileAsync(join(process.env.SystemRoot ?? "C:\\Windows", "System32", "taskkill.exe"), ["/PID", String(child.pid), "/T", "/F"], { windowsHide: true }).catch(() => child.kill());
  } else {
    child.kill();
  }
  if (await Promise.race([closed, timeout()])) return;
  throw new Error(`Process ${child.pid} did not exit within ${timeoutMs} ms`);
}
