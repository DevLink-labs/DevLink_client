import { expect, test } from "../src/fixtures.ts";

// S15P31D108-183 (SET-3-02, client side): MCP update_agent and `devlink agent update` change the
// part and role tags through the real central PATCH. The stored values are read back from central
// (GET /api/v1/agents/me with the Agent token), not taken from the tool or CLI result.
test("S17 agent update stores part and role tags on the real central @p1", async ({ seedTeam, connectMcp }) => {
  test.setTimeout(180_000);
  const { a, b } = await seedTeam({ a: { part: "FRONTEND", roles: ["화면"] } });
  const mcp = { a: await connectMcp(a.actor), b: await connectMcp(b.actor) };
  for (const [key, actor] of [["a", a], ["b", b]] as const) {
    expect((await mcp[key].call("register_agent")).data.agentId).toBe(actor.agent.agentId);
  }
  const stored = async (actor = a.actor) => {
    const me = await actor.central("/api/v1/agents/me");
    return { agentId: String(me.agentId), part: me.part, roles: me.roles, projectName: me.projectName, roleDescription: me.roleDescription };
  };
  const initial = await stored();
  const other = await stored(b.actor);
  expect(initial).toMatchObject({ agentId: a.agent.agentId, part: "FRONTEND", roles: ["화면"] });
  const tags = ["로그인", "결제 API"];

  await test.step("invalid part or tags stop before the broker and change nothing", async () => {
    const eleven = Array.from({ length: 11 }, (_, index) => `tag-${index}`);
    for (const args of [{ part: null }, { roles: null }, { part: "DESIGN" }, { roles: ["  "] }, { roles: ["r".repeat(65)] }, { roles: eleven }]) {
      expect(await mcp.a.rejectsInput("update_agent", args), JSON.stringify(args).slice(0, 80)).toBe(true);
    }
    expect(await stored()).toEqual(initial);
  });

  await test.step("part and tags are stored trimmed and without duplicates", async () => {
    const changed = (await mcp.a.call("update_agent", { part: "BACKEND", roles: [" 로그인 ", "결제 API", "로그인"] })).data;
    expect(changed.changedFields).toEqual(["part", "roles"]);
    expect(changed.credentialsRotated).toBe(false);
    expect(changed.agent).toMatchObject({ agentId: a.agent.agentId, part: "BACKEND", roles: tags });
    expect(await stored()).toEqual({ ...initial, part: "BACKEND", roles: tags });
  });

  await test.step("an omitted part and roles keep the stored values", async () => {
    const renamed = (await mcp.a.call("update_agent", { projectName: "S17 omitted" })).data;
    expect(renamed.changedFields).toEqual(["projectName"]);
    expect(await stored()).toEqual({ ...initial, part: "BACKEND", roles: tags, projectName: "S17 omitted" });
  });

  await test.step("a null part and roles keep the stored values", async () => {
    const kept = (await mcp.a.call("update_agent", { projectName: "S17 null", part: null, roles: null })).data;
    expect(kept.changedFields).toEqual(["projectName"]);
    expect(kept.agent).toMatchObject({ part: "BACKEND", roles: tags });
    expect(await stored()).toEqual({ ...initial, part: "BACKEND", roles: tags, projectName: "S17 null" });
  });

  await test.step("an empty roles list removes every tag and keeps the part", async () => {
    const cleared = (await mcp.a.call("update_agent", { roles: [] })).data;
    expect(cleared.changedFields).toEqual(["roles"]);
    expect(await stored()).toEqual({ ...initial, part: "BACKEND", roles: [], projectName: "S17 null" });
    const again = (await mcp.a.call("update_agent", { roles: [] })).data;
    expect(again.updated).toBe(false);
    expect(again.changedFields).toEqual([]);
  });

  await test.step("the CLI changes the same fields", async () => {
    const changed = await a.actor.json(["agent", "update", "--part", "INFRA", "--roles", " 배포 ,로그인,배포"]);
    expect(changed.changedFields).toEqual(["part", "roles"]);
    expect(await stored()).toEqual({ ...initial, part: "INFRA", roles: ["배포", "로그인"], projectName: "S17 null" });
    const cleared = await a.actor.json(["agent", "update", "--clear-roles"]);
    expect(cleared.changedFields).toEqual(["roles"]);
    expect((await stored()).roles).toEqual([]);
    await a.actor.json(["agent", "update", "--roles", "로그인"]);
    expect(await stored()).toEqual({ ...initial, part: "INFRA", roles: ["로그인"], projectName: "S17 null" });
  });

  await test.step("registration, the team list and the recommendation read the stored tags", async () => {
    expect((await mcp.a.call("register_agent")).data).toMatchObject({ agentId: a.agent.agentId, part: "INFRA", roles: ["로그인"] });
    const listed = (await mcp.b.call("list_agents")).data.agents.find((agent: any) => agent.agentId === a.agent.agentId);
    expect(listed).toMatchObject({ part: "INFRA", roles: ["로그인"] });
    const found = (await mcp.b.call("find_agent", { question: "로그인 오류를 고쳐줄 사람" })).data;
    const candidate = found.candidates.find((item: any) => item.agentId === a.agent.agentId);
    expect(candidate?.matchedRoles).toEqual(["로그인"]);
  });

  await test.step("the other Agent and this Agent's identity are unchanged", async () => {
    expect(await stored(b.actor)).toEqual(other);
    expect((await stored()).agentId).toBe(initial.agentId);
  });
});
