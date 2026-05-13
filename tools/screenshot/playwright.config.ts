import { defineConfig, devices } from '@playwright/test';

// We spawn ferrited ourselves inside the test fixture (e2e/fixtures.ts)
// rather than using Playwright's webServer hook — that hook can't pick
// a random port at run time and the helper has more interesting cleanup.
export default defineConfig({
  testDir: './e2e',
  fullyParallel: false, // each test spawns a SoapySource-capable backend; serialise to avoid USB/RAM contention
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 1 : 0,
  workers: 1,
  reporter: process.env.CI ? [['list'], ['html', { open: 'never' }]] : 'list',
  outputDir: 'test-results',
  use: {
    actionTimeout: 10_000,
    navigationTimeout: 30_000,
    trace: 'retain-on-failure',
    screenshot: 'only-on-failure',
    video: 'retain-on-failure',
    launchOptions: {
      args: [
        '--autoplay-policy=no-user-gesture-required',
        '--use-gl=swiftshader',
      ],
    },
  },
  projects: [
    {
      name: 'chromium',
      use: { ...devices['Desktop Chrome'], viewport: { width: 1600, height: 1000 } },
    },
  ],
});
