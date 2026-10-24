import { defineConfig } from "@playwright/test";
import path from "node:path";
import { mkdirSync } from "node:fs";

const external = Boolean(process.env.PLAYWRIGHT_BASE_URL);
const artifacts =
  process.env.TEST_ARTIFACTS_DIR ?? path.resolve(".fixture-artifacts");
if (!external) mkdirSync(artifacts, { recursive: true });
process.env.TEST_ARTIFACTS_DIR = artifacts;
export default defineConfig({
  testDir: "./tests",
  testMatch: "**/*.spec.js",
  fullyParallel: false,
  workers: 1,
  timeout: 30_000,
  outputDir: process.env.PLAYWRIGHT_OUTPUT_DIR ?? "test-results",
  reporter: [
    ["list"],
    [
      "html",
      {
        outputFolder: process.env.PLAYWRIGHT_REPORT_DIR ?? "playwright-report",
        open: "never",
      },
    ],
  ],
  use: {
    baseURL: process.env.PLAYWRIGHT_BASE_URL ?? "http://127.0.0.1:4173",
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
  },
  webServer: external
    ? undefined
    : {
        command: "npm run build && npm start",
        url: "http://127.0.0.1:4173/api/runs",
        env: { ARTIFACTS_DIR: artifacts, HOST: "127.0.0.1", PORT: "4173" },
        reuseExistingServer: false,
      },
});
