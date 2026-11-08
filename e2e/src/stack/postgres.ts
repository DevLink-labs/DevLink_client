import { spawn } from "node:child_process";
import { randomBytes } from "node:crypto";
import { rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { execFileAsync, freePort } from "./util.ts";

export interface Postgres {
  port: number;
  database: string;
  username: string;
  /** Kept in memory only; never written to the receipt or logs. */
  password: string;
  stop(): Promise<void>;
}

/**
 * pg_ctl leaves the server running with any inherited stdio handles, so execFile would wait for
 * pipes that never close. Ignore stdio (the server logs to -l) and wait for pg_ctl's own exit.
 */
function pgCtl(command: string, args: string[]): Promise<void> {
  return new Promise((resolve, reject) => {
    const child = spawn(command, args, { stdio: "ignore", windowsHide: true });
    child.once("error", reject);
    child.once("exit", (code) => (code === 0 ? resolve() : reject(new Error(`pg_ctl ${args.at(-1)} exited with ${code}`))));
  });
}

const exe = (bin: string, name: string) => join(bin, process.platform === "win32" ? `${name}.exe` : name);

/** Creates a throwaway cluster on a random loopback port (same approach as the 2026-10-30 manual harness). */
export async function startPostgres(bin: string, directory: string): Promise<Postgres> {
  const { stdout: version } = await execFileAsync(exe(bin, "postgres"), ["--version"], { windowsHide: true });
  if (!/\(PostgreSQL\) 16\./.test(version)) throw new Error(`PostgreSQL 16 is required, found: ${version.trim()}`);
  const data = join(directory, "data");
  const username = "devlink_e2e";
  const database = "devlink_e2e";
  const password = randomBytes(24).toString("hex");
  const port = await freePort();
  const passwordFile = join(directory, `initdb-${randomBytes(6).toString("hex")}.tmp`);
  await writeFile(passwordFile, password, { mode: 0o600, flag: "wx" });
  try {
    await execFileAsync(exe(bin, "initdb"), [`--pgdata=${data}`, `--username=${username}`, `--pwfile=${passwordFile}`,
      "--auth=scram-sha-256", "--encoding=UTF8", "--locale=C"], { windowsHide: true });
  } finally {
    // initdb only reads the file; remove it right away so the password never stays on disk.
    await rm(passwordFile, { force: true });
  }
  const stop = () => pgCtl(exe(bin, "pg_ctl"), ["-D", data, "-m", "fast", "-w", "-t", "60", "stop"]);
  // Ignore a developer's PG* settings (PGSSLMODE, PGSERVICE, ...); only the generated password applies.
  const env = { ...Object.fromEntries(Object.entries(process.env).filter(([key]) => !/^PG/i.test(key))), PGPASSWORD: password };
  try {
    await pgCtl(exe(bin, "pg_ctl"), ["-D", data, "-l", join(directory, "postgres.log"), "-o",
      `-h 127.0.0.1 -p ${port} -c listen_addresses=127.0.0.1`, "-w", "-t", "60", "start"]);
    await execFileAsync(exe(bin, "createdb"), ["-h", "127.0.0.1", "-p", String(port), "-U", username, database], { env, windowsHide: true });
  } catch (error) {
    // A slow start (-t timeout) or a createdb failure can leave a running postmaster behind.
    await stop().catch(() => undefined);
    throw error;
  }
  return {
    port, database, username, password,
    async stop() {
      await stop();
      await rm(data, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
    },
  };
}
