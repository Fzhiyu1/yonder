// Mobile UX probe: walks the main phone flows of the mock UI in WebKit (iPhone 15 profile),
// counts taps, and measures touch targets, clipped controls and keyboard occlusion.
// Builds nothing: run `pnpm build` first.
//
// Keyboard: iOS overlays the soft keyboard and shrinks only the visual viewport. The probe
// stubs `window.visualViewport` to do the same (height - 336 px) and checks that the focused
// field and its action stay in the visible band.
//
// Usage: node scripts/ux-probe.mjs [outDir]
// Prints a JSON summary; screenshots go to outDir (default <tmp>/yonder-ux). Exits 1 when a
// flow fails or a check in `report.fail` is set.
import { spawn } from 'node:child_process';
import { mkdirSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:net';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { devices, webkit } from 'playwright';

const OUT = process.argv[2] ?? join(tmpdir(), 'yonder-ux');
mkdirSync(OUT, { recursive: true });
const webDir = new URL('..', import.meta.url).pathname;
const MAC = 'Z1hHL-dorBeR1UaSDh1I0MDRKj5QeMs-7E57uB2IAV0';
const LINUX = 'dGYGR2OrMJTg2mU20Z89weghdwbhLs7bnlh0M1lhq2w';
const MIN = 44;
const KB = 336;

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

const port = await freePort();
const server = spawn(join(webDir, 'node_modules/.bin/vite'), ['preview', '--port', String(port), '--strictPort', '--host', '127.0.0.1'], {
  cwd: webDir,
  stdio: ['ignore', 'ignore', 'inherit'],
  detached: true,
});
const stop = () => {
  try {
    process.kill(-server.pid, 'SIGTERM');
  } catch {
    /* gone */
  }
};
process.on('exit', stop);
const base = `http://127.0.0.1:${port}`;
for (let i = 0; i < 100; i++) {
  try {
    if ((await fetch(base)).ok) break;
  } catch {
    /* not up yet */
  }
  await new Promise((r) => setTimeout(r, 200));
}

const report = { screens: {}, flows: {}, keyboard: {}, fail: [], errors: [] };
const check = (ok, what) => {
  if (!ok) report.fail.push(what);
  return ok;
};

/** Interactive elements on screen that are smaller than the touch minimum, or cut off. */
async function audit(page, name) {
  const r = await page.evaluate((MIN) => {
    const vw = innerWidth;
    const vh = window.visualViewport ? window.visualViewport.offsetTop + window.visualViewport.height : innerHeight;
    const sel =
      'button, a[href], input:not([type=hidden]), select, textarea, [role=button], [role=radio], [role=menuitem], [role=menuitemradio], [role=switch], [role=option], [role=tab]';
    const small = [];
    const offscreen = [];
    const seen = new Set();
    for (const el of document.querySelectorAll(sel)) {
      const cs = getComputedStyle(el);
      if (cs.visibility === 'hidden' || cs.display === 'none' || el.closest('[hidden], .xterm-helpers, [aria-hidden=true]')) continue;
      const b = el.getBoundingClientRect();
      if (!b.width || !b.height) continue;
      if (b.bottom < 0 || b.top > vh) continue;
      // Controls inside a horizontally scrolled strip that are scrolled out of view.
      const strip = el.parentElement?.closest('.overflow-x-auto');
      if (strip) {
        const s = strip.getBoundingClientRect();
        if (b.right <= s.left + 1 || b.left >= s.right - 1) continue;
      }
      const label = (el.getAttribute('aria-label') || el.textContent || el.getAttribute('placeholder') || el.tagName).trim().replace(/\s+/g, ' ').slice(0, 28);
      const key = `${label}@${Math.round(b.width)}x${Math.round(b.height)}`;
      if (seen.has(key)) continue;
      seen.add(key);
      // Full-width rows (list items, text fields) only need the height; icons need both.
      const wide = b.width >= 120;
      if (b.height < MIN - 4.5 || (!wide && b.width < MIN - 4.5)) small.push(`${label} ${Math.round(b.width)}x${Math.round(b.height)}`);
      if (!strip && (b.right > vw + 1 || b.left < -1)) offscreen.push(label);
    }
    const tiny = new Set();
    for (const el of document.querySelectorAll('body *')) {
      const t = [...el.childNodes].filter((n) => n.nodeType === 3).map((n) => n.textContent.trim()).join('');
      if (!t) continue;
      const b = el.getBoundingClientRect();
      if (!b.width || b.bottom < 0 || b.top > vh) continue;
      const fs = parseFloat(getComputedStyle(el).fontSize);
      if (fs < 12 && !el.closest('.xterm')) tiny.add(`${t.slice(0, 16)}(${fs}px)`);
    }
    return {
      small,
      offscreen,
      tinyText: [...tiny].slice(0, 12),
      hScroll: document.documentElement.scrollWidth > vw + 1,
      docScrollY: scrollY,
    };
  }, MIN);
  report.screens[name] = r;
  await page.screenshot({ path: join(OUT, `${name}.png`) });
  return r;
}

/** Where an element sits relative to the visible part of the screen (above the keyboard). */
async function place(page, locator) {
  const b = await locator.boundingBox();
  const band = await page.evaluate(() => {
    const v = window.visualViewport;
    return v ? { top: v.offsetTop, bottom: v.offsetTop + v.height } : { top: 0, bottom: innerHeight };
  });
  if (!b) return 'missing';
  if (b.y + b.height <= band.bottom + 0.5 && b.y >= band.top - 0.5) return 'visible';
  return b.y + b.height > band.bottom ? `below fold (${Math.round(b.y + b.height - band.bottom)}px)` : 'above';
}

function flow(name) {
  const f = { taps: 0, scrolls: 0, notes: [] };
  report.flows[name] = f;
  return {
    f,
    async tap(locator) {
      await locator.scrollIntoViewIfNeeded().catch(() => undefined);
      await locator.tap();
      f.taps++;
    },
    note(s) {
      f.notes.push(s);
    },
  };
}

const browser = await webkit.launch();
const ctx = await browser.newContext({ ...devices['iPhone 15'] });
// iOS keyboard model: `__kb(px)` shrinks the visual viewport (layout stays), like Safari does.
await ctx.addInitScript(() => {
  const listeners = new Set();
  let kb = 0;
  const vv = {
    get width() {
      return innerWidth;
    },
    get height() {
      return innerHeight - kb;
    },
    offsetTop: 0,
    offsetLeft: 0,
    pageTop: 0,
    pageLeft: 0,
    scale: 1,
    addEventListener: (t, fn) => (t === 'resize' || t === 'scroll') && listeners.add(fn),
    removeEventListener: (t, fn) => listeners.delete(fn),
  };
  Object.defineProperty(window, 'visualViewport', { get: () => vv, configurable: true });
  window.__kb = (px) => {
    kb = px;
    for (const fn of listeners) fn(new Event('resize'));
  };
});
const page = await ctx.newPage();
page.on('pageerror', (e) => report.errors.push(String(e)));
page.on('console', (m) => m.type() === 'error' && report.errors.push(m.text()));
// A fresh document per flow (a hash-only goto would keep the mock state of the last flow).
const go = async (hash) => {
  await page.goto('about:blank');
  await page.goto(`${base}/?mock=1${hash}`);
  await page.waitForSelector('#root aside, #root main', { timeout: 10_000 });
  await page.waitForTimeout(400);
};
const keyboard = async (up) => {
  await page.evaluate((px) => window.__kb(px), up ? KB : 0);
  await page.waitForTimeout(250);
};

try {
  // A. Home -> the chat that needs approval -> approve.
  await go('#/');
  await page.waitForSelector('text=修复锁屏后中继连接假在线');
  await audit(page, 'a1-home');
  {
    const { tap, note, f } = flow('approve');
    await tap(page.getByText('修复锁屏后中继连接假在线'));
    await page.waitForSelector('text=运行中继集成测试');
    await page.waitForTimeout(400);
    await audit(page, 'a2-chat-approval');
    const allow = page.getByRole('button', { name: '允许', exact: true });
    const allowPlace = await place(page, allow);
    note(`allow button: ${allowPlace}`);
    check(allowPlace === 'visible', 'approve: allow button not visible');
    const card = page.locator('text=运行中继集成测试').first();
    note(`approval title: ${await place(page, card)}`);
    await tap(allow);
    await page.waitForTimeout(800);
    await audit(page, 'a3-after-approve');
    note(`taps to approve from home: ${f.taps}`);
  }

  // B. New Codex chat: folder, approval mode, first message, create.
  await go('#/');
  {
    const { tap, note, f } = flow('new-chat');
    await tap(page.getByRole('button', { name: '新建会话' }).first());
    const dlg = page.getByRole('dialog');
    await dlg.getByText('工作目录').waitFor();
    await page.waitForTimeout(400);
    await audit(page, 'b1-new-session');
    const create = dlg.getByRole('button', { name: '创建' });
    note(`create button: ${await place(page, create)}`);
    const approval = dlg.getByRole('radio', { name: '完全放行' });
    const approvalPlace = await place(page, approval);
    note(`approval control: ${approvalPlace}`);
    check(approvalPlace === 'visible', 'new-chat: approval control below the fold');
    const firstMsg = dlg.getByPlaceholder('可选').first();
    const msgPlace = await place(page, firstMsg);
    note(`first message field: ${msgPlace}`);
    const body = dlg.locator('.overflow-y-auto').first();
    const sh = await body.evaluate((el) => ({ scroll: el.scrollHeight, client: el.clientHeight }));
    note(`sheet body ${sh.scroll}px content in ${sh.client}px (${(sh.scroll / sh.client).toFixed(1)} screens)`);
    await tap(dlg.getByRole('button', { name: 'yonder' }).first());
    await tap(approval);
    if (msgPlace !== 'visible') {
      await firstMsg.scrollIntoViewIfNeeded();
      f.scrolls++;
    }
    await tap(firstMsg);
    await keyboard(true);
    await page.keyboard.type('检查 relay 的重连逻辑');
    const createKb = await place(page, create);
    note(`create button with keyboard up: ${createKb}`);
    check(createKb === 'visible', 'new-chat: create button under the keyboard');
    await audit(page, 'b2-new-session-filled');
    await keyboard(false);
    await tap(create);
    await page.waitForTimeout(1200);
    await audit(page, 'b3-created');
    const header = await page.locator('header').first().innerText();
    note(`header after create: ${header.replace(/\s+/g, ' ').slice(0, 80)}`);
    const modeChip = page.getByRole('button', { name: /^审批模式/ });
    note(`mode chip: ${(await modeChip.count()) ? await modeChip.getAttribute('aria-label') : 'missing'}`);
    check((await modeChip.getAttribute('aria-label').catch(() => '')) === '审批模式：完全放行', 'new-chat: created chat does not run in the picked mode');
    note(`taps: ${f.taps}, scrolls: ${f.scrolls}`);
  }

  // B2. The picked mode is remembered for the next Codex chat, and pi has no approval control.
  {
    const { note } = flow('new-chat-defaults');
    await page.getByRole('button', { name: '返回' }).tap();
    await page.getByRole('button', { name: '新建会话' }).first().tap();
    const dlg = page.getByRole('dialog');
    await dlg.getByText('工作目录').waitFor();
    const checked = await dlg.getByRole('radio', { checked: true }).allInnerTexts();
    note(`preselected: ${checked.join(' / ')}`);
    check(checked.some((t) => t.includes('完全放行')), 'new-chat-defaults: last pick not remembered');
    await dlg.getByRole('button', { name: /^pi/ }).tap();
    await page.waitForTimeout(200);
    const piApproval = await dlg.getByRole('radio', { name: '完全放行' }).count();
    note(`pi approval control: ${piApproval ? 'shown' : 'hidden'}`);
    check(!piApproval, 'new-chat-defaults: pi shows an approval control');
    await dlg.getByRole('button', { name: '关闭' }).tap();
    await page.waitForTimeout(300);
  }

  // B3. A running chat: switch to full access while an approval waits; it resolves.
  await go(`#/h/${MAC}/s/s_chat_relay`);
  {
    const { tap, note, f } = flow('switch-mode');
    await page.waitForSelector('text=运行中继集成测试');
    const chip = page.getByRole('button', { name: /^审批模式/ });
    note(`chip: ${await chip.getAttribute('aria-label')} (${await place(page, chip)})`);
    await tap(chip);
    const menu = page.getByRole('menu', { name: '审批模式' });
    await menu.waitFor();
    await audit(page, 'b4-mode-menu');
    const items = await menu.getByRole('menuitemradio').allInnerTexts();
    note(`menu: ${items.map((t) => t.split('\n')[0]).join(' / ')}`);
    const yolo = menu.getByRole('menuitemradio', { name: /完全放行/ });
    const yoloPlace = await place(page, yolo);
    note(`yolo item: ${yoloPlace}`);
    check(yoloPlace === 'visible', 'switch-mode: menu item off screen');
    await tap(yolo);
    await page.waitForTimeout(900);
    const cards = await page.getByRole('button', { name: '允许', exact: true }).count();
    note(`approval cards left: ${cards}`);
    check(cards === 0, 'switch-mode: pending approval not resolved by full access');
    note(`chip after: ${await chip.getAttribute('aria-label')}`);
    check((await chip.getAttribute('aria-label')) === '审批模式：完全放行', 'switch-mode: chip did not change');
    await audit(page, 'b5-mode-yolo');
    note(`taps to full access: ${f.taps}`);
  }

  // C. Running chat: send, open a diff, header menu.
  await go(`#/h/${MAC}/s/s_chat_relay`);
  {
    const { tap, note } = flow('chat');
    await page.waitForSelector('text=运行中继集成测试');
    const composer = page.getByRole('textbox', { name: '消息' });
    await tap(composer);
    await page.keyboard.type('再看一下 ping 超时');
    await tap(page.getByRole('button', { name: '发送' }));
    await page.waitForTimeout(600);
    // The tap must send: the message shows up and the box empties (WebKit drops the click
    // of a tap whose pointerdown was cancelled).
    const sent = await page.getByRole('main').getByText('再看一下 ping 超时').count();
    const left = await composer.inputValue();
    note(`send tap: ${sent ? 'sent' : 'not sent'}, box ${left ? `still has "${left}"` : 'empty'}`);
    check(sent > 0 && !left, 'chat: tapping send did not send');
    await audit(page, 'c1-sent');
    const diffHdr = page.getByText('修改 1 个文件').first();
    note(`diff item: ${await place(page, diffHdr)}`);
    await tap(diffHdr);
    await page.waitForTimeout(300);
    await audit(page, 'c2-diff-open');
    const menu = page.getByRole('button', { name: '更多操作' });
    await tap(menu);
    const items = await page.getByRole('menuitem').allInnerTexts();
    note(`header menu: ${items.join(' / ')}`);
    await page.keyboard.press('Escape');
  }

  // D. Keyboard up (iOS overlay model): the focused field and its action must stay visible.
  for (const [name, setup] of [
    [
      'composer',
      async () => {
        await go(`#/h/${MAC}/s/s_chat_relay`);
        await page.waitForSelector('text=运行中继集成测试');
        const box = page.getByRole('textbox', { name: '消息' });
        await box.focus();
        // While a turn runs, send appears once there is text (stop has the spot before).
        await page.keyboard.type('x');
        return [box, page.getByRole('button', { name: '发送' })];
      },
    ],
    [
      'new-session-prompt',
      async () => {
        await go('#/');
        await page.getByRole('button', { name: '新建会话' }).first().tap();
        const dlg = page.getByRole('dialog');
        await dlg.getByText('工作目录').waitFor();
        const msg = dlg.getByPlaceholder('可选').first();
        await msg.scrollIntoViewIfNeeded();
        await msg.focus();
        return [msg, dlg.getByRole('button', { name: '创建' })];
      },
    ],
    [
      'terminal',
      async () => {
        await go(`#/h/${MAC}/s/s_term_zsh`);
        await page.waitForSelector('.xterm-rows');
        await page.locator('.term-host').tap();
        return [page.locator('.xterm-cursor-layer, .xterm-rows').first(), page.getByRole('button', { name: 'Ctrl+C' })];
      },
    ],
  ]) {
    const [field, action] = await setup();
    await keyboard(true);
    const r = { field: await place(page, field), action: await place(page, action) };
    report.keyboard[name] = r;
    check(r.field === 'visible' && r.action === 'visible', `keyboard-${name}: ${JSON.stringify(r)}`);
    await audit(page, `d-keyboard-${name}`);
    await keyboard(false);
  }

  // D2. Every button that keeps the keyboard up must still react to a touch tap. Checked
  // generically: count clicks reaching the buttons of the composer and the key bar.
  for (const [name, hash, ready, area] of [
    ['composer', `#/h/${MAC}/s/s_chat_relay`, 'text=运行中继集成测试', 'main form, main textarea'],
    ['keybar', `#/h/${MAC}/s/s_term_zsh`, '.xterm-rows', '.term-host'],
  ]) {
    await go(hash);
    await page.waitForSelector(ready);
    await page.evaluate(() => {
      window.__clicks = 0;
      document.addEventListener('click', (e) => e.target.closest?.('button') && window.__clicks++, true);
    });
    const buttons =
      name === 'composer'
        ? [page.getByRole('button', { name: '发送' })]
        : ['Esc', 'Tab', '上', '下', 'Ctrl+C', '|', '~'].map((k) => page.getByRole('button', { name: k, exact: true }));
    if (name === 'composer') {
      await page.getByRole('textbox', { name: '消息' }).tap();
      await page.keyboard.type('y');
    } else {
      await page.locator(area).tap();
    }
    let lost = 0;
    for (const b of buttons) {
      const before = await page.evaluate(() => window.__clicks);
      await b.tap();
      await page.waitForTimeout(80);
      if ((await page.evaluate(() => window.__clicks)) === before) lost++;
    }
    report.keyboard[`${name}-taps`] = `${buttons.length - lost}/${buttons.length} taps clicked`;
    check(!lost, `taps-${name}: ${lost} of ${buttons.length} taps produced no click`);
  }

  // E. Terminal with the key bar.
  await go(`#/h/${MAC}/s/s_term_zsh`);
  {
    const { tap, note } = flow('terminal');
    await page.waitForSelector('.xterm-rows');
    await page.waitForTimeout(500);
    await tap(page.locator('.term-host'));
    await page.keyboard.type('ls');
    await page.keyboard.press('Enter');
    await page.keyboard.type('abc');
    await tap(page.getByRole('button', { name: 'Ctrl+C' }));
    await page.waitForTimeout(300);
    const rows = await page.locator('.xterm-rows').innerText();
    note(`ctrl+c tap: ${rows.includes('abc^C') ? 'sent' : 'ignored'}`);
    check(rows.includes('abc^C'), 'terminal: tapping a key bar key did nothing');
    await audit(page, 'e1-terminal');
    const bar = await page.locator('.overflow-x-auto').last().evaluate((el) => ({ sw: el.scrollWidth, cw: el.clientWidth }));
    note(`key bar ${bar.sw}px wide in ${bar.cw}px`);
    note(`keyboard toggle: ${await place(page, page.getByRole('button', { name: /键盘/ }))}`);
  }

  // F. Files: open a folder, preview a file, go up.
  await go(`#/h/${MAC}/files`);
  {
    const { tap, note, f } = flow('files');
    await page.waitForSelector('text=Downloads');
    await audit(page, 'f1-files');
    await tap(page.getByText('Documents', { exact: true }));
    await page.waitForSelector('text=周报-0926.md');
    await tap(page.getByText('周报-0926.md'));
    await page.getByRole('dialog').waitFor();
    await page.waitForTimeout(400);
    await audit(page, 'f2-preview');
    await tap(page.getByRole('dialog').getByRole('button', { name: '关闭' }));
    await tap(page.getByRole('button', { name: '上一级' }));
    await page.waitForSelector('text=Downloads');
    note(`taps: ${f.taps}`);
  }

  // G. Settings and host info.
  await go('#/settings');
  await page.waitForSelector('text=已配对设备');
  await page.waitForTimeout(400);
  await audit(page, 'g1-settings');
  await go(`#/h/${MAC}/info`);
  await page.waitForTimeout(600);
  await audit(page, 'g2-host-info');

  // H. A pi chat has no approval chip; linux-box's Claude chat shows its mode.
  await go(`#/h/${LINUX}/s/s_chat_deploy`);
  {
    const { note } = flow('other-host');
    await page.waitForSelector('text=部署中继到 relay-1');
    note(`chip: ${(await page.getByRole('button', { name: /^审批模式/ }).getAttribute('aria-label').catch(() => 'missing')) ?? 'missing'}`);
    await audit(page, 'h1-claude-yolo');
  }
} catch (err) {
  report.errors.push(`probe: ${err.stack ?? err}`);
} finally {
  await browser.close();
  stop();
}

writeFileSync(join(OUT, 'report.json'), JSON.stringify(report, null, 1));
console.log(JSON.stringify(report, null, 1));
process.exit(report.errors.length || report.fail.length ? 1 : 0);
