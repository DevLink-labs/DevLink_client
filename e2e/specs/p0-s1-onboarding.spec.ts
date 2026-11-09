import { randomBytes } from "node:crypto";
import { readdir, readFile, stat } from "node:fs/promises";
import { join } from "node:path";
import { expect, test } from "../src/fixtures.ts";

const exists = (path: string) => stat(path).then(() => true, () => false);

test("S1 UI onboarding then desktop-app setup @p0", async ({ newUser, newScreen, newActor, desktopSetup, connectMcp, secrets }) => {
  const users = { a: newUser("a"), b: newUser("b") };
  const screenA = await newScreen();
  const screenB = await newScreen();
  const teamName = `E2E S1 ${randomBytes(4).toString("hex")}`;

  const team = await test.step("A: sign up, sign in, create team", async () => {
    await screenA.signup(users.a);
    await screenA.login(users.a);
    const created = await screenA.createTeam(teamName, (value) => secrets.add(value));
    return { ...created, ownerId: await screenA.webUserId() };
  });

  const memberId = await test.step("B: sign up, sign in, join with the invite code", async () => {
    await screenB.signup(users.b);
    await screenB.login(users.b);
    await screenB.joinTeam(team.inviteCode, teamName);
    await screenA.expectMember(users.b.name);
    return screenB.webUserId();
  });

  const connected = await test.step("desktop app path: CLI setup for A (Codex) and B (Claude Code)", async () => ({
    a: await desktopSetup(await newActor("a", "codex"), users.a, team.teamId, team.ownerId),
    b: await desktopSetup(await newActor("b", "claude-code"), users.b, team.teamId, memberId),
  }));

  await test.step("MCP config is written only to the project target", async () => {
    for (const { actor } of Object.values(connected)) {
      const config = await readFile(actor.targetPath, "utf8");
      if (actor.tool === "claude-code") {
        expect(JSON.parse(config).mcpServers.devlink.env.DEVLINK_BROKER_FILE).toBe(actor.brokerFile);
      } else {
        // TOML literal string: the path appears verbatim inside single quotes.
        expect(config).toContain(`DEVLINK_BROKER_FILE = '${actor.brokerFile}'`);
      }
      // Project-scoped only: the actor's Host homes stay empty.
      expect(await readdir(actor.env.CODEX_HOME!)).toEqual([]);
      expect(await readdir(actor.env.CLAUDE_CONFIG_DIR!)).toEqual([]);
      const other = actor.tool === "codex" ? join(actor.project, ".mcp.json") : join(actor.project, ".codex", "config.toml");
      expect(await exists(other), `${actor.label} must not write the other Host's config`).toBe(false);
    }
  });

  await test.step("brokers are connected to central and MCP identity matches the web list", async () => {
    const listed = await screenA.agentIdsFromList([connected.a.agent.name, connected.b.agent.name]);
    for (const { actor, agent, userId } of Object.values(connected)) {
      await expect.poll(async () => (await actor.brokerHealth())?.centralServer, { timeout: 15_000 }).toBe("Connected");
      const mcp = await connectMcp(actor);
      const context = (await mcp.call("get_my_context")).data;
      expect(context.identity).toBe("verified");
      expect(context.principal.agentId).toBe(agent.agentId);
      expect(context.principal.agentId).toBe(listed[agent.name]);
      expect(context.principal.userId).toBe(userId);
      expect(context.principal.teamId).toBe(String(team.teamId));
    }
  });
});
