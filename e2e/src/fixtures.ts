import { randomBytes } from "node:crypto";
import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { test as base, expect, type Browser, type Page } from "@playwright/test";
import { Actor, type AgentDuty, type Tool } from "./client/actor.ts";
import { McpSession } from "./client/mcp.ts";
import { Secrets } from "./client/secrets.ts";
import { WebSession } from "./client/web-session.ts";
import { WorkspacePage } from "./pages/workspace.ts";

export interface Stack {
  runRoot: string;
  runId: string;
  frontendUrl: string;
  centralUrl: string;
  cli: string;
  mcpEntry: string;
  node: string;
}

export interface UserSpec { label: string; name: string; email: string; password: string }

export interface ConnectedActor {
  user: UserSpec;
  actor: Actor;
  agent: { agentId: string; name: string };
  userId: string;
}

interface Fixtures {
  stack: Stack;
  secrets: Secrets;
  newUser: (label: string) => UserSpec;
  newActor: (label: string, tool: Tool) => Promise<Actor>;
  newScreen: () => Promise<WorkspacePage>;
  connectMcp: (actor: Actor) => Promise<McpSession>;
  /** `webUserId` is the signed-in web user; the desktop app compares setup status against it. */
  desktopSetup: (actor: Actor, user: UserSpec, teamId: number, webUserId: number, duty?: AgentDuty) => Promise<ConnectedActor>;
  /** `duty` sets SET-3-01 part/roles per actor at Agent creation (3rd MVP scenarios). */
  seedTeam: (duty?: { a?: AgentDuty; b?: AgentDuty }) => Promise<{ teamId: number; teamName: string; a: ConnectedActor; b: ConnectedActor }>;
}

/** Runs every cleanup step even when one fails, then throws all failures together. */
async function settleAll(steps: (() => Promise<unknown>)[], message: string) {
  const failures: unknown[] = [];
  for (const step of steps) {
    try { await step(); } catch (error) { failures.push(error); }
  }
  // Playwright reports only message/stack/cause, so put every sub-error into the message.
  if (failures.length) throw new AggregateError(failures, `${message}: ${failures.map((f) => (f instanceof Error ? f.message : String(f))).join("; ")}`);
}

function slug(value: string) {
  return value.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, "").slice(0, 40) || "test";
}

async function newContextPage(browser: Browser, frontendUrl: string): Promise<Page> {
  // Separate contexts: one context would share cookies and the BroadcastChannel session, so a
  // logout in one actor's tab would spread to the other.
  const context = await browser.newContext({ locale: "ko-KR", timezoneId: "Asia/Seoul", reducedMotion: "reduce", baseURL: frontendUrl, viewport: { width: 1440, height: 960 } });
  return context.newPage();
}

export const test = base.extend<Fixtures>({
  stack: async ({}, use) => {
    const runRoot = process.env.E2E_RUN_ROOT;
    if (!runRoot) throw new Error("E2E_RUN_ROOT missing: global setup did not run");
    await use({
      runRoot,
      runId: runRoot.split(/[\\/]/).at(-1)!,
      frontendUrl: process.env.E2E_FRONTEND_URL!,
      centralUrl: process.env.E2E_CENTRAL_URL!,
      cli: process.env.E2E_DEVLINK_CLI_PATH!,
      mcpEntry: process.env.E2E_MCP_ENTRY!,
      node: process.execPath,
    });
  },

  secrets: async ({ stack }, use, testInfo) => {
    const secrets = new Secrets();
    await use(secrets);
    // Playwright writes error-context.md and attachments after fixtures finish; leave only digests so
    // the runner's teardown can still check those files.
    const digestDir = join(stack.runRoot, "secret-digests");
    await mkdir(digestDir, { recursive: true });
    await writeFile(join(digestDir, `${randomBytes(8).toString("hex")}.json`), JSON.stringify(secrets.digests()));
    // Every generated password/token must be absent from what the run keeps.
    await secrets.assertAbsentInFiles([join(stack.runRoot, "logs"), join(stack.runRoot, "actors"), join(stack.runRoot, "central.log"),
      join(stack.runRoot, "stack-receipt.json"), join(stack.runRoot, "postgres", "postgres.log"), testInfo.outputDir]);
  },

  newUser: async ({ stack, secrets }, use, testInfo) => {
    const spec = slug(testInfo.title.split(" ")[0]);
    // repeatEachIndex/retry keep emails unique under --repeat-each and retries within one run.
    const attempt = `${testInfo.repeatEachIndex}${testInfo.retry}`;
    await use((label: string) => {
      const password = `E2e-${randomBytes(18).toString("base64url")}`;
      secrets.add(password);
      return {
        label,
        name: `E2E ${label.toUpperCase()} ${spec}`,
        email: `e2e-${stack.runId.toLowerCase()}-${spec}-${attempt}-${label}@example.test`,
        password,
      };
    });
  },

  newActor: async ({ stack, secrets }, use, testInfo) => {
    const actors: Actor[] = [];
    const logs = join(stack.runRoot, "logs");
    await mkdir(logs, { recursive: true });
    const prefix = `${slug(testInfo.title)}-${testInfo.repeatEachIndex}-${testInfo.retry}`;
    await use(async (label, tool) => {
      const actor = await Actor.create({
        label, tool, root: join(stack.runRoot, "actors", `${prefix}-${label}`), logFile: join(logs, `${prefix}-${label}.log`),
        cli: stack.cli, centralUrl: stack.centralUrl, secrets, projectName: `project-${label}-${randomBytes(3).toString("hex")}`,
      });
      actors.push(actor);
      return actor;
    });
    await settleAll(actors.reverse().map((actor) => () => actor.dispose()), "actor cleanup failed");
  },

  connectMcp: async ({ stack, secrets }, use) => {
    const sessions: McpSession[] = [];
    await use(async (actor) => {
      const session = await McpSession.connect(actor, stack.mcpEntry, secrets);
      sessions.push(session);
      return session;
    });
    await settleAll(sessions.map((session) => () => session.close()), "MCP session cleanup failed");
  },

  newScreen: async ({ browser, stack, secrets }, use) => {
    const pages: Page[] = [];
    await use(async () => {
      const page = await newContextPage(browser, stack.frontendUrl);
      pages.push(page);
      return new WorkspacePage(page, stack.frontendUrl);
    });
    // Browser session cookies are secrets too; register them before the secrets fixture scans.
    for (const page of pages) {
      for (const cookie of await page.context().cookies().catch(() => [])) secrets.add(cookie.value);
    }
    await settleAll(pages.map((page) => () => page.context().close()), "browser context cleanup failed");
  },

  /**
   * The desktop app path: runs `setup` like frontend desktop/setup-ipc.js and judges completion in
   * the same order (JSONL result, then `setup status`).
   */
  desktopSetup: async ({ stack }, use) => {
    await use(async (actor, user, teamId, webUserId, duty) => {
      await actor.initProfile();
      // The app detects installed tools when a folder is picked and only offers detected ones.
      expect(await actor.setupTools(), "setup tools detects the fake Host").toContain(actor.tool);
      // The app asks for the password (user.mode=login) when the CLI profile has no valid session.
      const session = await actor.authStatus();
      expect(session === null || ["logged_out", "expired"].includes(session.state ?? ""), `auth status before setup: ${session?.state}`).toBe(true);
      const { events, code, plan } = await actor.desktopSetup({ email: user.email, password: user.password, teamId, mcpEntry: stack.mcpEntry, node: stack.node, duty });
      actor.started = true;
      expect(events.length, "setup printed JSONL events").toBeGreaterThan(0);
      expect(events.every((event) => event.schemaVersion === 1), "every JSONL record is schemaVersion 1").toBe(true);
      const results = events.filter((event) => event.type === "result");
      expect(results, "exactly one result record").toHaveLength(1);
      expect(events.at(-1)?.type, "result is the last record").toBe("result");
      expect(events.slice(0, -1).every((event) => event.type === "progress"), "only progress before result").toBe(true);
      const result = results[0];
      expect(result.error, `setup error ${JSON.stringify(result.error?.code)}`).toBeUndefined();
      expect(result.exitCode).toBe(0);
      expect(code, "process exit code matches result.exitCode").toBe(result.exitCode);

      const status = await actor.setupStatus({ mcpEntry: stack.mcpEntry, node: stack.node });
      const data = status.data;
      // Same checks as matchesProject() in desktop/setup-ipc.js (string IDs compared strictly).
      expect(data?.connection?.setupReady, "setup status reports setupReady").toBe(true);
      expect(data.userId).toBe(String(webUserId));
      expect(data.teamId).toBe(String(teamId));
      expect(data.toolType).toBe(actor.tool === "claude-code" ? "claude_code" : "codex");
      expect(data.projectPath.replace(/^\\\\\?\\/, "").toLowerCase()).toBe(plan.project.path.toLowerCase());
      await actor.registerStoredSecrets();
      // contracts/setup/v1: the result carries the agent ID, and status must report the same agent.
      const agentId = result.data?.agentId;
      expect(agentId).toMatch(/^[1-9][0-9]*$/);
      expect(data.agentId).toBe(agentId);
      return { user, actor, agent: { agentId, name: plan.agent.name }, userId: String(data.userId) };
    });
  },

  /** Fast path for S2/S3: REST signup + team, then the same desktop setup for both actors. */
  seedTeam: async ({ stack, secrets, newUser, newActor, desktopSetup }, use) => {
    await use(async (duty) => {
      const users = { a: newUser("a"), b: newUser("b") };
      const sessions = { a: new WebSession(stack.centralUrl, secrets), b: new WebSession(stack.centralUrl, secrets) };
      const userIds = { a: 0, b: 0 };
      for (const key of ["a", "b"] as const) {
        await sessions[key].signup(users[key]);
        userIds[key] = (await sessions[key].login(users[key])).userId;
      }
      const teamName = `E2E team ${randomBytes(4).toString("hex")}`;
      const team = await sessions.a.send<{ teamId: number; inviteCode: string }>("POST", "/api/v1/teams", { name: teamName });
      secrets.add(team.inviteCode);
      await sessions.b.send("POST", "/api/v1/teams/join", { inviteCode: team.inviteCode });
      const a = await desktopSetup(await newActor("a", "codex"), users.a, team.teamId, userIds.a, duty?.a);
      const b = await desktopSetup(await newActor("b", "claude-code"), users.b, team.teamId, userIds.b, duty?.b);
      return { teamId: team.teamId, teamName, a, b };
    });
  },
});

export { expect };
