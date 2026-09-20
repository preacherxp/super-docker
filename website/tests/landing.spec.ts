import { readFileSync } from "node:fs";
import { expect, test } from "@playwright/test";
import AxeBuilder from "@axe-core/playwright";

test("hero shows the real capture and opens it at full size", async ({
  page,
}) => {
  await page.goto("/");
  const capture = page.locator(".terminal-capture img");
  await expect(capture).toBeVisible();
  await expect(capture).toHaveAttribute("alt", /expanded Containers panel/);
  await page.waitForFunction(
    () =>
      (document.querySelector(".terminal-capture img") as HTMLImageElement)
        ?.naturalWidth > 0,
  );
  const source = await capture.getAttribute("src");
  const response = await page.request.get(source!);
  expect(await response.body()).toEqual(
    readFileSync(new URL("../../docs/demo.png", import.meta.url)),
  );
  await expect(page.locator('.terminal [role="tablist"]')).toHaveCount(0);
  const [enlarged] = await Promise.all([
    page.waitForEvent("popup"),
    page
      .getByRole("link", { name: /actual super-docker terminal capture/ })
      .click(),
  ]);
  await enlarged.waitForLoadState();
  expect(new URL(enlarged.url()).pathname).toBe(source);
  await enlarged.close();
});

test("install copy and permission-denied fallback work", async ({ page }) => {
  await page.addInitScript(() => {
    Object.defineProperty(navigator, "clipboard", {
      configurable: true,
      value: {
        writeText: async (text: string) => {
          (window as any).copiedCommand = text;
        },
      },
    });
  });
  await page.goto("/#install");
  await page
    .locator('astro-island[component-url*="InstallCommand"]:not([ssr])')
    .waitFor();
  await page.locator("#copy-install").click();
  expect(await page.evaluate(() => (window as any).copiedCommand)).toBe(
    "curl -fsSL https://github.com/preacherxp/super-docker/releases/latest/download/install.sh | sh",
  );
  await expect(page.getByRole("status")).toContainText("copied to clipboard");
  await expect(page.locator("#copy-install svg")).toBeVisible();
  await page.reload();
  await page
    .locator('astro-island[component-url*="InstallCommand"]:not([ssr])')
    .waitFor();
  await page.evaluate(() => {
    Object.defineProperty(navigator, "clipboard", {
      configurable: true,
      value: {
        writeText: async () => {
          throw new Error("Permission denied");
        },
      },
    });
  });
  await page.locator("#copy-install").click();
  await expect(page.getByRole("status")).toContainText("Command selected");
  expect(
    await page.evaluate(() => window.getSelection()?.toString()),
  ).toContain("curl -fsSL");
});

test("demo loads on request, closes, and returns focus", async ({ page }) => {
  await page.goto("/");
  await page
    .locator('astro-island[component-url*="DemoPlayer"]:not([ssr])')
    .waitFor();
  await expect(page.locator("#demo-media img")).toHaveCount(0);
  await page.locator("#watch-demo").click();
  await expect(page.getByRole("dialog")).toBeVisible();
  await expect(page.locator("#demo-media img")).toBeVisible();
  await page.waitForFunction(
    () =>
      (document.querySelector("#demo-media img") as HTMLImageElement)
        ?.naturalWidth > 0,
  );
  const placement = await page.getByRole("dialog").evaluate((dialog) => {
    const bounds = dialog.getBoundingClientRect();
    return {
      horizontalOffset: Math.abs(bounds.x + bounds.width / 2 - innerWidth / 2),
      verticalOffset: Math.abs(bounds.y + bounds.height / 2 - innerHeight / 2),
    };
  });
  expect(placement.horizontalOffset).toBeLessThanOrEqual(1);
  expect(placement.verticalOffset).toBeLessThanOrEqual(1);
  await page.keyboard.press("Escape");
  await expect(page.getByRole("dialog")).toBeHidden();
  await expect(page.locator("#demo-media img")).toHaveCount(0);
  await expect(page.locator("#watch-demo")).toBeFocused();
});

test("layout, feature links, and accessibility remain usable", async ({
  page,
}) => {
  const failures: string[] = [];
  page.on("response", (response) => {
    if (response.status() >= 400) failures.push(response.url());
  });
  await page.goto("/");
  await expect(page).toHaveTitle("super-docker — Docker. In your element.");
  for (const target of ["#features", "#workflow", "#install"]) {
    await page.locator(target).scrollIntoViewIfNeeded();
    await page.waitForTimeout(850);
    expect(
      await page.evaluate(
        () => document.documentElement.scrollWidth <= window.innerWidth,
      ),
    ).toBeTruthy();
  }
  await page.locator(".keymap summary").click();
  await expect(page.locator(".keymap-grid")).toBeVisible();
  await page.evaluate(() =>
    document
      .querySelectorAll(".reveal")
      .forEach((node) => node.classList.add("is-visible")),
  );
  await page.waitForTimeout(900);
  const accessibility = await new AxeBuilder({ page })
    .withTags(["wcag2a", "wcag2aa", "wcag21aa"])
    .analyze();
  expect(
    accessibility.violations.map((violation) => ({
      rule: violation.id,
      nodes: violation.nodes.map((node) => ({
        target: node.target,
        issue: node.failureSummary,
      })),
    })),
  ).toEqual([]);
  expect(failures).toEqual([]);
});

test("workflow is compact and readable without scroll-driven steps", async ({
  page,
}) => {
  await page.goto("/#workflow");
  const steps = page.locator(".workflow-step");
  await expect(steps).toHaveCount(3);
  for (const step of await steps.all()) {
    await step.scrollIntoViewIfNeeded();
    await expect(step).toBeVisible();
    await expect(step).toHaveCSS("opacity", "1");
  }
  const height = await page
    .locator("#workflow")
    .evaluate((node) => node.clientHeight);
  expect(height).toBeLessThan(900);
});
