import { defineConfig, devices } from "@playwright/test";

export default defineConfig({
  testDir: "./tests",
  fullyParallel: true,
  use: {
    baseURL: "http://127.0.0.1:4321",
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
    launchOptions: {
      executablePath: process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH,
    },
  },
  projects: [
    {
      name: "desktop",
      use: {
        ...devices["Desktop Chrome"],
        viewport: { width: 1440, height: 1000 },
      },
    },
    {
      name: "mobile",
      use: { ...devices["iPhone 13"], defaultBrowserType: "chromium" },
    },
    {
      name: "reduced-motion",
      use: { ...devices["Desktop Chrome"], reducedMotion: "reduce" },
    },
  ],
  webServer: {
    command: "npm run preview -- --port 4321",
    env: { ASTRO_PREVIEW_BACKGROUND: "1" },
    url: "http://127.0.0.1:4321",
    reuseExistingServer: !process.env.CI,
  },
});
