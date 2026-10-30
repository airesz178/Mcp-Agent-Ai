import { chromium } from 'playwright';
import { mkdtemp, mkdir, rm } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { existsSync, readdirSync } from 'node:fs';
import { startServer } from '../src/server.mjs';

const projectRoot = fileURLToPath(new URL('..', import.meta.url));
const directory = await mkdtemp(path.join(os.tmpdir(), 'cos-fusion-ui-'));
const screenshotFolder = path.join(projectRoot, 'work', 'ui-qa');
await mkdir(screenshotFolder, { recursive: true });
await mkdir(path.join(directory, 'workspace'));
const app = await startServer({ home: path.join(directory, 'data'), workspaceRoot: path.join(directory, 'workspace'), openBrowser: false });
let browser;
let checks = 0;
function ensure(value, message) {
  if (!value) throw new Error(`UI QA failed: ${message}`);
  checks++;
}
async function pageGeometry(page, label) {
  const result = await page.evaluate(() => ({
    documentWidth: document.documentElement.scrollWidth,
    screenWidth: window.innerWidth,
    composer: (() => { const rect = document.getElementById('prompt').getBoundingClientRect(); return { left: rect.left, right: rect.right, bottom: rect.bottom }; })(),
    titleVisible: (() => { const rect = document.querySelector('.welcome h1').getBoundingClientRect(); return rect.width > 0; })(),
  }));
  ensure(result.documentWidth <= result.screenWidth + 1, `${label}: page horizontally overflows by ${result.documentWidth - result.screenWidth}px`);
  ensure(result.composer.left >= 0 && result.composer.right <= result.screenWidth, `${label}: composer goes off-screen`);
  ensure(result.titleVisible, `${label}: title is missing`);
  console.log(`${label}: ${result.screenWidth}px viewport, ${result.documentWidth}px document, composer right=${result.composer.right.toFixed(0)}`);
}

try {
  // Reuse a locally installed Playwright Chromium when npm and browser revisions differ.
  const browsersRoot = path.join(os.homedir(), 'AppData', 'Local', 'ms-playwright');
  const browserPaths = existsSync(browsersRoot)
    ? readdirSync(browsersRoot).filter(folder => /^chromium-\d+$/.test(folder))
      .map(folder => path.join(browsersRoot, folder, 'chrome-win64', 'chrome.exe'))
    : [];
  const existingChromium = browserPaths.find(candidate => existsSync(candidate));
  browser = await chromium.launch({ ...(existingChromium ? { executablePath: existingChromium } : {}), headless: true, args: ['--disable-extensions'] });
  const desktop = await browser.newPage({ viewport: { width: 1440, height: 900 }, deviceScaleFactor: 1 });
  const errors = [];
  desktop.on('pageerror', error => errors.push(error.message));
  await desktop.goto(app.url);
  await desktop.locator('#threadCount').waitFor();
  await pageGeometry(desktop, 'Desktop 1440x900');
  await desktop.screenshot({ path: path.join(screenshotFolder, 'desktop-1440.png'), fullPage: true });
  await desktop.locator('#settingsToggle').click();
  ensure(await desktop.locator('#settingsPanel').isVisible(), 'desktop customize panel did not open');
  await desktop.locator('#modules input[type="checkbox"]').first().check();
  ensure((await desktop.evaluate(async () => (await fetch('/api/state')).json())).modules.workspace, 'workspace module did not enable');
  await desktop.locator('#skillName').fill('qa-skill');
  await desktop.locator('#skillText').fill('# QA guidance\nAlways verify output');
  await desktop.locator('#skillForm button').click();
  await desktop.getByText('qa-skill', { exact: true }).waitFor({ timeout: 5000 });
  ensure(await desktop.getByText('qa-skill', { exact: true }).isVisible(), 'skill import failed');
  await desktop.locator('#settingsClose').click();
  ensure(!(await desktop.locator('#settingsPanel').isVisible()), 'desktop customize panel did not close');
  await desktop.locator('#prompt').fill('/list');
  await desktop.locator('#prompt').press('Enter');
  await desktop.getByText('Demo tool result:', { exact: false }).waitFor({ timeout: 15000 });
  ensure(await desktop.locator('#threads button').count() === 1, 'first conversation missing from history');
  ensure(await desktop.locator('.message').count() === 2, 'message list should show user and assistant');
  await desktop.locator('#newChat').click();
  ensure(await desktop.locator('.welcome h1').isVisible(), 'new chat did not restore welcome');
  await desktop.locator('#threads button').first().click();
  ensure(await desktop.locator('.message').count() === 2, 'history did not restore conversation');
  await desktop.keyboard.press('Control+k');
  ensure(await desktop.locator('.welcome h1').isVisible(), 'Ctrl+K new conversation shortcut failed');
  ensure(errors.length === 0, `desktop JavaScript errors: ${errors.join('; ')}`);
  await desktop.screenshot({ path: path.join(screenshotFolder, 'desktop-after-qa.png'), fullPage: true });

  const mobile = await browser.newPage({ viewport: { width: 390, height: 844 }, deviceScaleFactor: 1, isMobile: true, hasTouch: true });
  mobile.on('pageerror', error => errors.push(error.message));
  await mobile.goto(app.url);
  await pageGeometry(mobile, 'Mobile 390x844');
  ensure(await mobile.locator('#sidebarToggle').isVisible(), 'mobile navigation button is missing');
  await mobile.locator('#sidebarToggle').click();
  ensure(await mobile.locator('#sidebar').evaluate(node => node.classList.contains('open')), 'mobile drawer failed to open');
  await mobile.locator('#sidebarBackdrop').click({ position: { x: 380, y: 120 } });
  ensure(!(await mobile.locator('#sidebar').evaluate(node => node.classList.contains('open'))), 'mobile drawer did not close');
  await mobile.locator('#newChatTop').click();
  ensure(await mobile.locator('.welcome h1').isVisible(), 'mobile New button failed');
  await mobile.locator('#settingsToggle').click();
  ensure(await mobile.locator('#settingsPanel').isVisible(), 'mobile customize did not open');
  await mobile.keyboard.press('Escape');
  ensure(!(await mobile.locator('#settingsPanel').isVisible()), 'Escape failed to close settings');
  await mobile.screenshot({ path: path.join(screenshotFolder, 'mobile-390.png'), fullPage: true });
  await mobile.setViewportSize({ width: 375, height: 600 });
  await pageGeometry(mobile, 'Mobile 375x600');
  ensure(errors.length === 0, `mobile JavaScript errors: ${errors.join('; ')}`);
  console.log(`UI QA PASS: ${checks} assertions. Screenshots: ${screenshotFolder}`);
} finally {
  if (browser) await browser.close();
  await app.close();
  await rm(directory, { recursive: true, force: true });
}
