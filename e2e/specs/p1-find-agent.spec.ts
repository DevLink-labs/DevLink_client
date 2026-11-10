import { randomUUID } from "node:crypto";
import type { Actor } from "../src/client/actor.ts";
import { expect, test } from "../src/fixtures.ts";

/** Whether the receiver broker stored the task in its local inbox (actual delivery, unlike get_tasks). */
async function inboxHas(actor: Actor, taskId: string) {
  const inbox = await actor.json<any[]>(["inbox", "list", "--limit", "1024"]);
  return inbox.some((item) => String(item.message.payload.task_id) === taskId);
}

// S15P31D108-188 (FIND-3-01): MCP find_agent -> Rust broker -> central recommendation API.
// S15P31D108-190 (REQ-3-01): a picked candidate receives the task through create_task.
// Git collection (IDX-3-01/02) is not implemented yet, so only role evidence can score here.
test("S15 find_agent relays the central recommendation @p1", async ({ seedTeam, connectMcp }) => {
  // Two desktop setups, a 60 s OFFLINE wait and a reconnect observation.
  test.setTimeout(300_000);
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

  await test.step("REQ-3-01: the picked candidate receives the task through create_task", async () => {
    const [picked] = (await mcp.a.call("find_agent", { question: "로그인 API 오류를 고쳐줄 사람" })).data.candidates;
    // A Host shows the candidate and the user picks it; the test stands in for that choice.
    const made = await mcp.a.call("create_task", {
      recipientAgentId: picked.agentId, title: `S15 추천 후보 요청 ${randomUUID().slice(0, 8)}`,
      description: "find_agent 후보에게 보내는 E2E 요청", taskContext: `nonce=${randomUUID()}`,
    });
    expect(made.data.receiverAgentId).toBe(b.agent.agentId);
    expect(made.data.status).toBe("PENDING");
    const taskId: string = made.data.taskId;
    await expect.poll(async () => (await mcp.a.call("get_task_result", { taskId })).data.events.some((e: any) => e.eventType === "ACK_RECEIVED"),
      { timeout: 10_000 }).toBe(true);
    // Delivery proof: the receiver broker stored it locally (get_tasks alone reads central).
    expect(await inboxHas(b.actor, taskId), "receiver inbox holds the task").toBe(true);
    const received = (await mcp.b.call("get_tasks", { direction: "received" })).data;
    expect(received.items.some((item: any) => item.taskId === taskId)).toBe(true);
  });

  await test.step("B stops: the recommendation reports it OFFLINE without dropping it", async () => {
    await mcp.b.close();
    await b.actor.stop();
    await expect.poll(async () => (await mcp.a.call("find_agent", { question: "로그인 API" })).data.candidates[0]?.connectionStatus,
      { timeout: 60_000, intervals: [1_000, 2_000, 5_000] }).toBe("OFFLINE");
  });

  // The offline-recipient policy is still under team discussion: record what happens and assert only
  // that it is a result the client already handles (any delivery shape the broker accepts, or 409).
  await test.step("observation: a task to the OFFLINE candidate, then B reconnects", async () => {
    const offline = await mcp.a.attempt("create_task", {
      recipientAgentId: b.agent.agentId, title: `S15 오프라인 후보 요청 ${randomUUID().slice(0, 8)}`,
      description: "OFFLINE 후보에게 보내는 관찰용 요청", requestKey: `s15-offline-${randomUUID()}`,
    });
    const created = offline.isError ? undefined : offline.value?.data;
    const observed: Record<string, unknown> = offline.isError
      ? { outcome: "rejected", code: offline.value?.error?.code ?? null, taskId: offline.value?.error?.taskId ?? null }
      : { outcome: "created", status: created?.status, delivery: created?.delivery ?? null };
    if (offline.isError) expect(offline.value?.error?.code).toBe("RECEIVER_OFFLINE");
    else expect(created?.status).toBe("PENDING");
    if (created) {
      await b.actor.start();
      await expect.poll(async () => (await b.actor.brokerHealth())?.centralServer, { timeout: 15_000 }).toBe("Connected");
      const again = await connectMcp(b.actor);
      await again.call("register_agent");
      // A plain deadline loop: the result is recorded, not asserted, until the policy is decided.
      const deadline = Date.now() + 20_000;
      let redelivered = false;
      while (!redelivered && Date.now() < deadline) {
        redelivered = (await mcp.a.call("get_task_result", { taskId: created.taskId })).data.events
          .some((e: any) => e.eventType === "ACK_RECEIVED");
        if (!redelivered) await new Promise((resolve) => setTimeout(resolve, 1_000));
      }
      observed.redeliveredAfterReconnect = redelivered;
      observed.inboxHasTask = await inboxHas(b.actor, created.taskId);
    }
    test.info().annotations.push({ type: "observation:offline-recipient", description: JSON.stringify(observed) });
    console.log(`[S15 observation] offline recipient ${JSON.stringify(observed)}`);
  });
});
