import { expect, test } from "../src/fixtures.ts";

test("S2 presence follows broker connect and stop @p0", async ({ seedTeam, connectMcp, newScreen }) => {
  const { a, b } = await seedTeam();
  const names = { a: a.agent.name, b: b.agent.name };

  const sessions = await test.step("register_agent binds each Host session and reports ONLINE", async () => {
    const result = { a: await connectMcp(a.actor), b: await connectMcp(b.actor) };
    for (const [key, actor] of [["a", a], ["b", b]] as const) {
      expect((await result[key].call("register_agent")).data.agentId).toBe(actor.agent.agentId);
      await expect.poll(async () => (await actor.actor.central("/api/v1/agents/me")).status, { timeout: 15_000 }).toBe("ONLINE");
    }
    return result;
  });

  const screens = { a: await newScreen(), b: await newScreen() };
  await test.step("both browsers show both agents online", async () => {
    await screens.a.login(a.user);
    await screens.b.login(b.user);
    for (const screen of Object.values(screens)) {
      await screen.expectAgentStatus({ [names.a]: "온라인", [names.b]: "온라인" });
    }
    await screens.a.expectOverviewOnlineCount("2");
  });

  await test.step("B stops its broker: A sees B offline", async () => {
    await sessions.b.close();
    await b.actor.stop();
    await expect.poll(async () => (await b.actor.central("/api/v1/agents/me").catch(() => undefined))?.status, { timeout: 15_000 }).toBe("OFFLINE");
    await screens.a.expectAgentStatus({ [names.a]: "온라인", [names.b]: "오프라인" });
  });

  await test.step("B restarts and registers again: A sees B online", async () => {
    await b.actor.start();
    await expect.poll(async () => (await b.actor.brokerHealth())?.centralServer, { timeout: 15_000 }).toBe("Connected");
    const again = await connectMcp(b.actor);
    expect((await again.call("register_agent")).data.agentId).toBe(b.agent.agentId);
    await expect.poll(async () => (await b.actor.central("/api/v1/agents/me")).status, { timeout: 15_000 }).toBe("ONLINE");
    await screens.a.expectAgentStatus({ [names.a]: "온라인", [names.b]: "온라인" });
  });
});
