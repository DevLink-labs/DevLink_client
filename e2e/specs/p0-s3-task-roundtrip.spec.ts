import { randomUUID } from "node:crypto";
import type { ConnectedActor } from "../src/fixtures.ts";
import { expect, test } from "../src/fixtures.ts";
import type { McpSession } from "../src/client/mcp.ts";

interface Exchange { taskId: string; title: string; resultText: string; relativePath: string; diffText: string }

async function detail(mcp: McpSession, taskId: string) {
  return (await mcp.call("get_task_result", { taskId })).data;
}

/** Drains get_updates from `cursor` until an update matches, returning it and the new cursor. */
async function waitForUpdate(mcp: McpSession, cursor: string | undefined, match: (update: any) => boolean) {
  let found: any;
  let next = cursor;
  await expect.poll(async () => {
    const page = (await mcp.call("get_updates", next ? { cursor: next, limit: 50 } : { limit: 50 })).data;
    next = page.nextCursor;
    found = page.updates.find(match) ?? found;
    return Boolean(found);
  }, { timeout: 10_000, intervals: [250, 500, 1_000] }).toBe(true);
  return { update: found, cursor: next };
}

/** Steps 1-4 of S3: create -> ACK -> accept (IN_PROGRESS) -> complete -> requester notification. */
async function exchange(sender: ConnectedActor & { mcp: McpSession }, receiver: ConnectedActor & { mcp: McpSession }, label: string): Promise<Exchange> {
  const { cursor: start } = await (async () => {
    const page = (await sender.mcp.call("get_updates", { limit: 50 })).data;
    let cursor = page.nextCursor;
    let more = page.hasMore;
    while (more) {
      const nextPage = (await sender.mcp.call("get_updates", { cursor, limit: 50 })).data;
      cursor = nextPage.nextCursor;
      more = nextPage.hasMore;
    }
    return { cursor };
  })();

  const title = `S3 ${label} ${randomUUID().slice(0, 8)}`;
  const made = await sender.mcp.call("create_task", {
    recipientAgentId: receiver.agent.agentId, title, description: `E2E ${label} 요청 설명\n한글 포함`,
    taskContext: `nonce=${randomUUID()}`,
  });
  expect(made.data.status).toBe("PENDING");
  expect(made.requestKey).toBeTruthy();
  // The receiver may ACK before the create response returns.
  expect(["ACK_PENDING", "ACK_RECEIVED"]).toContain(made.data.delivery?.status ?? made.data.delivery);
  const taskId: string = made.data.taskId;

  await expect.poll(async () => (await detail(sender.mcp, taskId)).events.some((event: any) => event.eventType === "ACK_RECEIVED"),
    { timeout: 10_000 }).toBe(true);
  const inbox = await receiver.actor.json<any[]>(["inbox", "list", "--limit", "1024"]);
  expect(inbox.some((item) => String(item.message.payload.task_id) === taskId), "receiver inbox holds the task").toBe(true);
  const received = (await receiver.mcp.call("get_tasks", { direction: "received" })).data;
  expect(received.items.some((item: any) => item.taskId === taskId)).toBe(true);

  const pending = await detail(receiver.mcp, taskId);
  await sender.mcp.fail("update_task_status", { taskId, version: pending.version, status: "IN_PROGRESS" }, "BROKER_FORBIDDEN");
  const accepted = await receiver.mcp.call("update_task_status", { taskId, version: pending.version, status: "IN_PROGRESS", requestKey: made.requestKey });
  expect(accepted.data.status).toBe("IN_PROGRESS");
  const conflict = await receiver.mcp.fail("update_task_status", { taskId, version: pending.version, status: "IN_PROGRESS", requestKey: made.requestKey }, "TASK_VERSION_CONFLICT");
  expect(conflict.error.taskId).toBe(taskId);

  const relativePath = `src/e2e-${label.replace(/\W+/g, "-").toLowerCase()}.txt`;
  const diffText = `@@ -1 +1 @@\n-before\n+after ${label}\n`;
  const resultText = `${label} 완료\n검증: E2E MCP SDK 왕복`;
  const done = await receiver.mcp.call("complete_task", {
    taskId, version: accepted.data.version, resultText, requestKey: made.requestKey,
    taskFiles: [{ relativePath, changeType: "MODIFIED", diffText }],
  });
  expect(done.data.status).toBe("COMPLETED");

  const { update } = await waitForUpdate(sender.mcp, start, (item) => item.kind === "TASK_COMPLETED" && item.taskId === taskId);
  expect(update.status).toBe("COMPLETED");
  const final = await detail(sender.mcp, taskId);
  expect(final.status).toBe("COMPLETED");
  expect(final.resultText).toBe(resultText);
  expect(final.taskFiles.map(({ fileId, ...file }: any) => file)).toEqual([{ relativePath, changeType: "MODIFIED", diffText }]);
  return { taskId, title, resultText, relativePath, diffText };
}

test("S3 task roundtrip reaches the web conversation @p0", async ({ seedTeam, connectMcp, newScreen }) => {
  const seeded = await seedTeam();
  const a = { ...seeded.a, mcp: await connectMcp(seeded.a.actor) };
  const b = { ...seeded.b, mcp: await connectMcp(seeded.b.actor) };
  for (const actor of [a, b]) {
    expect((await actor.mcp.call("register_agent")).data.agentId).toBe(actor.agent.agentId);
    await expect.poll(async () => (await actor.actor.central("/api/v1/agents/me")).status, { timeout: 15_000 }).toBe("ONLINE");
  }

  const forward = await test.step("A → B: create, ACK, accept, complete, notify", () => exchange(a, b, "A→B"));

  await test.step("both web conversations show the completed result", async () => {
    for (const actor of [a, b]) {
      const screen = await newScreen();
      await screen.login(actor.user);
      const conversation = await screen.openConversation(forward.taskId, forward.title);
      await expect(async () => {
        await conversation.getByRole("button", { name: "대화 기록 새로고침" }).click();
        await expect(conversation.locator('.tc-status[data-status="COMPLETED"]')).toBeVisible({ timeout: 2_000 });
      }).toPass({ timeout: 15_000 });
      await expect(conversation.locator(".tc-participants")).toContainText(a.agent.name);
      await expect(conversation.locator(".tc-participants")).toContainText(b.agent.name);
      const result = conversation.getByRole("region", { name: "저장된 작업 결과" });
      await expect(result.locator(".tc-result-text")).toHaveText(forward.resultText);
      const file = result.locator(".tc-file").filter({ hasText: forward.relativePath });
      await expect(file.locator(".tc-file-change")).toHaveText("수정");
      await file.locator("summary").click();
      await expect(file.locator(".tc-file-diff")).toHaveText(forward.diffText.trimEnd());
      const timeline = conversation.locator(".tc-events");
      await expect(timeline.locator("li[data-event-id]").filter({ hasText: "작업 생성" })).toHaveCount(1);
      await expect(timeline.locator("li[data-event-id]").filter({ hasText: "상태 변경" }).first()).toBeVisible();
    }
  });

  await test.step("B → A: the reverse direction completes too", async () => {
    const reverse = await exchange(b, a, "B→A");
    expect(reverse.taskId).not.toBe(forward.taskId);
  });
});
