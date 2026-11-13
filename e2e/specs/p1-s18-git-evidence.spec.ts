import { execFile } from "node:child_process";
import { randomBytes, randomUUID } from "node:crypto";
import { mkdir, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { promisify } from "node:util";
import { expect, test } from "../src/fixtures.ts";

// S15P31D108-242: find_agent with Git evidence from real work in a small project. B writes and
// commits a few files, keeps two changes uncommitted, and its broker uploads the working files
// (IDX-3-02) and commit history (IDX-3-01) with B's registered Git email (SET-3-03). A then asks.
// The cases S16 left unverified ("Git only", "role + Git") are covered here. The upload payload
// (no absolute path, diff or email) is not inspected here; the broker's own tests cover it.
//
// Optional: E2E_DECISION_MODEL=1 records how central's role decision model (S15P31D108-241) answers
// the same questions. It needs a central built from a commit that has the feature (E2E_STACK_MODE=latest
// with such a checkout) and DECISION_MODEL_ENABLED=true plus DECISION_MODEL_API_KEY in the runner's
// environment (or DECISION_MODEL_URL with DECISION_MODEL_REQUIRE_API_KEY=false for a keyless
// self-hosted endpoint); the harness passes DECISION_MODEL_* to central only with this flag and fails the run
// if the key appears in a kept file. Every find_agent with an unmatched tag is then one model call
// (paid on OpenRouter). The default run uses no model.
const run = promisify(execFile);
const decisionModel = process.env.E2E_DECISION_MODEL === "1";
const duty = { a: { part: "FRONTEND" as const, roles: ["화면", "UI"] }, b: { part: "BACKEND" as const, roles: ["결제", "로그인"] } };

const committed: Record<string, string> = {
  "README.md": "# shop-sample\n\n작은 쇼핑몰 백엔드 예제.\n",
  "src/payment/PaymentController.java": "package shop.payment;\n\npublic class PaymentController {\n    public String approve(long orderId) { return \"APPROVED\"; }\n}\n",
  "src/payment/RefundService.java": "package shop.payment;\n\npublic class RefundService {\n    public boolean refund(long paymentId) { return false; }\n}\n",
  "src/auth/LoginController.java": "package shop.auth;\n\npublic class LoginController {\n    public String login(String id) { return \"session\"; }\n}\n",
};

interface Observation { label: string; question: string; ms: number; status: string; insufficientReason: string | null; gitCoverage: unknown;
  candidates: { agent: string; score: number; scores: Record<string, number>; matchedRoles: string[]; relatedFiles: string[]; latestCommitAt: string | null; workingObservedAt: string | null }[] }

test("S18 find_agent uses Git evidence from work in a small project @p1", async ({ seedTeam, connectMcp, secrets, stack }) => {
  // Two desktop setups, a broker restart and waiting for the first working-file and commit uploads.
  test.setTimeout(420_000);
  const { teamId, a, b } = await seedTeam(duty);
  const project = b.actor.project;
  const git = (...args: string[]) => run("git", ["-C", project, ...args], { windowsHide: true });
  const names: Record<string, string> = { [a.agent.agentId]: "A", [b.agent.agentId]: "B" };
  const observations: Observation[] = [];

  await test.step("B works in a small project: commit four files, then leave two changes uncommitted", async () => {
    // Commits count for B only when their author email is B's registered Git email.
    const localPart = randomUUID();
    const email = `${localPart}@example.test`;
    secrets.add(localPart);
    expect((await b.actor.json(["git-identity", "register", "--email-stdin"], { stdin: `${email}\n` })).registered).toBe(true);
    // The remote only feeds the repository key; nothing connects to it.
    await git("remote", "add", "origin", `https://github.com/devlink-e2e/shop-sample-${randomBytes(4).toString("hex")}.git`);
    for (const [path, body] of Object.entries(committed)) {
      await mkdir(dirname(join(project, path)), { recursive: true });
      await writeFile(join(project, path), body);
    }
    await git("add", "README.md", "src");
    // Not the developer's signing key or hooks: either would fail, block or add side effects to this
    // commit. The hooks path names a directory that does not exist, so no global hook runs.
    await git("-c", "user.name=E2E B", "-c", `user.email=${email}`, "-c", "commit.gpgsign=false",
      "-c", `core.hooksPath=${join(b.actor.root, "no-git-hooks")}`, "commit", "-q", "--no-verify", "-m", "shop payment and login skeleton");
    await writeFile(join(project, "src/payment/RefundService.java"), committed["src/payment/RefundService.java"].replace("return false;", "return paymentId > 0;"));
    await writeFile(join(project, "src/payment/RefundPolicy.java"), "package shop.payment;\n\npublic class RefundPolicy {\n    public int days() { return 7; }\n}\n");
    // The collectors read the remote once when the broker starts.
    await b.actor.stop();
    await b.actor.start();
    await expect.poll(async () => (await b.actor.brokerHealth())?.centralServer, { timeout: 20_000 }).toBe("Connected");
  });

  const mcp = { a: await connectMcp(a.actor), b: await connectMcp(b.actor) };
  for (const [key, actor] of [["a", a], ["b", b]] as const) {
    expect((await mcp[key].call("register_agent")).data.agentId).toBe(actor.agent.agentId);
  }

  const ask = async (label: string, question: string): Promise<Observation> => {
    const started = Date.now();
    const data = (await mcp.a.call("find_agent", { question })).data;
    const observation: Observation = {
      label, question, ms: Date.now() - started, status: data.status, insufficientReason: data.insufficientReason, gitCoverage: data.gitCoverage,
      candidates: data.candidates.map((c: any) => ({ agent: names[c.agentId] ?? c.agentId, score: c.score, scores: c.scores, matchedRoles: c.matchedRoles,
        relatedFiles: c.relatedFiles.map((f: any) => `${f.relativePath} [${f.sources.join("+")}]`), latestCommitAt: c.latestCommitAt, workingObservedAt: c.workingObservedAt })),
    };
    observations.push(observation);
    return observation;
  };

  await test.step("the broker's uploads reach central: repository key, working files, commits", async () => {
    // Same rule the broker uses; this failed with "Cannot start Git" before the actor PATH carried Git.
    const repoKey = /[0-9a-f]{64}/.exec((await b.actor.ok(["dependency-repo-key"])))?.[0];
    expect(repoKey, "dependency-repo-key prints the repository key").toBeTruthy();
    const revision = await b.actor.central(`/api/v1/teams/${teamId}/git/working-files/${repoKey}`);
    expect(revision.protocolVersion).toBe(2);
    // Both of B's tags match this question by tokens, so an enabled decision model is not called while waiting.
    const probe = async () => (await mcp.a.call("find_agent", { question: "결제 로그인 payment refund" })).data;
    await expect.poll(async () => (await probe()).gitCoverage, { timeout: 90_000, intervals: [2_000, 3_000, 5_000, 10_000] })
      .toEqual({ state: "AVAILABLE", reason: null, workingState: "FRESH" });
    // Commit sync waits for the Git identity check, so it can land after the working files.
    await expect.poll(async () => (await probe()).candidates[0]?.scores.frequency ?? 0, { timeout: 120_000, intervals: [2_000, 5_000, 10_000] })
      .toBeGreaterThan(0);
  });

  await test.step("Git evidence scores without a role match; role tags still match by tokens", async () => {
    const roleOnly = await ask("role tag by tokens", "로그인 오류를 고쳐줄 사람");
    expect(roleOnly.status).toBe("MATCHED");
    expect(roleOnly.candidates).toHaveLength(1);
    expect(roleOnly.candidates[0]).toMatchObject({ agent: "B", relatedFiles: [] });
    expect(roleOnly.candidates[0].scores).toMatchObject({ git: 0, working: 0, frequency: 0, recency: 0 });
    // With the optional model, "결제" is asked for these questions too; an extra match is reported, not fatal.
    const roleCheck = decisionModel ? expect.soft : expect;
    roleCheck(roleOnly.candidates[0].matchedRoles).toEqual(["로그인"]);
    roleCheck(roleOnly.candidates[0].scores.role).toBe(20);

    const gitBacked = await ask("cross-language with Git evidence", "payment 환불이 실패해요. refund 처리 누가 담당해?");
    expect(gitBacked.status).toBe("MATCHED");
    expect(gitBacked.candidates).toHaveLength(1);
    const [candidate] = gitBacked.candidates;
    expect(candidate.agent).toBe("B");
    // Working files rank first; the commit touched both payment files, the login file shares no token.
    expect(candidate.relatedFiles).toEqual(["src/payment/RefundPolicy.java [WORKING]", "src/payment/RefundService.java [WORKING+COMMIT]",
      "src/payment/PaymentController.java [COMMIT]"]);
    expect(candidate.scores.working).toBe(15);
    expect(candidate.scores.frequency).toBe(3);
    // 10 × 2^(-days/14) for a commit made seconds ago.
    expect(candidate.scores.recency).toBeGreaterThan(9.9);
    expect(candidate.scores.recency).toBeLessThanOrEqual(10);
    expect(candidate.latestCommitAt).not.toBeNull();
    expect(candidate.workingObservedAt).not.toBeNull();
    if (!decisionModel) {
      // Tokens alone: "결제" does not appear in the question, so the score is Git only.
      expect(candidate.matchedRoles).toEqual([]);
      expect(candidate.scores.role).toBe(0);
      expect(candidate.score).toBe(candidate.scores.git);
    }

    // Role and Git together: the tag matches by tokens and "login" matches the committed path.
    const both = await ask("role tag and Git evidence", "로그인 login 컨트롤러 오류");
    expect(both.status).toBe("MATCHED");
    expect(both.candidates).toHaveLength(1);
    expect(both.candidates[0]).toMatchObject({ agent: "B", relatedFiles: ["src/auth/LoginController.java [COMMIT]"] });
    expect(both.candidates[0].scores).toMatchObject({ working: 0, frequency: 3 });
    expect(both.candidates[0].score).toBeGreaterThan(32.9);
    roleCheck(both.candidates[0].matchedRoles).toEqual(["로그인"]);
    roleCheck(both.candidates[0].scores.role).toBe(20);
  });

  await test.step("questions the tokens and paths cannot connect", async () => {
    const crossLanguage = await ask("cross-language, no Git evidence", "sign-in 버튼을 눌러도 반응이 없어요");
    const synonym = await ask("synonym, no Git evidence", "카드 승인이 자꾸 거절돼요");
    const unrelated = await ask("unrelated", "점심 메뉴 추천해줘");
    if (!decisionModel) {
      for (const observation of [crossLanguage, synonym, unrelated]) {
        expect(observation.status, observation.label).toBe("INSUFFICIENT_EVIDENCE");
        expect(observation.insufficientReason).toBe("NO_MATCHING_EVIDENCE");
        expect(observation.candidates.map((c) => [c.agent, c.score])).toEqual([["B", 0]]);
      }
      return;
    }
    // A real model's answers are recorded; a miss is reported without hiding the rest.
    const gitBacked = observations.find((o) => o.label === "cross-language with Git evidence")!;
    expect.soft(gitBacked.candidates[0]?.matchedRoles, "payment/refund → 결제").toEqual(["결제"]);
    expect.soft(crossLanguage.status, "sign-in → 로그인").toBe("MATCHED");
    expect.soft(crossLanguage.candidates[0]?.matchedRoles).toEqual(["로그인"]);
    expect.soft(synonym.candidates[0]?.matchedRoles, "카드 승인 → 결제").toEqual(["결제"]);
    expect.soft(unrelated.status, "an unrelated question stays unmatched").toBe("INSUFFICIENT_EVIDENCE");
  });

  const report = { decisionModel, observations };
  await writeFile(join(stack.runRoot, "s18-git-evidence-observation.json"), JSON.stringify(report, null, 2));
  test.info().annotations.push({ type: "observation:s18-git-evidence", description: JSON.stringify(report) });
});
