import { expect, test } from "../src/fixtures.ts";

// S15P31D108-188 (FIND-3-01): MCP find_agent -> Rust broker -> central recommendation API.
// Git collection (IDX-3-01/02) is not implemented yet, so only role evidence can score here.
test("S15 find_agent relays the central recommendation @p1", async ({ seedTeam, connectMcp }) => {
  const { teamId, a, b } = await seedTeam();
  const mcp = { a: await connectMcp(a.actor), b: await connectMcp(b.actor) };
  for (const [key, actor] of [["a", a], ["b", b]] as const) {
    expect((await mcp[key].call("register_agent")).data.agentId).toBe(actor.agent.agentId);
  }

  await test.step("B's role matches the question: one candidate, never the requester", async () => {
    // Without role tags (SET-3-01/02) central splits roleDescription on , ; / | and newlines into tags.
    await mcp.b.call("update_agent", { roleDescription: "로그인, 결제 API" });
    const found = (await mcp.a.call("find_agent", { question: "  로그인 API 오류를 고쳐줄 사람  " })).data;
    expect(found.status).toBe("MATCHED");
    expect(found.insufficientReason).toBeNull();
    expect(found.candidates).toHaveLength(1);
    const [candidate] = found.candidates;
    expect(candidate.agentId).toBe(b.agent.agentId);
    expect(candidate.agentId).not.toBe(a.agent.agentId);
    expect(candidate.connectionStatus).toBe("ONLINE");
    expect(candidate.matchedRoles).toEqual(["로그인"]);
    expect(candidate.score).toBeGreaterThan(0);
    expect(candidate.scores.role).toBeGreaterThan(0);
    // No Git collection exists yet (IDX-3-01/02): role-only scoring, reported apart from missing evidence.
    expect(found.gitCoverage).toEqual({ state: "NOT_COLLECTED", reason: "NO_TEAM_REPOSITORIES", workingState: "NOT_COLLECTED" });
    expect(candidate.scores).toEqual({ role: 20, git: 0, working: 0, frequency: 0, recency: 0 });
    expect(candidate.relatedFiles).toEqual([]);
  });

  await test.step("an unrelated question returns reference candidates without a match", async () => {
    const found = (await mcp.a.call("find_agent", { question: "배포 화면 색상" })).data;
    expect(found.status).toBe("INSUFFICIENT_EVIDENCE");
    expect(found.insufficientReason).toBe("NO_MATCHING_EVIDENCE");
    expect(found.candidates.map((c: any) => [c.agentId, c.insufficientReason])).toEqual([[b.agent.agentId, "NO_MATCHING_EVIDENCE"]]);
  });

  await test.step("validation errors stop before central; central rejects the same input", async () => {
    for (const args of [{}, { question: " \t " }, { question: "가".repeat(1_001) }, { question: "q", repoKey: "AB".repeat(32) }, { question: "q", teamId: "1" }]) {
      expect(await mcp.a.rejectsInput("find_agent", args), JSON.stringify(args).slice(0, 80)).toBe(true);
    }
    const path = `/api/v1/teams/${teamId}/agents/recommendations`;
    await expect(a.actor.central(path, { body: { question: "   " } })).rejects.toThrow(/-> 400$/);
    await expect(a.actor.central(path, { body: { question: "q", repoKey: "x" } })).rejects.toThrow(/-> 400$/);
  });

  await test.step("central denies another team; the broker only ever uses the verified team", async () => {
    await expect(a.actor.central(`/api/v1/teams/${teamId + 1_000_000}/agents/recommendations`, { body: { question: "로그인" } }))
      .rejects.toThrow(/-> 403$/);
    // The broker still works after the direct denial.
    expect((await mcp.a.call("find_agent", { question: "로그인" })).data.candidates[0].agentId).toBe(b.agent.agentId);
  });

  await test.step("B stops: the recommendation reports it OFFLINE without dropping it", async () => {
    await mcp.b.close();
    await b.actor.stop();
    await expect.poll(async () => (await mcp.a.call("find_agent", { question: "로그인 API" })).data.candidates[0]?.connectionStatus,
      { timeout: 60_000, intervals: [1_000, 2_000, 5_000] }).toBe("OFFLINE");
  });
});
