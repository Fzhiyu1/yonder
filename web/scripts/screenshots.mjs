// Builds nothing: run `pnpm build` first. Starts `vite preview` on a free port, captures the
// mock UI at phone and desktop sizes in light and dark, then stops the server.
import { spawn } from 'node:child_process';
import { mkdirSync } from 'node:fs';
import { createServer } from 'node:net';
import { homedir } from 'node:os';
import { join } from 'node:path';
import { chromium } from 'playwright';

const OUT = process.env.SHOTS_DIR ?? join(homedir(), 'run/tmp/20260927-yonder-web');
mkdirSync(OUT, { recursive: true });
const webDir = new URL('..', import.meta.url).pathname;

function freePort() {
  return new Promise((resolve, reject) => {
    const s = createServer();
    s.listen(0, '127.0.0.1', () => {
      const { port } = s.address();
      s.close(() => resolve(port));
    });
    s.on('error', reject);
  });
}

async function waitFor(url, ms = 20_000) {
  const end = Date.now() + ms;
  while (Date.now() < end) {
    try {
      const r = await fetch(url);
      if (r.ok) return;
    } catch {
      /* not up yet */
    }
    await new Promise((r) => setTimeout(r, 200));
  }
  throw new Error(`server did not start: ${url}`);
}

const port = await freePort();
const server = spawn(join(webDir, 'node_modules/.bin/vite'), ['preview', '--port', String(port), '--strictPort', '--host', '127.0.0.1'], {
  cwd: webDir,
  stdio: ['ignore', 'pipe', 'inherit'],
  detached: true,
});
let stopped = false;
const stop = () => {
  if (stopped) return;
  stopped = true;
  try {
    process.kill(-server.pid, 'SIGTERM');
  } catch {
    /* already gone */
  }
};
process.on('exit', stop);
process.on('SIGINT', () => {
  stop();
  process.exit(130);
});

const base = `http://127.0.0.1:${port}`;
const MAC = 'Z1hHL-dorBeR1UaSDh1I0MDRKj5QeMs-7E57uB2IAV0';
const shots = [];
const problems = [];

/** Flags visible text elements that overflow their box or overlap siblings. */
async function checkLayout(page, name) {
  const issues = await page.evaluate(() => {
    const out = [];
    const vw = window.innerWidth;
    for (const el of document.querySelectorAll('button, a, span, h1, h2, dt, dd, td, th, label, div')) {
      const cs = getComputedStyle(el);
      if (cs.visibility === 'hidden' || cs.display === 'none') continue;
      const r = el.getBoundingClientRect();
      if (!r.width || !r.height) continue;
      const hasText = [...el.childNodes].some((n) => n.nodeType === 3 && n.textContent.trim());
      if (!hasText) continue;
      if (r.right > vw + 1 && cs.position !== 'fixed' && !el.closest('.overflow-x-auto, .overflow-auto, table, pre, .xterm')) {
        out.push(`offscreen: "${el.textContent.trim().slice(0, 40)}" right=${Math.round(r.right)}`);
      }
      const clip = cs.overflow === 'hidden' || cs.textOverflow === 'ellipsis';
      if (!clip && el.scrollWidth > el.clientWidth + 2 && cs.whiteSpace === 'nowrap' && !el.closest('.overflow-x-auto, .overflow-auto, pre, .xterm')) {
        out.push(`overflow: "${el.textContent.trim().slice(0, 40)}" ${el.scrollWidth}>${el.clientWidth}`);
      }
    }
    return out.slice(0, 10);
  });
  if (issues.length) problems.push(`${name}: ${issues.join(' | ')}`);
}

async function snap(page, name) {
  await page.waitForTimeout(350);
  const path = join(OUT, `${name}.png`);
  await page.screenshot({ path });
  await checkLayout(page, name);
  shots.push(path);
}

async function run(browser, { label, width, height, mobile, scheme }) {
  const ctx = await browser.newContext({
    viewport: { width, height },
    deviceScaleFactor: 2,
    colorScheme: scheme,
    isMobile: mobile,
    hasTouch: mobile,
    userAgent: mobile
      ? 'Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Mobile/15E148 Safari/604.1'
      : undefined,
  });
  const page = await ctx.newPage();
  const errors = [];
  page.on('pageerror', (e) => errors.push(String(e)));
  page.on('console', (m) => m.type() === 'error' && errors.push(m.text()));
  const p = `${label}-${scheme}`;
  const go = async (hash) => {
    await page.goto(`${base}/?mock=1${hash}`);
    await page.waitForSelector('#root aside, #root main', { timeout: 10_000 });
    await page.waitForTimeout(300);
  };

  // 1. List (mobile: sidebar is the home screen; desktop: sidebar + empty main).
  await go('#/');
  await page.waitForSelector('text=修复锁屏后中继连接假在线');
  await snap(page, `${p}-01-list`);

  // 2. Chat with a pending approval + command cards.
  await go(`#/h/${MAC}/s/s_chat_relay`);
  await page.waitForSelector('text=运行中继集成测试');
  await page.getByText('rg -n "interval|ping" crates/yonder-relay/src').click();
  await snap(page, `${p}-02-chat-approval`);

  // 2b. Approve -> command streams output, agent finishes.
  await page.getByRole('button', { name: '允许', exact: true }).click();
  await page.waitForSelector('text=7 个测试全部通过', { timeout: 15_000 });
  await page.waitForTimeout(1200);
  const jump = page.getByRole('button', { name: '新消息' });
  if (await jump.isVisible()) await jump.click();
  await page.waitForTimeout(500);
  await snap(page, `${p}-03-chat-after-approval`);

  // 3. Terminal (+ key bar on mobile).
  await go(`#/h/${MAC}/s/s_term_zsh`);
  await page.waitForSelector('.xterm-rows');
  await page.waitForTimeout(400);
  await page.locator('.term-host').click();
  await page.keyboard.type('ls');
  await page.keyboard.press('Enter');
  await page.waitForTimeout(300);
  await snap(page, `${p}-04-terminal`);

  // 4. New session dialog.
  await go('#/');
  await page.getByRole('button', { name: '新建会话' }).first().click();
  await page.waitForSelector('text=工作目录');
  await page.waitForTimeout(500);
  await snap(page, `${p}-05-new-session`);
  await page.keyboard.press('Escape');

  // 5. File manager.
  await go(`#/h/${MAC}/files`);
  await page.waitForSelector('text=Downloads');
  await snap(page, `${p}-06-files`);

  // 6. Settings.
  await go('#/settings');
  await page.waitForSelector('text=已配对设备');
  await page.waitForSelector('text=当前浏览器');
  await snap(page, `${p}-07-settings`);

  if (errors.length) problems.push(`${p} console: ${errors.slice(0, 5).join(' | ')}`);
  await ctx.close();
}

let browser;
let code = 0;
try {
  await waitFor(base + '/');
  browser = await chromium.launch();
  for (const scheme of ['light', 'dark']) {
    await run(browser, { label: 'mobile', width: 390, height: 844, mobile: true, scheme });
    await run(browser, { label: 'desktop', width: 1440, height: 900, mobile: false, scheme });
  }
  console.log(`captured ${shots.length} screenshots in ${OUT}`);
  if (problems.length) {
    console.log('layout/console findings:');
    for (const p of problems) console.log(' - ' + p);
  }
} catch (err) {
  console.error(err);
  code = 1;
} finally {
  await browser?.close();
  stop();
}
process.exit(code);
