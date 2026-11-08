import { expect, type Locator, type Page } from "@playwright/test";

/**
 * Screen object for the web app. The frontend has no data-testid yet (DevLink_frontend#15), so it uses
 * role/label selectors scoped to dialogs, the sidebar and panels. A three.js scene sits above the
 * dashboard; every action first brings #team-dashboard into view. The WebGL fallback ("기본 그래픽")
 * renders the same dashboard, so nothing here depends on the scene itself.
 */
export class WorkspacePage {
  constructor(readonly page: Page, private readonly frontendUrl: string) {}

  get sidebar(): Locator {
    return this.page.getByRole("complementary", { name: "워크스페이스 메뉴" });
  }

  get teamPicker(): Locator {
    return this.page.getByRole("combobox", { name: "현재 팀 변경" });
  }

  async open() {
    await this.page.goto(this.frontendUrl);
  }

  async signup(user: { name: string; email: string; password: string }) {
    await this.open();
    await expect(this.page.getByRole("heading", { name: "로그인", exact: true })).toBeVisible();
    await this.page.locator(".auth-mode-switch").getByRole("button", { name: "회원가입", exact: true }).click();
    await expect(this.page.getByRole("heading", { name: "회원가입", exact: true })).toBeVisible();
    await this.page.getByLabel("이름", { exact: true }).fill(user.name);
    await this.page.getByLabel("이메일", { exact: true }).fill(user.email);
    await fillSecret(this.page.getByLabel("비밀번호", { exact: true }), user.password);
    await this.page.getByRole("button", { name: "계정 만들기", exact: true }).click();
    // The app clears the password after signup. Poll its length (a plain value, so a failure carries no
    // page snapshot) before any locator assertion, then wipe it anyway if the app did not.
    try {
      await expect.poll(() => secretLength(this.page), { timeout: 10_000, message: "password field is cleared after signup" }).toBe(0);
    } finally {
      await clearSecrets(this.page);
    }
    // Signup does not sign in: the form returns to login and keeps the email.
    await expect(this.page.getByRole("heading", { name: "로그인", exact: true })).toBeVisible();
    await expect(this.page.getByLabel("이메일", { exact: true })).toHaveValue(user.email);
  }

  /** The signed-in web user, read with this browser context's session cookie. */
  async webUserId(): Promise<number> {
    const response = await this.page.request.get("/api/v1/auth/users/me");
    expect(response.ok()).toBe(true);
    return ((await response.json()) as { data: { userId: number } }).data.userId;
  }

  async login(user: { email: string; password: string }) {
    if (!(await this.page.getByLabel("이메일", { exact: true }).isVisible().catch(() => false))) await this.open();
    await this.page.getByLabel("이메일", { exact: true }).fill(user.email);
    await fillSecret(this.page.getByLabel("비밀번호", { exact: true }), user.password);
    await this.page.getByRole("button", { name: "로그인", exact: true }).click();
    // The submit handler already captured the value; a failed login keeps it in the field, where a
    // failing locator assertion would copy it into error-context.md.
    await clearSecrets(this.page);
    await this.showDashboard();
  }

  async showDashboard() {
    const dashboard = this.page.locator("#team-dashboard");
    await expect(dashboard).toBeAttached({ timeout: 20_000 });
    await dashboard.scrollIntoViewIfNeeded();
    await expect(this.sidebar).toBeVisible();
  }

  async go(view: "대시보드" | "Agent" | "팀 관리") {
    await this.showDashboard();
    await this.sidebar.getByRole("navigation", { name: "주 메뉴" }).getByRole("button", { name: view, exact: true }).click();
  }

  /** Creates a team from the teams panel and returns its ID and the invite code shown once. */
  async createTeam(name: string, onSecret: (value: string) => void): Promise<{ teamId: number; inviteCode: string }> {
    await this.go("팀 관리");
    await this.page.locator(".team-panel .section-actions").getByRole("button", { name: "팀 만들기" }).click();
    const form = this.page.getByRole("dialog", { name: "새 팀 만들기" });
    await form.getByLabel("팀 이름", { exact: true }).fill(name);
    await form.getByRole("button", { name: "팀 만들기", exact: true }).click();
    const created = this.page.getByRole("dialog", { name: "팀을 만들었습니다" });
    await expect(created).toBeVisible();
    const inviteCode = await created.getByLabel("초대 코드", { exact: true }).inputValue();
    onSecret(inviteCode); // register before any later step can fail and leave it in artifacts
    expect(inviteCode.length, "invite code is shown").toBeGreaterThan(0);
    await created.getByRole("button", { name: "닫기", exact: true }).click();
    await expect(created).toBeHidden();
    await expect(this.teamPicker.locator("option:checked")).toHaveText(name);
    const teamId = Number(await this.teamPicker.inputValue());
    expect(Number.isSafeInteger(teamId) && teamId > 0).toBe(true);
    return { teamId, inviteCode };
  }

  async joinTeam(inviteCode: string, teamName: string) {
    await this.go("팀 관리");
    await this.page.locator(".team-panel .section-actions").getByRole("button", { name: "코드로 참여" }).click();
    const form = this.page.getByRole("dialog", { name: "초대 코드로 참여" });
    await fillSecret(form.getByLabel("초대 코드", { exact: true }), inviteCode);
    await form.getByRole("button", { name: "팀에 참여하기", exact: true }).click();
    await clearSecrets(this.page);
    await expect(form).toBeHidden();
    await expect(this.teamPicker.locator("option:checked")).toHaveText(teamName);
  }

  memberRow(name: string): Locator {
    return this.page.locator(".team-panel .member-list .member-row").filter({ hasText: name });
  }

  /** Re-reads team data with the panel button until `check` passes (no polling interval exists for members). */
  async expectMember(name: string) {
    await this.go("팀 관리");
    await expect(async () => {
      await this.page.getByRole("button", { name: "팀 정보 새로고침" }).click();
      await expect(this.memberRow(name)).toBeVisible({ timeout: 2_000 });
    }).toPass({ timeout: 15_000 });
  }

  agentRow(name: string): Locator {
    return this.page.locator(".agents-panel tbody tr").filter({ has: this.page.getByRole("button", { name, exact: true }) });
  }

  /**
   * Agent presence is not polled by the web app (DevLink_frontend#16), so refresh the list until each
   * agent shows the expected label.
   */
  async expectAgentStatus(expected: Record<string, "온라인" | "오프라인">, timeout = 20_000) {
    await this.go("Agent");
    await expect(async () => {
      await this.page.getByRole("button", { name: "Agent 목록 새로고침" }).click();
      for (const [name, status] of Object.entries(expected)) {
        await expect(this.agentRow(name).locator(".badge-online, .badge-offline")).toHaveText(status, { timeout: 2_000 });
      }
    }).toPass({ timeout });
  }

  /** Overview statistic "온라인 Agent" on the dashboard, refreshed until it matches. */
  async expectOverviewOnlineCount(count: string, timeout = 20_000) {
    await this.go("대시보드");
    const value = this.page.locator('dl[aria-label="워크스페이스 현황"] > div')
      .filter({ has: this.page.locator("dt", { hasText: /^온라인 Agent$/ }) }).locator("dd");
    await expect(async () => {
      await this.page.getByRole("button", { name: "전체 새로고침" }).click();
      await expect(value).toHaveText(count, { timeout: 2_000 });
    }).toPass({ timeout });
  }

  /** Waits for each agent to be listed (refreshing), then reads its ID from the detail dialog. */
  async agentIdsFromList(names: string[], timeout = 20_000): Promise<Record<string, string>> {
    await this.go("Agent");
    await expect(async () => {
      await this.page.getByRole("button", { name: "Agent 목록 새로고침" }).click();
      for (const name of names) await expect(this.agentRow(name)).toBeVisible({ timeout: 2_000 });
    }).toPass({ timeout });
    const ids: Record<string, string> = {};
    for (const name of names) {
      await this.agentRow(name).getByRole("button", { name, exact: true }).click();
      const detail = this.page.getByRole("dialog", { name: "Agent 상세 정보" });
      const id = detail.locator(".detail-item").filter({ hasText: "Agent ID" }).locator("dd");
      await expect(id).toHaveText(/^#[1-9][0-9]*$/);
      ids[name] = (await id.innerText()).slice(1);
      await detail.getByRole("button", { name: "닫기", exact: true }).click();
      await expect(detail).toBeHidden();
    }
    return ids;
  }

  /** Opens a task conversation from the sidebar "최근 채팅" list, refreshing until it appears. */
  async openConversation(taskId: string, title: string): Promise<Locator> {
    await this.showDashboard();
    const chats = this.sidebar.getByRole("region", { name: "최근 채팅" });
    // The broker sends no taskSummary today, so the list shows "<id>번 세션"; anchor it so 1 ≠ 11.
    // `has` is resolved inside each item, so the inner locator must not start from the sidebar.
    const name = this.page.locator(".dashboard-chat-title strong", { hasText: new RegExp(`^(?:${escapeRegExp(title)}|${taskId}번 세션)$`) });
    const item = chats.locator(".dashboard-chat-item").filter({ has: name });
    await expect(async () => {
      await this.page.getByRole("button", { name: "전체 새로고침" }).click();
      await expect(item).toBeVisible({ timeout: 2_000 });
    }).toPass({ timeout: 15_000 });
    await item.click();
    const conversation = this.page.getByRole("region", { name: "작업 대화 기록" });
    await expect(conversation.getByText(`#${taskId}`, { exact: true })).toBeVisible();
    return conversation;
  }
}

/**
 * Types a secret without putting it in a Playwright step title: fill()/type()/insertText() record
 * their argument (`Fill "<value>"`) in the HTML report and error call logs. evaluate() is reported
 * as "Evaluate" only. Uses the native setter so React's controlled input sees the change.
 */
export async function fillSecret(input: Locator, value: string) {
  await input.click();
  await input.evaluate((element, secret) => {
    const field = element as HTMLInputElement;
    const setter = Object.getOwnPropertyDescriptor(Object.getPrototypeOf(field), "value")!.set!;
    setter.call(field, secret);
    field.dispatchEvent(new Event("input", { bubbles: true }));
  }, value);
  const length = await input.evaluate((element) => (element as HTMLInputElement).value.length);
  expect(length, "secret field received the whole value").toBe(value.length);
}

const SECRET_INPUTS = 'input[type="password"], input.code-input';

async function secretLength(page: Page): Promise<number> {
  return page.evaluate((selector) => [...document.querySelectorAll<HTMLInputElement>(selector)].reduce((sum, input) => sum + input.value.length, 0), SECRET_INPUTS);
}

/**
 * Empties password and invite-code inputs right after their form was submitted (the handler has
 * already read the value). Runs immediately without waiting for elements that may have unmounted.
 */
export async function clearSecrets(page: Page) {
  await page.evaluate((selector) => {
    for (const input of document.querySelectorAll<HTMLInputElement>(selector)) {
      if (!input.value) continue;
      Object.getOwnPropertyDescriptor(Object.getPrototypeOf(input), "value")!.set!.call(input, "");
      input.dispatchEvent(new Event("input", { bubbles: true }));
    }
  }, SECRET_INPUTS);
}

export function escapeRegExp(value: string) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}
