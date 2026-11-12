import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { appendFile, mkdir, mkdtemp, readFile, realpath, rm, rmdir, stat, unlink, writeFile } from "node:fs/promises";
import { homedir } from "node:os";
import { basename, delimiter, dirname, join } from "node:path";
import { execFileAsync } from "../stack/util.ts";
import type { Secrets } from "./secrets.ts";

export type Tool = "codex" | "claude-code";

export interface CliResult { code: number | null; stdout: string; stderr: string }

/** SET-3-01 Agent duty: part and role tags for `agent.action=create`. */
export interface AgentDuty { part?: "FRONTEND" | "BACKEND" | "INFRA" | "OTHER"; roles?: string[] }

export interface SetupEvent { schemaVersion: number; type: string; step?: string; stepStatus?: string; exitCode?: number; data?: any; error?: any }

/** Fake Host installs in the npm layout `setup` resolves (.cmd shim + known package entry + Node). */
const hosts: Record<Tool, { shim: string; pkg: string; entry: string; version: string }> = {
  codex: { shim: "codex", pkg: "@openai/codex", entry: "bin/codex.js", version: "codex-cli 0.166.0" },
  "claude-code": { shim: "claude", pkg: "@anthropic-ai/claude-code", entry: "cli.js", version: "2.1.317 (Claude Code)" },
};

/**
 * Environment for every child: no DEVLINK_* (developer broker/token), no harness E2E_* values, no
 * DECISION_MODEL_* (central-only settings, one of them an API key) and no Node preload options
 * inherited from the runner.
 */
export function childEnvironment(extra: NodeJS.ProcessEnv): NodeJS.ProcessEnv {
  const base = Object.fromEntries(Object.entries(process.env).filter(([key, value]) => value !== undefined
    && !/^(DEVLINK_|E2E_|DECISION_MODEL_|NODE_OPTIONS$|NODE_PATH$)/i.test(key)));
  return { ...base, ...extra };
}

let gitDirectoryLookup: Promise<string> | undefined;

/**
 * Directory of the Git executable the runner itself uses. The broker's working-file collector and
 * commit sync run `git`; without it on the actor's isolated PATH they never start and a Git-backed
 * recommendation cannot be exercised (client #155). Several copies can be on PATH (Git Bash puts
 * mingw64\bin first); prefer Git's `cmd` directory, which holds only the launchers. The entry goes
 * last on the actor PATH, so anything else in that directory cannot shadow System32 or the fake Host.
 */
function gitDirectory(): Promise<string> {
  gitDirectoryLookup ??= (async () => {
    // Scan the runner's PATH directly: `where.exe` prints in the OEM code page and would mangle a
    // non-ASCII install path. The harness is Windows-only (SystemRoot, .cmd Host shims).
    const executable = process.platform === "win32" ? "git.exe" : "git";
    const found: string[] = [];
    for (const entry of (process.env.PATH ?? process.env.Path ?? "").split(delimiter).filter(Boolean)) {
      if (!found.includes(entry) && (await stat(join(entry, executable)).catch(() => undefined))?.isFile()) found.push(entry);
    }
    if (!found.length) throw new Error("git is not on the runner's PATH; E2E actors need it for Git collection");
    return found.find((entry) => basename(entry).toLowerCase() === "cmd") ?? found[0];
  })();
  return gitDirectoryLookup;
}

/**
 * One isolated local user: its own --config directory, CLAUDE_CONFIG_DIR, CODEX_HOME, project and
 * fake Host on PATH. Each actor signs in as a different central user: setup takes machine-wide
 * broker owner locks keyed by (central origin, user) plus one per agent (devlink-broker agent_profile.rs).
 */
export class Actor {
  readonly config: string;
  readonly runtime: string;
  readonly project: string;
  readonly env: NodeJS.ProcessEnv;
  readonly targetPath: string;
  started = false;

  private constructor(readonly label: string, readonly tool: Tool, readonly root: string, readonly logFile: string,
    readonly cli: string, readonly centralUrl: string, private readonly secrets: Secrets, env: NodeJS.ProcessEnv, project: string) {
    this.config = join(root, "profile", "config.json");
    this.runtime = join(root, "profile", ".devlink-runtime");
    this.project = project;
    this.env = env;
    this.targetPath = tool === "claude-code" ? join(project, ".mcp.json") : join(project, ".codex", "config.toml");
  }

  static async create(options: { label: string; tool: Tool; root: string; logFile: string; cli: string; centralUrl: string; secrets: Secrets; projectName: string }): Promise<Actor> {
    const { root, tool } = options;
    const bin = join(root, "bin");
    const host = hosts[tool];
    const pkg = join(bin, "node_modules", ...host.pkg.split("/"));
    await mkdir(dirname(join(pkg, host.entry)), { recursive: true });
    await writeFile(join(pkg, "package.json"), JSON.stringify({ type: "module" }));
    // Answers only the version probe; any other invocation would be a real Host run, which E2E must not do.
    await writeFile(join(pkg, host.entry), `if (process.argv.slice(2).join(" ") !== "--version") process.exit(64);\nconsole.log(${JSON.stringify(host.version)});\n`);
    await writeFile(join(bin, `${host.shim}.cmd`), "@echo off\r\nexit /b 64\r\n");
    for (const dir of ["profile", "codex-home", "claude-config"]) await mkdir(join(root, dir), { recursive: true });
    await mkdir(join(root, options.projectName), { recursive: true });
    const project = await realpath(join(root, options.projectName));
    await execFileAsync("git", ["init", "-q", project], { windowsHide: true });
    const systemRoot = process.env.SystemRoot ?? "C:\\Windows";
    const env = childEnvironment({
      // The MCP server checks the broker file ACL through powershell.exe, so keep Windows PowerShell.
      // Git is last: the broker runs it for working-file and commit collection.
      PATH: [bin, dirname(process.execPath), join(systemRoot, "System32"), systemRoot, join(systemRoot, "System32", "Wbem"),
        join(systemRoot, "System32", "WindowsPowerShell", "v1.0"), await gitDirectory()].join(delimiter),
      CODEX_HOME: join(root, "codex-home"),
      CLAUDE_CONFIG_DIR: join(root, "claude-config"),
      NO_COLOR: "1",
    });
    delete env.Path; // Windows keeps both spellings; keep only the isolated PATH.
    return new Actor(options.label, tool, root, options.logFile, options.cli, options.centralUrl, options.secrets, env, project);
  }

  async log(line: string) {
    await appendFile(this.logFile, `[${new Date().toISOString()}] ${this.label} ${line}\n`);
  }

  /** Runs the CLI against this actor's profile. stdin carries passwords; arguments never do. */
  async run(args: string[], options: { stdin?: string; timeoutMs?: number } = {}): Promise<CliResult> {
    const fullArgs = ["--config", this.config, ...args];
    return new Promise((resolve, reject) => {
      const child = spawn(this.cli, fullArgs, { env: this.env, windowsHide: true, stdio: ["pipe", "pipe", "pipe"], cwd: this.root });
      let stdout = "", stderr = "";
      const timer = setTimeout(() => child.kill(), options.timeoutMs ?? 90_000);
      child.stdout.setEncoding("utf8");
      child.stderr.setEncoding("utf8");
      child.stdout.on("data", (chunk: string) => { stdout += chunk; });
      child.stderr.on("data", (chunk: string) => { stderr += chunk; });
      child.on("error", (error) => { clearTimeout(timer); reject(error); });
      child.on("close", (code) => {
        clearTimeout(timer);
        const result = { code, stdout, stderr };
        this.log(`$ devlink ${args.join(" ")} -> ${code}\n${stdout}${stderr ? `\n[stderr]\n${stderr}` : ""}`)
          .then(() => resolve(result), reject);
      });
      child.stdin.on("error", () => undefined);
      child.stdin.end(options.stdin ?? "");
    });
  }

  async ok(args: string[], options: { stdin?: string; timeoutMs?: number } = {}): Promise<string> {
    const result = await this.run(args, options);
    if (result.code !== 0) throw new Error(`devlink ${args.slice(0, 2).join(" ")} failed for ${this.label} with ${result.code}: ${codes(result.stdout + result.stderr)}`);
    return result.stdout;
  }

  async json<T = any>(args: string[], options: { stdin?: string; timeoutMs?: number } = {}): Promise<T> {
    return JSON.parse(await this.ok(args, options)) as T;
  }

  /** The profile the desktop app expects to exist before it runs setup (it reads serverUrl from it). */
  async initProfile() {
    await this.ok(["init"]);
    const settings: [string, string][] = [
      ["serverUrl", this.centralUrl.replace(/^http:/, "ws:")],
      ["localPort", "0"],
      ["taskMessageSyncIntervalMs", "1000"],
      ["reconnectInitialMs", "200"],
      ["taskOverdueMinutes", "0"],
    ];
    for (const [key, value] of settings) await this.ok(["config", "set", key, value]);
  }

  async serverUrl(): Promise<string> {
    return (JSON.parse(await readFile(this.config, "utf8")) as { serverUrl: string }).serverUrl;
  }

  /**
   * Reproduces frontend desktop/setup-runner.js: input file in a temp folder next to the profile,
   * `setup --non-interactive --input <file> --output jsonl --password-stdin --node-path --mcp-entry`,
   * password on stdin only. Returns parsed JSONL events and the exit code.
   */
  async desktopSetup(input: { email: string; password: string; teamId: number; mcpEntry: string; node: string; duty?: AgentDuty }) {
    const name = basename(this.project).trim();
    const plan = {
      schemaVersion: 1,
      // setup-runner.js passes URL.parse(profile.serverUrl).href.
      serverUrl: new URL(await this.serverUrl()).href,
      user: { mode: "login", email: input.email },
      team: { id: String(input.teamId) },
      project: { path: this.project, name },
      tool: { type: this.tool },
      // SET-3-01 (contracts/setup/v1): part/roles are sent only when given, as the desktop app does.
      agent: { action: "create", name, ...(input.duty?.part ? { part: input.duty.part } : {}), ...(input.duty?.roles ? { roles: input.duty.roles } : {}) },
      mcp: { mode: "apply", targetPath: this.targetPath },
    };
    const directory = await mkdtemp(join(dirname(this.config), "setup-input-"));
    const inputFile = join(directory, "input.json");
    let result: CliResult;
    try {
      await writeFile(inputFile, JSON.stringify(plan), { encoding: "utf8", flag: "wx", mode: 0o600 });
      result = await this.run(["setup", "--non-interactive", "--input", inputFile, "--output", "jsonl",
        "--node-path", input.node, "--mcp-entry", input.mcpEntry, "--password-stdin"], { stdin: `${input.password}\n`, timeoutMs: 75_000 });
    } finally {
      await unlink(inputFile).catch(() => undefined);
      await rmdir(directory).catch(() => undefined);
    }
    const lines = result.stdout.split(/\r?\n/).filter((line) => line.trim());
    const events = lines.map((line) => JSON.parse(line) as SetupEvent);
    return { events, code: result.code, plan };
  }

  /** setup-runner.js runJson(): 15 s limit, as the desktop app uses for status/tools/auth. */
  static readonly DESKTOP_JSON_TIMEOUT_MS = 15_000;

  async setupStatus(input: { mcpEntry: string; node: string }) {
    return this.json(["setup", "status", "--output", "json", "--node-path", input.node, "--mcp-entry", input.mcpEntry],
      { timeoutMs: Actor.DESKTOP_JSON_TIMEOUT_MS });
  }

  async setupTools(): Promise<string[]> {
    return this.json(["setup", "tools"], { timeoutMs: Actor.DESKTOP_JSON_TIMEOUT_MS });
  }

  /** checkSession(): `auth status` may exit non-zero; the app then reads its JSON or treats it as no session. */
  async authStatus(): Promise<{ state?: string; authenticated?: boolean } | null> {
    const result = await this.run(["auth", "status"], { timeoutMs: Actor.DESKTOP_JSON_TIMEOUT_MS });
    // runJson() rejects a killed (timed out) process instead of treating it as "no session".
    if (result.code === null) throw new Error(`devlink auth status did not finish within ${Actor.DESKTOP_JSON_TIMEOUT_MS} ms (${this.label})`);
    try { return JSON.parse(result.stdout); } catch { return null; }
  }

  async start() {
    await this.ok(["start"]);
    this.started = true;
  }

  async stop() {
    await this.ok(["stop"]);
    this.started = false;
  }

  async saved<T = any>(name: string): Promise<T> {
    return JSON.parse(await readFile(join(this.runtime, name), "utf8")) as T;
  }

  get brokerFile() { return join(this.runtime, "broker.json"); }

  /** Agent bearer token from the private profile; registered as a secret before use. */
  async agentToken(): Promise<string> {
    const token = (await this.saved<{ state: { token: string } }>("agent-profile.json")).state.token;
    this.secrets.add(token);
    return token;
  }

  async brokerHealth(): Promise<{ centralServer?: string } | undefined> {
    const descriptor = await this.saved<{ brokerUrl: string; localToken: string }>("broker.json").catch(() => undefined);
    if (!descriptor) return undefined;
    this.secrets.add(descriptor.localToken);
    const response = await fetch(`${descriptor.brokerUrl}/health`, { headers: { Authorization: `Bearer ${descriptor.localToken}` }, signal: AbortSignal.timeout(3_000) });
    return response.ok ? response.json() as Promise<{ centralServer?: string }> : undefined;
  }

  async central<T = any>(path: string, init: { method?: string; body?: unknown } = {}): Promise<T> {
    const token = await this.agentToken();
    const response = await fetch(new URL(path, this.centralUrl), {
      method: init.method ?? (init.body ? "POST" : "GET"),
      headers: { Authorization: `Bearer ${token}`, ...(init.body ? { "Content-Type": "application/json" } : {}) },
      ...(init.body ? { body: JSON.stringify(init.body) } : {}),
    });
    if (!response.ok) throw new Error(`central ${path} -> ${response.status}`);
    const value = await response.json() as { success: boolean; data: T };
    if (!value.success) throw new Error(`central ${path} unsuccessful`);
    return value.data;
  }

  /** Collects every token the CLI stored for this actor so the secret scan can look for them. */
  async registerStoredSecrets() {
    for (const [file, pick] of [
      ["agent-profile.json", (v: any) => v?.state?.token],
      ["broker.json", (v: any) => v?.localToken],
      ["runtime.json", (v: any) => v?.controlToken],
      ["user-session.json", (v: any) => v?.state?.cookie],
    ] as const) {
      const value = await this.saved(file).then(pick).catch(() => undefined);
      if (typeof value === "string" && value) this.secrets.add(value);
    }
  }

  /**
   * Stops the broker, removes setup's per-target state under ~/.devlink-mcp-config (not isolated on
   * Windows) and deletes the profile so stored tokens do not outlive the test.
   */
  async dispose() {
    await this.registerStoredSecrets();
    // setup starts the broker itself, so stop unconditionally; stop on a stopped profile is a no-op.
    // Each step runs even if an earlier one fails, so the token-bearing profile is always attempted.
    const failures: Error[] = [];
    const step = async (label: string, action: () => Promise<unknown>) => {
      try { await action(); } catch (error) { failures.push(new Error(`${this.label} ${label}: ${error instanceof Error ? error.message : String(error)}`)); }
    };
    await step("stop", async () => {
      const stopped = await this.run(["stop"]);
      if (stopped.code !== 0) throw new Error(`exit ${stopped.code}; its broker may still run`);
    });
    this.started = false;
    await step("MCP config state cleanup", async () => {
      for (const candidate of new Set([this.targetPath, await realpath(this.targetPath).catch(() => this.targetPath)])) {
        const key = createHash("sha256").update(candidate.replace(/^\\\\\?\\/, "").toLowerCase()).digest("hex");
        await rm(join(homedir(), ".devlink-mcp-config", key), { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
      }
    });
    // The profile holds the agent token and session cookie: failing to delete it fails the test.
    await step("profile removal", () => rm(join(this.root, "profile"), { recursive: true, force: true, maxRetries: 10, retryDelay: 200 }));
    if (failures.length) throw new AggregateError(failures, failures.map((f) => f.message).join("; "));
  }
}

function codes(text: string) {
  return [...new Set(text.match(/\b[A-Z][A-Z_]{3,}\b/g) ?? [])].join(",");
}
