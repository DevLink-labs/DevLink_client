import { defineConfig, devices } from "@playwright/test";

// On failure Playwright writes an aria snapshot (input values included) into error-context.md after our
// fixtures' secret scan. This switch only skips the snapshot taken when a context closes; a failing
// locator assertion still attaches its own. Those are handled by clearSecrets() after each submit and
// by the runner's digest scan (see README "비밀정보").
process.env.PLAYWRIGHT_NO_COPY_PROMPT = "1";

// Spec files run in parallel workers against the one stack global setup starts. Each scenario uses
// its own central users, team, actor directories and project folders, so they share no account or
// per-target lock. Tests inside a file stay serial. E2E_WORKERS overrides the count; the real-Host
// turns (E2E_REAL_HOSTS=1) measure owner-release timing and stay on one worker.
const requestedWorkers = Number(process.env.E2E_WORKERS ?? "3");
const workers = process.env.E2E_REAL_HOSTS === "1" ? 1
  : Number.isInteger(requestedWorkers) && requestedWorkers >= 1 ? requestedWorkers : 1;

export default defineConfig({
  testDir: "./specs",
  outputDir: "./test-results",
  globalSetup: "./src/stack/global-setup.ts",
  fullyParallel: false,
  workers,
  retries: 0,
  forbidOnly: !!process.env.CI,
  timeout: 240_000,
  expect: { timeout: 10_000 },
  reporter: [["list"], ["html", { outputFolder: "playwright-report", open: "never" }]],
  use: {
    ...devices["Desktop Chrome"],
    viewport: { width: 1440, height: 960 },
    locale: "ko-KR",
    timezoneId: "Asia/Seoul",
    reducedMotion: "reduce",
    actionTimeout: 10_000,
    navigationTimeout: 20_000,
    // Traces record fill() arguments and login bodies (passwords), which the secret scan cannot
    // inspect inside zips. Opt in with E2E_TRACE=1 when debugging and delete them afterwards.
    trace: process.env.E2E_TRACE === "1" ? "retain-on-failure" : "off",
    video: "retain-on-failure",
    screenshot: "only-on-failure",
  },
  projects: [{ name: "chromium", use: { browserName: "chromium" } }],
});
