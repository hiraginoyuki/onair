import { defineConfig } from "@playwright/test";

const host = "127.0.0.1";
const port = 4179;
const baseURL = `http://${host}:${port}`;

export default defineConfig({
  testDir: "./tests/browser",
  fullyParallel: false,
  forbidOnly: Boolean(process.env.CI),
  retries: 0,
  workers: process.env.CI ? 1 : undefined,
  reporter: "line",
  outputDir: "test-results/playwright",
  expect: { timeout: 5_000 },
  use: {
    baseURL,
    headless: true,
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
    video: "off"
  },
  webServer: {
    command: "node tests/browser/server.mjs",
    url: `${baseURL}/_onair/inspector-next`,
    reuseExistingServer: false,
    timeout: 30_000,
    stdout: "ignore",
    stderr: "pipe",
    gracefulShutdown: { signal: "SIGTERM", timeout: 500 },
    env: {
      INSPECTOR_BROWSER_TEST_HOST: host,
      INSPECTOR_BROWSER_TEST_PORT: String(port)
    }
  },
  projects: [{ name: "chromium", use: { browserName: "chromium" } }]
});
