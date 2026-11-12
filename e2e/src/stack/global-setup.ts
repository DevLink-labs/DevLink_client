import { execFileSync, spawnSync } from "node:child_process";
import { createHash, randomBytes } from "node:crypto";
import { cp, readdir, readFile, rm, stat, writeFile } from "node:fs/promises";
import { basename, dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import type { FullConfig } from "@playwright/test";
import { decisionModelEnvironment, startCentral, type Central } from "./central.ts";
import { startPostgres, type Postgres } from "./postgres.ts";
import { startStaticProxy, type StaticProxy } from "./static-proxy.ts";
import { makePrivateDirectory } from "./util.ts";
import { Secrets } from "../client/secrets.ts";

const e2eRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..", "..");
const repoRoot = resolve(e2eRoot, "..");

interface Lock { server: { commit: string }; frontend: { commit: string } }

function required(name: string, fallback?: string): string {
  const value = process.env[name]?.trim() || fallback;
  if (!value) throw new Error(`${name} is required (see e2e/scripts/e2e-local.ps1)`);
  return resolve(value);
}

async function mustExist(path: string, label: string) {
  if (!(await stat(path).catch(() => undefined))) throw new Error(`${label} not found: ${path}. Build it before running the E2E suite.`);
}

/**
 * Resolves the real java executable from java.home so the harness owns the JVM process itself, not a
 * launcher such as Oracle's javapath\java.exe (killing a launcher leaves the JVM behind).
 */
async function resolveJava(explicit: string | undefined): Promise<string> {
  const launcher = explicit || "java";
  const probe = spawnSync(launcher, ["-XshowSettings:properties", "-version"], { encoding: "utf8", windowsHide: true });
  const output = `${probe.stderr ?? ""}${probe.stdout ?? ""}`;
  const home = /^\s*java\.home = (.+)$/m.exec(output)?.[1]?.trim();
  if (!home) throw new Error(`Cannot run ${launcher}; install JDK 21 or set E2E_JAVA (e2e-local.ps1 -Java)`);
  const version = /^\s*java\.specification\.version = (.+)$/m.exec(output)?.[1]?.trim();
  if (version !== "21") throw new Error(`central needs Java 21, ${launcher} is ${version ?? "unknown"}`);
  const exe = join(home, "bin", process.platform === "win32" ? "java.exe" : "java");
  await mustExist(exe, "java executable");
  return exe;
}

function headOf(checkout: string): string {
  return execFileSync("git", ["-C", checkout, "rev-parse", "HEAD"], { encoding: "utf8", windowsHide: true }).trim();
}

/**
 * Starts PostgreSQL 16, central and the static frontend + /api proxy once per run. Everything is
 * bound to 127.0.0.1 on random ports. Secrets (DB password, cursor secret) stay in memory.
 */
export default async function globalSetup(_config: FullConfig) {
  const stackDir = required("E2E_STACK_DIR", join(e2eRoot, ".stack"));
  const serverDir = required("E2E_SERVER_DIR", join(stackDir, "server"));
  const frontendDir = required("E2E_FRONTEND_DIR", join(stackDir, "frontend"));
  const pgBin = required("E2E_PG_BIN", join(stackDir, "postgres16", "pgsql", "bin"));
  const cli = required("E2E_DEVLINK_CLI", join(repoRoot, "rust", "target", "release", process.platform === "win32" ? "devlink.exe" : "devlink"));
  const mcpPackage = required("E2E_MCP_PACKAGE", join(repoRoot, "packages", "mcp-server"));
  const java = await resolveJava(process.env.E2E_JAVA?.trim());
  const jar = join(serverDir, "build", "libs", "app.jar");
  const dist = join(frontendDir, "dist");
  for (const [path, label] of [[jar, "central jar"], [join(dist, "index.html"), "frontend build"], [cli, "devlink CLI"],
    [join(mcpPackage, "dist", "index.js"), "MCP server build"], [pgBin, "PostgreSQL bin"]] as const) await mustExist(path, label);

  const lock = JSON.parse(await readFile(join(e2eRoot, "stack.lock.json"), "utf8")) as Lock;
  const serverCommit = headOf(serverDir);
  const frontendCommit = headOf(frontendDir);
  const latest = process.env.E2E_STACK_MODE === "latest";
  // Throws before anything starts when the opt-in flag is set without the settings central needs
  // (enabled, plus a key unless it targets a keyless self-hosted endpoint).
  const decisionModel = decisionModelEnvironment();
  if (!latest && (serverCommit !== lock.server.commit || frontendCommit !== lock.frontend.commit)) {
    throw new Error(`Checkouts differ from e2e/stack.lock.json (server ${serverCommit}, frontend ${frontendCommit}). Set E2E_STACK_MODE=latest to run unpinned.`);
  }
  if (!latest) {
    for (const [checkout, label] of [[serverDir, "server"], [frontendDir, "frontend"]] as const) {
      const status = execFileSync("git", ["-C", checkout, "status", "--porcelain"], { encoding: "utf8", windowsHide: true }).trim();
      if (status) throw new Error(`${label} checkout ${checkout} has local changes; the pinned source must be clean.`);
    }
  }
  // Vite would read an ignored .env*.local and could point the build at a remote API.
  for (const name of [".env.local", ".env.production.local"]) {
    if (await stat(join(frontendDir, name)).catch(() => undefined)) throw new Error(`Remove ${join(frontendDir, name)}: the E2E build must use the same-origin /api proxy.`);
  }
  // e2e-local.ps1 records which commit each artifact was built from; a stale jar/dist must not run as "locked".
  for (const [marker, commit, label] of [[join(serverDir, "build", "e2e-built-commit"), serverCommit, "central jar"],
    [join(dist, ".e2e-built-commit"), frontendCommit, "frontend build"]] as const) {
    const built = (await readFile(marker, "utf8").catch(() => "")).trim();
    if (built !== commit) throw new Error(`${label} was built from ${built || "an unknown commit"}, checkout is ${commit}. Rebuild with e2e/scripts/e2e-local.ps1.`);
  }
  // The client CLI/MCP are built from this working tree; record exactly which binaries ran.
  const clientDirty = execFileSync("git", ["-C", repoRoot, "status", "--porcelain"], { encoding: "utf8", windowsHide: true }).trim() !== "";
  const sha256 = async (path: string) => createHash("sha256").update(await readFile(path)).digest("hex");
  const clientArtifacts = { cliSha256: await sha256(cli), mcpEntrySha256: await sha256(join(mcpPackage, "dist", "index.js")) };

  const runId = `${new Date().toISOString().replace(/[-:]/g, "").replace(/\..*/, "")}-${randomBytes(3).toString("hex")}`;
  const runRoot = join(stackDir, "runs", runId);
  await makePrivateDirectory(runRoot);
  // setup rejects an MCP entry whose directory other accounts can modify, so run a private copy.
  const mcpRuntime = join(runRoot, "mcp-runtime");

  let postgres: Postgres | undefined, central: Central | undefined, proxy: StaticProxy | undefined;
  const stackSecrets = new Secrets();
  // Runs every step even if one fails, then reports all failures.
  const teardown = async () => {
    const failures: unknown[] = [];
    for (const [label, step] of [["central", () => central?.stop()], ["postgres", () => postgres?.stop()], ["proxy", () => proxy?.close()],
      ["mcp runtime copy", () => rm(mcpRuntime, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 })]] as const) {
      try { await step(); } catch (error) { failures.push(new Error(`${label}: ${error instanceof Error ? error.message : String(error)}`, { cause: error })); }
    }
    if (failures.length) throw new AggregateError(failures, `E2E stack teardown failed: ${failures.map((f) => (f as Error).message).join("; ")}`);
  };
  try {
    for (const part of ["dist", "package.json", "node_modules"]) {
      await cp(join(mcpPackage, part), join(mcpRuntime, part), {
        recursive: true,
        filter: (source) => !/[\\/]node_modules[\\/](typescript|@typescript|@types)([\\/]|$)/.test(source),
      });
    }
    postgres = await startPostgres(pgBin, await (async () => { const d = join(runRoot, "postgres"); await makePrivateDirectory(d); return d; })());
    proxy = await startStaticProxy(dist);
    central = await startCentral({ java, jar, directory: runRoot, postgres, frontendOrigin: proxy.origin });
    proxy.setTarget(central.url);
    const health = await fetch(`${proxy.origin}/api/v1/health`, { signal: AbortSignal.timeout(5_000) }).then((r) => r.json()) as { success?: boolean };
    if (health.success !== true) throw new Error("Frontend proxy cannot reach central");

    const receipt = {
      runId, startedAt: new Date().toISOString(), mode: latest ? "latest" : "locked",
      serverCommit, frontendCommit, clientCommit: headOf(repoRoot), clientDirty, ...clientArtifacts,
      postgres: { port: postgres.port, version: 16 },
      central: { url: central.url, pid: central.pid, java, profile: "default", timezone: "UTC", decisionModel: decisionModel.enabled },
      frontend: { origin: proxy.origin, dist: basename(dist) },
    };
    await writeFile(join(runRoot, "stack-receipt.json"), JSON.stringify(receipt, null, 2));

    Object.assign(process.env, {
      E2E_RUN_ROOT: runRoot,
      E2E_FRONTEND_URL: proxy.origin,
      E2E_CENTRAL_URL: central.url,
      E2E_DEVLINK_CLI_PATH: cli,
      E2E_MCP_ENTRY: join(mcpRuntime, "dist", "index.js"),
    });
    // Stack secrets stay in this runner process, out of Chromium and CLI environments.
    stackSecrets.add(postgres.password);
    stackSecrets.add(central.cursorSecret);
    // The model API key goes to central only; the run fails if it shows up in any kept file.
    if (decisionModel.apiKey) stackSecrets.add(decisionModel.apiKey);
  } catch (error) {
    try {
      await teardown();
    } catch (cleanupError) {
      throw new AggregateError([error, cleanupError], "E2E stack startup and teardown failed");
    }
    throw error;
  }
  return async () => {
    const failures: unknown[] = [];
    await teardown().catch((error) => failures.push(error));
    await stackSecrets.assertAbsentInFiles([runRoot, join(e2eRoot, "test-results")]).catch((error) => failures.push(error));
    // Test secrets: workers left SHA-256 digests; check what Playwright wrote after their fixtures.
    await (async () => {
      const digestDir = join(runRoot, "secret-digests");
      const digests = new Set<string>();
      for (const file of await readdir(digestDir).catch(() => [] as string[])) {
        for (const value of JSON.parse(await readFile(join(digestDir, file), "utf8")) as string[]) digests.add(value);
      }
      // Whole run directory too: central.log keeps growing after the last test's own scan.
      await Secrets.assertDigestsAbsent([join(e2eRoot, "test-results"), runRoot], digests);
    })().catch((error) => failures.push(error));
    if (failures.length === 1) throw failures[0];
    if (failures.length) throw new AggregateError(failures, failures.map((f) => (f as Error).message).join("; "));
  };
}
