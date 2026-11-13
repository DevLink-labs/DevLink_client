import { execFile } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { promisify } from "node:util";
// @ts-expect-error -- plain ESM helpers shared with packages/mcp-server (no type declarations).
import { captureHost } from "../../packages/mcp-server/test/helpers/host-process.mjs";
// @ts-expect-error -- plain ESM helpers shared with packages/mcp-server (no type declarations).
import { readHostTranscript } from "../../packages/mcp-server/test/helpers/host-transcript.mjs";
import type { ConnectedActor, Stack } from "../src/fixtures.ts";
import { expect, test } from "../src/fixtures.ts";
import type { Secrets } from "../src/client/secrets.ts";

// S15P31D108-191: 3rd MVP integration — part/roles at setup (SET-3-01) → find_agent (FIND-3-01/02)
// → the user's pick → create_task (REQ-3-01) → accept → complete.
// SET-3-03 registers the Git identity used for attribution. The Git-only and role+Git cases run
// in S18 (p1-s18-git-evidence.spec.ts); the upload payload is not inspected by any E2E scenario.
//
// E2E_REAL_HOSTS=1 additionally drives the real Codex (requester A) and Claude Code (receiver B) CLIs
// with the user's own Host login and a DevLink-only MCP config, and verifies their tool events.
// What the models wrote to the user is kept as an annotation for manual review; it is not asserted.

const duty = { a: { part: "FRONTEND" as const, roles: ["화면"] }, b: { part: "BACKEND" as const, roles: ["로그인", "결제 API"] } };
const realHosts = process.env.E2E_REAL_HOSTS === "1";
const notCollected = { state: "NOT_COLLECTED", reason: "NO_TEAM_REPOSITORIES", workingState: "NOT_COLLECTED" };

interface CaseRecord { case: string; status: string; insufficientReason: string | null; gitCoverage: unknown; candidates: unknown[] }
const caseRecord = (name: string, data: any): CaseRecord => ({
  case: name, status: data.status, insufficientReason: data.insufficientReason, gitCoverage: data.gitCoverage,
  candidates: data.candidates.map((c: any) => ({ agentId: c.agentId, part: c.part, connectionStatus: c.connectionStatus, score: c.score,
    scores: c.scores, matchedRoles: c.matchedRoles, relatedFiles: c.relatedFiles.length, insufficientReason: c.insufficientReason })),
});

// Same filter as packages/mcp-server/scripts/verify-onboarding-hosts.mjs, plus harness variables: the
// Host keeps the user's own login but none of the DevLink, nested-Host or Node loader settings.
const hostEnv = () => Object.fromEntries(Object.entries(process.env).filter(([key]) =>
  !/^DEVLINK_|^E2E_|^CODEX_|^CLAUDECODE$|^CLAUDE_CONFIG_DIR$|^CLAUDE_CODE_(SSE_PORT|ENTRYPOINT)$|^NODE_OPTIONS$|^NODE_PATH$/i.test(key)));

/** node.exe processes whose command line carries the marker (the Host-spawned MCP server). */
async function markerProcesses(marker: string, kill = false): Promise<number[]> {
  if (process.platform !== "win32") throw new Error("marker process tracking is Windows-only");
  const script = "$items=@(Get-CimInstance Win32_Process -Filter \"Name = 'node.exe'\" | Where-Object { $_.CommandLine -and $_.CommandLine.Contains($env:S16_MARKER) }); "
    + "if($env:S16_KILL -eq '1'){foreach($item in $items){Stop-Process -Id $item.ProcessId -Force -ErrorAction SilentlyContinue}}; "
    + "ConvertTo-Json -Compress -InputObject @($items | ForEach-Object {$_.ProcessId})";
  const { stdout } = await promisify(execFile)("powershell.exe", ["-NoProfile", "-NonInteractive", "-Command", script],
    { windowsHide: true, timeout: 10_000, env: { ...process.env, S16_MARKER: marker, S16_KILL: kill ? "1" : "0" } });
  return JSON.parse(stdout || "[]");
}

interface HostTurn { calls: { tool: string; args: any; value: any }[]; model: string | null; finalText: string; endedAt: number;
  mcpProcessesSeenWhileRunning: number; mcpProcessesAtExit: number; mcpProcessGoneMs: number | null }

/** Runs one real Host turn against the actor's broker with only the listed DevLink tools enabled. */
async function hostTurn(stack: Stack, secrets: Secrets, actor: ConnectedActor, host: "codex" | "claude", tools: string[], prompt: string,
  label: string, signal: AbortSignal): Promise<HostTurn> {
  const evidence = join(stack.runRoot, "real-hosts", `${label}-${randomUUID().slice(0, 8)}`);
  await mkdir(evidence, { recursive: true });
  // Run outside the repository so the Host does not load this repo's AGENTS.md or project settings.
  const cwd = await mkdtemp(join(tmpdir(), "devlink-s16-"));
  const marker = `devlink-s16-${randomUUID()}`;
  const serverArgs = [`--title=${marker}`, stack.mcpEntry, "--transport", "stdio", "--request-timeout-ms", "10000", "--call-timeout-ms", "30000"];
  let command: string; let args: string[];
  if (host === "claude") {
    const config = join(cwd, "mcp.json");
    await writeFile(config, JSON.stringify({ mcpServers: { devlink: { command: stack.node, args: serverArgs, env: { DEVLINK_BROKER_FILE: actor.actor.brokerFile } } } }));
    command = process.env.DEVLINK_TEST_CLAUDE ?? "claude.exe";
    // Only the (empty) project settings source: no user hooks or settings; login is stored separately.
    args = ["-p", "--model", "claude-opus-5-5", "--effort", "high", "--restricted", "--strict-mcp-config", "--mcp-config", config, "--setting-sources", "project",
      "--tools", "", "--allowedTools", tools.map((t) => `mcp__devlink__${t}`).join(","), "--permission-mode", "dontAsk", "--permission-prompts", "none",
      "--disable-slash-commands", "--no-session-persistence", "--no-chrome", "--output-format", "stream-json", "--verbose", prompt];
  } else {
    const config = `mcp_servers={devlink={command=${JSON.stringify(stack.node)},args=${JSON.stringify(serverArgs)},env={DEVLINK_BROKER_FILE=${JSON.stringify(actor.actor.brokerFile)}},enabled_tools=${JSON.stringify(tools)},required=true,default_tools_approval_mode="approve",startup_timeout_sec=30,tool_timeout_sec=40}}`;
    command = process.env.DEVLINK_TEST_CODEX ?? "codex.exe";
    args = ["--no-daemon", "-a", "never", "exec", "--ignore-user-config", "--ephemeral", "--skip-git-repo-check", "--sandbox", "read-only",
      "--json", "--cd", cwd, "-c", config, "-c", "mcp_optional_startup_grace_ms=0", "-c", "project_doc_max_bytes=0", prompt];
  }
  let result: any; let endedAt = 0; let atExit = 0; let goneMs: number | null = null; let maxSeen = 0;
  try {
    // Sample the marker while the Host runs: proves the detector sees the Host's MCP server, so a
    // count of 0 at exit means the process really ended.
    let running = true;
    const sampler = (async () => {
      while (running) {
        maxSeen = Math.max(maxSeen, (await markerProcesses(marker).catch(() => [])).length);
        await new Promise((resolve) => setTimeout(resolve, 500));
      }
    })();
    try {
      result = await captureHost(command, args, { cwd, env: hostEnv(), timeoutMs: 240_000, signal });
    } finally {
      endedAt = Date.now();
      running = false;
      await sampler;
    }
    secrets.assertAbsent(result.stdout + result.stderr, `${host} output (${label})`);
    await writeFile(join(evidence, "transcript.jsonl"), result.stdout);
    await writeFile(join(evidence, "stderr.txt"), result.stderr);
    // Does the Host leave its MCP server running? Watch for up to 10 s, then stop leftovers.
    atExit = (await markerProcesses(marker)).length;
    while (goneMs === null && Date.now() - endedAt < 10_000) {
      if ((await markerProcesses(marker)).length === 0) goneMs = Date.now() - endedAt;
      else await new Promise((resolve) => setTimeout(resolve, 250));
    }
  } finally {
    await markerProcesses(marker, true).catch(() => []);
    await rm(cwd, { recursive: true, force: true }).catch(() => undefined);
  }
  expect(result.code, `${host} exit (${label})`).toBe(0);
  expect(result.timedOut || result.overflow || result.interrupted || result.spawnError, `${host} run (${label})`).toBeFalsy();
  expect(maxSeen, `marker detector saw the ${host} MCP server while it ran (${label})`).toBeGreaterThan(0);
  const proof = readHostTranscript(host, result.stdout);
  if (host === "claude") expect(proof.model).toBe("claude-opus-5-5");
  expect(proof.calls.every((c: any) => c.completed && !c.hostError && !c.inputError && c.value?.data && !c.value.error),
    `${host} tool calls succeeded (${label})`).toBe(true);
  return { ...proof, endedAt, mcpProcessesSeenWhileRunning: maxSeen, mcpProcessesAtExit: atExit, mcpProcessGoneMs: goneMs };
}

test("S16 3rd MVP: role tags → find_agent → pick → create_task → accept → complete @p1", async ({ stack, secrets, seedTeam, connectMcp }) => {
  // Worst case with real Hosts: 4 turns × (240 s + 10 s watch) + 4 binds × 150 s ≈ 27 min, plus setup.
  test.setTimeout(realHosts ? 2_400_000 : 300_000);
  const testStarted = Date.now();
  const { a, b } = await seedTeam(duty);
  const mcp = { a: await connectMcp(a.actor), b: await connectMcp(b.actor) };
  for (const [key, actor] of [["a", a], ["b", b]] as const) {
    const context = (await mcp[key].call("get_my_context")).data;
    expect(context.principal.agentId).toBe(actor.agent.agentId);
    expect((await mcp[key].call("register_agent")).data.agentId).toBe(actor.agent.agentId);
  }
  const records: CaseRecord[] = [];

  await test.step("SET-3-01: part and role tags from setup are stored centrally", async () => {
    for (const [actor, expected] of [[a, duty.a], [b, duty.b]] as const) {
      const me = await actor.actor.central("/api/v1/agents/me");
      expect({ part: me.part, roles: me.roles }).toEqual(expected);
    }
  });

  await test.step("SET-3-03: register Git email and verify the Agent comparison hash", async () => {
    for (const { actor } of [a, b]) {
      const localPart = randomUUID();
      const email = `${localPart}@example.test`;
      const hash = createHash("sha256").update(email).digest("hex");
      // Scanning the unique local part catches email leaks without recording the comparison hash.
      secrets.add(localPart);
      secrets.add(hash);
      const registered = await actor.json(["git-identity", "register", "--email-stdin"], { stdin: `${email}\n` });
      expect(registered.registered).toBe(true);
      const status = await actor.json(["git-identity", "status"]);
      expect(status.registered).toBe(true);
      const identity = await actor.central("/api/v1/agents/me/git-identity");
      expect(identity.registered).toBe(true);
      expect(identity.gitEmailHash === hash).toBe(true);
    }
  });

  await test.step("case role tags only (Git not collected): the tagged teammate is MATCHED", async () => {
    const found = (await mcp.a.call("find_agent", { question: "로그인 오류를 고쳐줄 담당자" })).data;
    records.push(caseRecord("roleTagsOnly+gitNotCollected", found));
    expect(found.status).toBe("MATCHED");
    expect(found.candidates).toHaveLength(1);
    const [candidate] = found.candidates;
    expect(candidate).toMatchObject({ agentId: b.agent.agentId, part: "BACKEND", connectionStatus: "ONLINE", matchedRoles: ["로그인"], score: 20 });
    expect(candidate.scores).toEqual({ role: 20, git: 0, working: 0, frequency: 0, recency: 0 });
    expect(found.gitCoverage).toEqual(notCollected);
    // B searches too: its own Agent is excluded and A's tag matches.
    const reverse = (await mcp.b.call("find_agent", { question: "결제 화면 레이아웃" })).data;
    records.push(caseRecord("roleTagsOnly(reverse)", reverse));
    expect(reverse.status).toBe("MATCHED");
    expect(reverse.candidates.map((c: any) => [c.agentId, c.matchedRoles, c.score])).toEqual([[a.agent.agentId, ["화면"], 20]]);
    expect(reverse.gitCoverage).toEqual(notCollected);
  });

  await test.step("case insufficient evidence: no tag or Git evidence matches", async () => {
    const found = (await mcp.a.call("find_agent", { question: "배포 파이프라인 캐시 설정" })).data;
    records.push(caseRecord("insufficientEvidence", found));
    expect(found.status).toBe("INSUFFICIENT_EVIDENCE");
    expect(found.insufficientReason).toBe("NO_MATCHING_EVIDENCE");
    expect(found.candidates.map((c: any) => [c.agentId, c.score, c.insufficientReason])).toEqual([[b.agent.agentId, 0, "NO_MATCHING_EVIDENCE"]]);
    expect(found.gitCoverage).toEqual(notCollected);
  });

  await test.step("REQ-3-01 (SDK): pick → create_task → ACK → accept → complete → requester sees COMPLETED", async () => {
    const [picked] = (await mcp.a.call("find_agent", { question: "로그인 오류를 고쳐줄 담당자" })).data.candidates;
    const made = await mcp.a.call("create_task", { recipientAgentId: picked.agentId, title: `S16 로그인 오류 ${randomUUID().slice(0, 8)}`,
      description: "find_agent로 고른 담당자에게 보내는 3차 통합 요청", taskContext: `nonce=${randomUUID()}` });
    expect(made.data.receiverAgentId).toBe(b.agent.agentId);
    const taskId: string = made.data.taskId;
    await expect.poll(async () => (await mcp.a.call("get_task_result", { taskId })).data.events.some((e: any) => e.eventType === "ACK_RECEIVED"),
      { timeout: 10_000 }).toBe(true);
    const pending = (await mcp.b.call("get_task_result", { taskId })).data;
    const accepted = await mcp.b.call("update_task_status", { taskId, version: pending.version, status: "IN_PROGRESS" });
    const done = await mcp.b.call("complete_task", { taskId, version: accepted.data.version, resultText: "S16 완료\n검증: E2E SDK 왕복" });
    expect(done.data.status).toBe("COMPLETED");
    await expect.poll(async () => (await mcp.a.call("get_task_result", { taskId })).data.status, { timeout: 10_000 }).toBe("COMPLETED");
  });

  await test.step("cases not runnable yet are recorded as unverified", async () => {
    const unverified = {
      gitOnly: "not in this scenario; covered by S18 (p1-s18-git-evidence.spec.ts)",
      roleAndGit: "not in this scenario; covered by S18 (p1-s18-git-evidence.spec.ts)",
      uploadPayloadHasNoAbsolutePathDiffOrEmail: "no E2E scenario inspects the upload payload",
    };
    test.info().annotations.push({ type: "s16:cases", description: JSON.stringify(records) });
    test.info().annotations.push({ type: "s16:unverified", description: JSON.stringify(unverified) });
    console.log(`[S16 cases] ${JSON.stringify(records)}`);
    console.log(`[S16 unverified] ${JSON.stringify(unverified)}`);
  });

  if (!realHosts) return;
  // Marker process tracking (and the measured Host CLIs) are Windows-only here; keep the SDK result.
  if (process.platform !== "win32") {
    test.info().annotations.push({ type: "s16:real-hosts", description: "not run: Windows-only stage" });
    return;
  }

  // One live owner per Agent: release the SDK sessions so each real Host can bind.
  await mcp.a.close();
  await mcp.b.close();
  const versions = Object.fromEntries(await Promise.all((["codex", "claude"] as const).map(async (host) => {
    const exe = host === "codex" ? process.env.DEVLINK_TEST_CODEX ?? "codex.exe" : process.env.DEVLINK_TEST_CLAUDE ?? "claude.exe";
    const { stdout } = await promisify(execFile)(exe, ["--version"], { windowsHide: true, timeout: 20_000, env: hostEnv() });
    return [host, stdout.trim()];
  })));
  const abort = new AbortController();
  // Stop a running Host before the test timeout so fixture cleanup never races a live Host.
  const budget = test.info().timeout;
  const stopHosts = budget > 0 ? setTimeout(() => abort.abort(), Math.max(1_000, budget - (Date.now() - testStarted) - 120_000)) : undefined;
  // Measures, after an Agent's own last Host turn, how long until a new MCP client can bind it,
  // then optionally reads through that session and releases it. SESSION_CONFLICT is terminal for
  // one MCP client, so each attempt opens a new session.
  const release: Record<string, unknown>[] = [];
  const bindAfter = async <T>(actor: ConnectedActor, turn: HostTurn, label: string, fn?: (session: any) => Promise<T>) => {
    for (;;) {
      const session = await connectMcp(actor.actor);
      try {
        const bound = await session.attempt("register_agent");
        if (!bound.isError) {
          release.push({ label, afterTurnEndSeconds: Math.round((Date.now() - turn.endedAt) / 1000),
            mcpProcessesSeenWhileRunning: turn.mcpProcessesSeenWhileRunning, mcpProcessesAtExit: turn.mcpProcessesAtExit, mcpProcessGoneMs: turn.mcpProcessGoneMs });
          return fn ? await fn(session) : undefined;
        }
        expect(bound.value?.error?.code, `bind ${label}`).toBe("SESSION_CONFLICT");
      } finally { await session.close(); }
      expect(Date.now() - turn.endedAt, `owner released (${label})`).toBeLessThan(150_000);
      await new Promise((resolve) => setTimeout(resolve, 2_000));
    }
  };
  const findPrompt = (question: string) => "You are a DevLink Agent in an isolated local acceptance test. Use only DevLink MCP tools, serially. "
    + `Call register_agent({}) once, then find_agent once with question exactly ${JSON.stringify(question)}. Do not call any other tool and do not send any task. `
    + "Show the user each candidate as the find_agent description says, then stop and wait for the user to choose.";

  try {
    await test.step("real Hosts: each searches and shows candidates without sending anything", async () => {
      const codex = await hostTurn(stack, secrets, a, "codex", ["register_agent", "find_agent"], findPrompt("로그인 오류를 고쳐줄 담당자"), "codex-find", abort.signal);
      expect(codex.calls.map((c) => c.tool)).toEqual(["register_agent", "find_agent"]);
      expect(codex.calls[1].value.data.candidates.map((c: any) => c.agentId)).toEqual([b.agent.agentId]);
      await bindAfter(a, codex, "after codex-find");
      const claude = await hostTurn(stack, secrets, b, "claude", ["register_agent", "find_agent"], findPrompt("결제 화면 레이아웃"), "claude-find", abort.signal);
      expect(claude.calls.map((c) => c.tool)).toEqual(["register_agent", "find_agent"]);
      expect(claude.calls[1].value.data.candidates.map((c: any) => c.agentId)).toEqual([a.agent.agentId]);
      await bindAfter(b, claude, "after claude-find");
      test.info().annotations.push({ type: "s16:real-host-find (manual review)", description: JSON.stringify({ versions,
        codexModel: codex.model, claudeModel: claude.model,
        candidateConnectionStatus: { codex: codex.calls[1].value.data.candidates[0].connectionStatus, claude: claude.calls[1].value.data.candidates[0].connectionStatus },
        codexFinal: codex.finalText, claudeFinal: claude.finalText }) });
    });

    let realTaskId = "";
    await test.step("real Codex: the user picked B; create_task to exactly that candidate", async () => {
      const title = `S16 실제 Host 요청 ${randomUUID().slice(0, 8)}`;
      const send = await hostTurn(stack, secrets, a, "codex", ["register_agent", "find_agent", "create_task"],
        `You are a DevLink Agent in an isolated local acceptance test. The user already saw find_agent candidates and explicitly chose agentId ${b.agent.agentId}. `
        + "Use only DevLink MCP tools, serially. Call register_agent({}) once, then create_task exactly once with "
        + `recipientAgentId ${JSON.stringify(b.agent.agentId)}, title ${JSON.stringify(title)}, description "로그인 오류 수정 요청(실제 Host 3차 통합 검증)". `
        + "Do not call find_agent again and do not retry. Report the taskId and delivery status.", "codex-send", abort.signal);
      expect(send.calls.map((c) => c.tool)).toEqual(["register_agent", "create_task"]);
      expect(send.calls[1].args.recipientAgentId).toBe(b.agent.agentId);
      realTaskId = send.calls[1].value.data.taskId;
      expect(send.calls[1].value.data.status).toBe("PENDING");
      // B has no Host bound between turns, so it is expected to be OFFLINE: the task is stored and
      // delivered when B binds again. Record what the Host saw and that no ACK exists yet.
      const ackBeforeB = await bindAfter(a, send, "after codex-send", async (session) =>
        (await session.call("get_task_result", { taskId: realTaskId })).data.events.some((e: any) => e.eventType === "ACK_RECEIVED"));
      // Premises of the ACK attribution below: B was not bound, so nothing was delivered yet.
      expect(send.calls[1].value.data.delivery).toEqual({ status: "DELIVERY_NOT_SENT", reasonCode: "RECEIVER_OFFLINE" });
      expect(ackBeforeB).toBe(false);
      test.info().annotations.push({ type: "s16:real-host-send", description: JSON.stringify({
        delivery: send.calls[1].value.data.delivery ?? null, ackBeforeReceiverHostBound: ackBeforeB, codexFinal: send.finalText }) });
    });

    await test.step("real Claude Code: the user accepts the received task; accept and complete it", async () => {
      // B was released by the "after claude-find" session, so the Claude turn below is B's first binding
      // since the task was created; the ACK that follows comes from that binding.
      const work = await hostTurn(stack, secrets, b, "claude", ["register_agent", "get_task_result", "update_task_status", "complete_task"],
        `You are a DevLink Agent in an isolated local acceptance test. The user explicitly accepts received task ${realTaskId} and asks you to finish it without editing files. `
        + "Use only DevLink MCP tools, serially. Call register_agent({}), then get_task_result for that task, then update_task_status to IN_PROGRESS with the version you just read, "
        + "then complete_task with the version returned by that update and resultText \"실제 Host 완료\\n검증: 파일 변경 없음(3차 통합 검증용)\". Do not retry on errors.",
        "claude-work", abort.signal);
      expect(work.calls.map((c) => c.tool)).toEqual(["register_agent", "get_task_result", "update_task_status", "complete_task"]);
      expect(work.calls[3].value.data.status).toBe("COMPLETED");
      await bindAfter(b, work, "after claude-work");
      // A was released after its own check; read the final state through a fresh session.
      const session = await connectMcp(a.actor);
      try {
        await session.call("register_agent");
        const detail = (await session.call("get_task_result", { taskId: realTaskId })).data;
        expect(detail.status).toBe("COMPLETED");
        expect(detail.events.some((e: any) => e.eventType === "ACK_RECEIVED")).toBe(true);
      } finally { await session.close(); }
      test.info().annotations.push({ type: "s16:real-host-work (manual review)", description: JSON.stringify({ claudeModel: work.model, claudeFinal: work.finalText }) });
    });
  } finally {
    clearTimeout(stopHosts);
    abort.abort();
    test.info().annotations.push({ type: "s16:owner-release", description: JSON.stringify(release) });
    console.log(`[S16 owner release] ${JSON.stringify(release)}`);
  }
});
