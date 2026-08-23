import { defineConfig } from "@playwright/test";

if (process.env.CI) {
  throw new Error("Inspector browser measurements are local-only and must not run in CI");
}

const host = "127.0.0.1";
const port = 4180;
const baseURL = `http://${host}:${port}`;

export default defineConfig({
  testDir: "./tests/measure",
  fullyParallel: false,
  forbidOnly: true,
  retries: 0,
  workers: 1,
  reporter: "line",
  outputDir: "test-results/measure",
  timeout: 120_000,
  use: {
    baseURL,
    headless: true,
    trace: "off",
    screenshot: "off",
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
  projects: [{ name: "chromium-measurement", use: { browserName: "chromium" } }]
});
