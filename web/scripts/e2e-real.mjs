#!/usr/bin/env node
// Self-contained real-host UI end-to-end test:
//   Playwright (bundled Chromium / WebKit) -> local yonder-relay (serving web/dist) -> real `yonder daemon`.
// The daemon runs with temp config/data dirs and a fake pi agent, so nothing touches the user's real host.
//
// Usage: node scripts/e2e-real.mjs [chromium|webkit|all] [desktop|mobile|all]
// Env:   YONDER_BIN, YONDER_RELAY_BIN  override the binaries (default: target/debug)
//        SHOTS_DIR                      screenshot root (default: <tmp>/shots)
//        KEEP=1                         keep the temp dir (logs, screenshots) and print its path
import { chromium, webkit, devices } from 'playwright';
import { execFile, spawn } from 'node:child_process';
import { appendFileSync, existsSync, mkdirSync, mkdtempSync, openSync, readFileSync, realpathSync, rmSync, writeFileSync, closeSync } from 'node:fs';
import { createServer } from 'node:net';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const WIN = process.platform === 'win32';
const EXE = WIN ? '.exe' : '';
const REPO = join(dirname(fileURLToPath(import.meta.url)), '..', '..');
const YONDER = process.env.YONDER_BIN || join(REPO, 'target', 'debug', `yonder${EXE}`);
const RELAY = process.env.YONDER_RELAY_BIN || join(REPO, 'target', 'debug', `yonder-relay${EXE}`);
const WEB_DIST = join(REPO, 'web', 'dist');
const FAKE_PI = join(REPO, 'crates', 'yonder-host', 'tests', 'fixtures', 'fake-pi.mjs');
const FAKE_CODEX = join(REPO, 'crates', 'yonder-host', 'tests', 'fixtures', 'fake-codex.mjs');
const HOST_NAME = 'e2e-host';

// ---------- arguments ----------

const ENGINES = { chromium, webkit };
const FORMS = ['desktop', 'mobile'];
const engineArg = process.argv[2] ?? 'all';
const formArg = process.argv[3] ?? 'all';
if (engineArg !== 'all' && !ENGINES[engineArg]) usage(`unknown engine: ${engineArg}`);
if (formArg !== 'all' && !FORMS.includes(formArg)) usage(`unknown form: ${formArg}`);
const engines = engineArg === 'all' ? Object.keys(ENGINES) : [engineArg];
const forms = formArg === 'all' ? FORMS : [formArg];

function usage(msg) {
  console.error(`${msg}\nusage: node scripts/e2e-real.mjs [chromium|webkit|all] [desktop|mobile|all]`);
  process.exit(2);
}

// ---------- preflight ----------

for (const bin of [YONDER, RELAY]) {
  if (!existsSync(bin)) {
    console.error(`missing binary: ${bin}\nbuild it with:\n  cd ${REPO} && cargo build -p yonder-cli -p yonder-relay\n(or point YONDER_BIN / YONDER_RELAY_BIN at existing binaries)`);
    process.exit(1);
  }
}
if (!existsSync(join(WEB_DIST, 'index.html'))) {
  console.error(`missing ${join(WEB_DIST, 'index.html')}\nbuild the web client with:\n  cd ${join(REPO, 'web')} && pnpm build`);
  process.exit(1);
}

// ---------- temp environment ----------

// Unix sockets live under data/, so keep the path short (/tmp, not the long macOS $TMPDIR).
const TMP = realpathSync(mkdtempSync(join(WIN ? tmpdir() : '/tmp', 'yonder-e2e-')));
const CFG = join(TMP, 'cfg');
const DATA = join(TMP, 'data');
const WORK = join(TMP, 'work');
const SRC = join(TMP, 'src'); // files the browser uploads / attaches
for (const d of [CFG, DATA, WORK, SRC]) mkdirSync(d, { recursive: true });
const SHOTS_ROOT = process.env.SHOTS_DIR || join(TMP, 'shots');
const KEEP = process.env.KEEP === '1';

const hostEnv = {
  ...process.env,
  YONDER_CONFIG_DIR: CFG,
  YONDER_DATA_DIR: DATA,
  YONDER_LOG_STDERR: '1',
  YONDER_PRIVATE_TRASH: '1',
  YONDER_LINGER_SECS: '20',
};

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** Runs the yonder CLI against the temp host; rejects with stderr on a non-zero exit. */
function yonder(...args) {
  return new Promise((resolve, reject) => {
    execFile(YONDER, args, { env: hostEnv, cwd: TMP, encoding: 'utf8', timeout: 30_000 }, (err, stdout, stderr) => {
      if (err) reject(new Error(`yonder ${args.join(' ')} failed: ${err.message}\n${stderr}${stdout}`));
      else resolve(stdout);
    });
  });
}

function freePort() {
  return new Promise((resolve, reject) => {
    const srv = createServer();
    srv.unref();
    srv.on('error', reject);
    srv.listen(0, '127.0.0.1', () => {
      const { port } = srv.address();
      srv.close(() => resolve(port));
    });
  });
}

async function waitFor(what, fn, ms, logFile) {
  const end = Date.now() + ms;
  let last;
  while (Date.now() < end) {
    try {
      if (await fn()) return;
    } catch (err) {
      last = err;
    }
    await sleep(200);
  }
  const log = logFile && existsSync(logFile) ? `\n--- ${logFile} ---\n${readFileSync(logFile, 'utf8').slice(-3000)}` : '';
  throw new Error(`timed out waiting for ${what}${last ? `: ${last.message}` : ''}${log}`);
}

/** Parses `yonder ls` into { id, state, title } rows. */
async function sessions() {
  const out = await yonder('ls');
  return out
    .split('\n')
    .slice(1)
    .map((l) => l.trim().split(/\s+/))
    .filter((c) => c.length >= 4)
    .map(([id, , , state, ...rest]) => ({ id, state, rest: rest.join(' ') }));
}

// ---------- processes and cleanup ----------

let relay = null;
let daemon = null;
const browsers = new Set();
let cleaning = null;

function spawnLogged(bin, args, logFile, env) {
  const fd = openSync(logFile, 'a');
  // Own process group on Unix so a terminal Ctrl-C reaches only this script, which then
  // shuts the host down in order (sessions first, then daemon, then relay).
  const child = spawn(bin, args, { cwd: TMP, env, stdio: ['ignore', fd, fd], windowsHide: true, detached: !WIN });
  closeSync(fd);
  child.exited = new Promise((r) => child.once('exit', r));
  return child;
}

async function stopChild(child, graceMs) {
  if (!child || child.exitCode !== null || child.signalCode !== null) return;
  const done = await Promise.race([child.exited.then(() => true), sleep(graceMs).then(() => false)]);
  if (done) return;
  child.kill();
  const killed = await Promise.race([child.exited.then(() => true), sleep(3000).then(() => false)]);
  if (!killed) child.kill('SIGKILL');
}

/** Kills and removes every session on the temp host so no supervisor outlives the run. Serialized. */
let killChain = Promise.resolve();
function killAllSessions() {
  killChain = killChain.then(killAllSessionsNow, killAllSessionsNow);
  return killChain;
}

async function killAllSessionsNow() {
  let list = [];
  try {
    list = await sessions();
  } catch (err) {
    console.error(`cleanup: cannot list sessions: ${err.message}`);
    return;
  }
  for (const s of list.filter((s) => s.state === 'running' || s.state === 'starting')) {
    await yonder('kill', s.id).catch((e) => console.error(`cleanup: ${e.message}`));
  }
  // Wait for the kills to land, then remove ended sessions (this also stops lingering supervisors).
  await waitFor('sessions to exit', async () => (await sessions()).every((s) => s.state !== 'running' && s.state !== 'starting'), 15_000).catch(
    (e) => console.error(`cleanup: ${e.message}`),
  );
  if ((await sessions().catch(() => [])).length) await yonder('rm', '--exited').catch((e) => console.error(`cleanup: ${e.message}`));
}

function cleanup() {
  cleaning ??= (async () => {
    for (const b of browsers) await b.close().catch(() => undefined);
    browsers.clear();
    if (daemon) {
      await killAllSessions();
      await yonder('stop').catch(() => undefined);
      await stopChild(daemon, 5000);
    }
    if (relay) {
      relay.kill();
      await stopChild(relay, 3000);
    }
    if (KEEP) {
      console.log(`KEEP=1: temp dir kept at ${TMP}`);
    } else {
      // Supervisors may still be releasing files for a moment (Windows).
      for (let i = 0; i < 5; i++) {
        try {
          rmSync(TMP, { recursive: true, force: true });
          break;
        } catch {
          await sleep(500);
        }
      }
    }
  })();
  return cleaning;
}

for (const sig of ['SIGINT', 'SIGTERM']) {
  process.once(sig, () => {
    console.error(`\n${sig}: cleaning up...`);
    cleanup().finally(() => process.exit(130));
  });
}

// ---------- host setup ----------

async function startHost() {
  const port = await freePort();
  const relayLog = join(TMP, 'relay.log');
  relay = spawnLogged(
    RELAY,
    ['--listen', `127.0.0.1:${port}`, '--key-file', join(TMP, 'relay.key'), '--web-dir', WEB_DIST],
    relayLog,
    process.env,
  );
  const base = `http://127.0.0.1:${port}`;
  await waitFor(
    'relay health',
    async () => {
      if (relay.exitCode !== null) throw new Error(`relay exited with ${relay.exitCode}`);
      const r = await fetch(`${base}/v1/health`);
      return r.ok && (await r.text()).trim() === 'ok';
    },
    15_000,
    relayLog,
  );

  await yonder('init', '--name', HOST_NAME, '--relay', `ws://127.0.0.1:${port}/v1/ws`, '--web', base, '--root', WORK);
  appendFileSync(
    join(CFG, 'config.toml'),
    // The fake Codex behaves like Codex 0.72 (no turn/steer), the oldest one in the fleet.
    `\n[agents.pi]\nprogram = ${JSON.stringify(['node', FAKE_PI])}\n\n[agents.codex]\nprogram = ${JSON.stringify(['node', FAKE_CODEX])}\nenv = { FAKE_CODEX_NO_STEER = "1" }\n`,
  );

  const daemonLog = join(TMP, 'daemon.log');
  daemon = spawnLogged(YONDER, ['daemon'], daemonLog, hostEnv);
  await waitFor(
    'daemon to connect to the relay',
    async () => {
      if (daemon.exitCode !== null) throw new Error(`daemon exited with ${daemon.exitCode}`);
      return (await yonder('status')).includes('(connected)');
    },
    30_000,
    daemonLog,
  );
  console.log(`relay ${base}, daemon connected, temp dir ${TMP}`);
}

// ---------- one engine x form combination ----------

async function runCombo(engine, form) {
  const tag = `${engine}-${form}`;
  const mobile = form === 'mobile';
  const shots = join(SHOTS_ROOT, tag);
  mkdirSync(shots, { recursive: true });
  const results = [];
  const errors = [];
  console.log(`\n=== ${tag} ===`);

  const link = (await yonder('pair', '--link-only', '--minutes', '10')).trim();
  // Playwright would otherwise exit the process on SIGINT before our cleanup runs.
  const browser = await ENGINES[engine].launch({ handleSIGINT: false, handleSIGTERM: false });
  browsers.add(browser);
  try {
    const ctxOpts = mobile ? { ...devices['iPhone 15'] } : { viewport: { width: 1280, height: 820 } };
    if (engine === 'chromium' && mobile) delete ctxOpts.defaultBrowserType;
    const ctx = await browser.newContext({ ...ctxOpts, acceptDownloads: true });
    // iOS saves downloads through the share sheet; capture what would be shared. Like WebKit,
    // the stub refuses without a recent tap; `__denyShareOnce` simulates a tap that expired.
    const shareMode = engine === 'webkit' && mobile;
    if (shareMode) {
      await ctx.addInitScript(() => {
        window.__shared = [];
        window.__denyShareOnce = false;
        navigator.canShare = () => true;
        navigator.share = async (data) => {
          const active = navigator.userActivation ? navigator.userActivation.isActive : true;
          if (window.__denyShareOnce || !active) {
            window.__denyShareOnce = false;
            throw new DOMException('share() needs a user gesture', 'NotAllowedError');
          }
          for (const f of data.files ?? []) window.__shared.push({ name: f.name, text: await f.text() });
        };
      });
    }
    const page = await ctx.newPage();
    page.on('pageerror', (e) => errors.push(`pageerror: ${e.message}`));
    page.on('console', (m) => {
      if (m.type() === 'error') errors.push(`console: ${m.text()}`);
    });

    let n = 0;
    const shot = (name) => page.screenshot({ path: join(shots, `${String(++n).padStart(2, '0')}-${name}.png`) });
    async function step(name, fn) {
      const t0 = Date.now();
      try {
        await fn();
        await shot(name);
        results.push({ name, ok: true, ms: Date.now() - t0 });
        console.log(`PASS ${name} (${Date.now() - t0} ms)`);
      } catch (err) {
        await shot(`FAIL-${name}`).catch(() => undefined);
        results.push({ name, ok: false, ms: Date.now() - t0, error: String(err).slice(0, 600) });
        console.log(`FAIL ${name}: ${String(err).slice(0, 600)}`);
        // Do not let a dialog or menu left open by a failed step break the following ones.
        for (let i = 0; i < 3 && (await page.getByRole('dialog').or(page.getByRole('menu')).count().catch(() => 0)); i++) {
          await page.keyboard.press('Escape').catch(() => undefined);
          await page.waitForTimeout(200);
        }
      }
    }

    const main = () => page.getByRole('main');
    const marker = `${engine[0]}${form[0]}${Date.now() % 100000}`;

    async function waitDisk(path, want, ms = 30_000) {
      const end = Date.now() + ms;
      while (Date.now() < end) {
        if (existsSync(path) && readFileSync(path, 'utf8') === want) return;
        await page.waitForTimeout(200);
      }
      throw new Error(`file never reached expected content: ${path}`);
    }

    // On phones the sidebar is a separate screen; this returns to it.
    async function toSidebar() {
      if (!mobile) return;
      await page.evaluate(() => (location.hash = '#/'));
      await page.waitForTimeout(300);
    }

    async function newSession({ kind, agentLabel, cwd, title, model, approval }) {
      await toSidebar();
      await page.getByRole('button', { name: '新建会话' }).first().click();
      const dlg = page.getByRole('dialog');
      await dlg.getByRole('radio', { name: kind, exact: true }).click();
      const agentBtn = dlg.getByRole('button', { name: new RegExp(`^${agentLabel}\\b`) });
      await agentBtn.and(page.locator(':not([disabled])')).waitFor({ timeout: 15_000 });
      await agentBtn.click();
      if (cwd) await dlg.getByLabel('工作目录').fill(cwd);
      if (approval) await dlg.getByRole('radio', { name: approval, exact: true }).click();
      if (model) {
        await dlg.getByRole('button', { name: /^模型：/ }).click();
        const search = dlg.getByRole('combobox', { name: '搜索模型' });
        await search.fill(model);
        await dlg.getByRole('option', { name: `使用 ${model}` }).click();
        await dlg.getByRole('button', { name: `模型：${model}` }).waitFor({ timeout: 5_000 });
      }
      if (title) await dlg.getByLabel('标题', { exact: true }).fill(title);
      await dlg.getByRole('button', { name: /^(创建|恢复会话)$/ }).click();
      await dlg.waitFor({ state: 'detached', timeout: 15_000 });
    }

    async function termText() {
      // Rows are separate lines; long lines wrap on phones, so join them.
      return (await page.locator('.xterm-rows').innerText()).replace(/\u00a0/g, ' ').replace(/\n/g, '');
    }

    const termHas = (needle, timeout = 15_000) =>
      page.waitForFunction((s) => document.querySelector('.xterm-rows')?.textContent?.includes(s), needle, { timeout });

    async function termReady() {
      await page.locator('.xterm-rows').waitFor({ timeout: 15_000 });
      await page.waitForFunction(() => !document.body.innerText.includes('正在连接终端'), null, { timeout: 15_000 });
      await page.waitForTimeout(1200);
    }

    async function termType(line) {
      await page.locator('.term-host').click();
      await page.keyboard.type(line);
      await page.keyboard.press('Enter');
    }

    // Phones tap (a real touch sequence): WebKit drops the click of a tap whose pointerdown
    // was cancelled, which a mouse click would not catch.
    const pressSend = () => (mobile ? page.getByRole('button', { name: '发送' }).tap() : page.getByRole('button', { name: '发送' }).click());

    async function headerMenu(item) {
      await page.getByRole('button', { name: '更多操作', exact: true }).first().click();
      await page.getByRole('menuitem', { name: item }).click();
    }

    async function rowMenu(name, item) {
      const row = page.locator('li, tr, [role=row], div.group').filter({ hasText: name }).last();
      await row.hover().catch(() => undefined);
      await row.getByRole('button', { name: '更多' }).click();
      await page.getByRole('menuitem', { name: item }).or(page.getByRole('button', { name: item, exact: true })).first().click();
    }

    // ---- steps ----

    await step('pair', async () => {
      await page.goto(link);
      await page.getByRole('dialog').getByText(HOST_NAME).first().waitFor();
      await page.getByRole('dialog').getByRole('button', { name: '配对', exact: true }).click();
      await page.getByText('暂无会话').or(page.getByText(`已与 ${HOST_NAME} 配对`)).first().waitFor({ timeout: 20_000 });
    });

    await step('terminal-create-and-type', async () => {
      await newSession({ kind: '终端', agentLabel: 'Shell', cwd: WORK, title: `term-${marker}` });
      await termReady();
      await termType(`echo ${marker}-$((6*7)) && pwd`);
      await termHas(`${marker}-42`);
      await waitFor('pwd output', async () => (await termText()).includes(WORK), 10_000).catch(async () => {
        throw new Error(`cwd ${WORK} missing in terminal:\n${(await termText()).slice(-600)}`);
      });
    });

    await step('terminal-reattach-after-reload', async () => {
      const url = page.url();
      await page.reload();
      if (!page.url().includes('/s/')) await page.goto(url);
      await page.locator('.xterm-rows').waitFor({ timeout: 15_000 });
      await termHas(`${marker}-42`, 20_000);
      await termType('echo after-$((40+2))');
      await termHas('after-42');
    });

    await step('host-session-takeover', async () => {
      const title = `host-${marker}`;
      const id = (await yonder('run', '-d', '-t', title, '-C', WORK)).trim();
      if (!/^\w+$/.test(id)) throw new Error(`unexpected session id from yonder run: ${id}`);
      await toSidebar();
      await page.getByRole('button', { name: new RegExp(title) }).first().click({ timeout: 15_000 });
      await termReady();
      await termType(`echo take-$((3*4))`);
      await termHas('take-12');
      // End it from the header menu.
      await headerMenu('结束会话');
      const confirmEnd = page.getByRole('dialog');
      await confirmEnd.getByRole('button', { name: '结束会话' }).click();
      await confirmEnd.waitFor({ state: 'detached', timeout: 10_000 });
      await page.getByText('进程已退出').first().waitFor({ timeout: 20_000 });
      // Then delete it.
      await headerMenu('删除');
      const confirmDel = page.getByRole('dialog');
      await confirmDel.getByRole('button', { name: '删除' }).click();
      await confirmDel.waitFor({ state: 'detached', timeout: 10_000 });
      await waitFor(`session ${id} to disappear from yonder ls`, async () => !(await sessions()).some((s) => s.id === id), 15_000);
    });

    await step('chat-create-and-reply', async () => {
      // The fake pi has no model list: the picker takes a typed id and pi reports it back.
      await newSession({ kind: '对话', agentLabel: 'pi', cwd: WORK, title: `chat-${marker}`, model: 'fake/picked-model' });
      await page.locator('header').getByText('fake/picked-model').first().waitFor({ timeout: 15_000 });
      const box = page.getByLabel('消息', { exact: true });
      await box.waitFor({ timeout: 15_000 });
      await box.fill('hello yonder');
      await pressSend();
      await main().getByText('echo: hello yonder').waitFor({ timeout: 20_000 });
    });

    await step('chat-approval', async () => {
      await page.getByLabel('消息', { exact: true }).fill('ask permission please');
      await pressSend();
      await main().getByText('Run fake command?').first().waitFor({ timeout: 20_000 });
      await shot('approval-card');
      await page.getByRole('button', { name: '确认', exact: true }).click();
      await main().getByText('approved', { exact: true }).waitFor({ timeout: 20_000 });
    });

    await step('chat-interrupt', async () => {
      await page.getByLabel('消息', { exact: true }).fill('slow stream please');
      await pressSend();
      await main().getByText(/chunk3 /).first().waitFor({ timeout: 20_000 });
      await page.getByRole('button', { name: '停止' }).click();
      await page.getByRole('button', { name: '停止' }).waitFor({ state: 'detached', timeout: 20_000 });
      if ((await main().innerText()).includes('chunk39')) throw new Error('stream was not interrupted');
    });

    await step('chat-attachment', async () => {
      const name = `attach-${marker}.txt`;
      const f = join(SRC, name);
      writeFileSync(f, `attachment body ${marker}\n`);
      await page.locator('input[type=file]').setInputFiles(f);
      await page.getByText(name).first().waitFor({ timeout: 15_000 });
      await page.waitForFunction(() => !document.querySelector('[aria-label="移除附件"]')?.parentElement?.querySelector('.animate-spin'), null, {
        timeout: 15_000,
      });
      await page.getByLabel('消息', { exact: true }).fill('see attached');
      await pressSend();
      await main().getByText(/echo: see attached/).first().waitFor({ timeout: 20_000 });
    });

    // Phone locked / app in the background: the connection goes silent without a close
    // handshake (WebKit's setOffline does not cut open sockets, so freeze the relay instead).
    // The page must notice by itself, show it, and recover once the network is back, with the
    // chat still usable.
    await step('chat-resume-after-network-loss', async () => {
      if (WIN) return; // no SIGSTOP on Windows
      process.kill(relay.pid, 'SIGSTOP');
      let noticed;
      try {
        // Relay ping every 25 s, pong timeout 10 s: the banner must appear within ~40 s.
        noticed = await page
          .getByRole('status')
          .filter({ hasText: /正在连接|离线/ })
          .first()
          .waitFor({ timeout: 45_000 })
          .then(
            () => true,
            () => false,
          );
      } finally {
        process.kill(relay.pid, 'SIGCONT');
      }
      if (!noticed) throw new Error('the page never noticed the dead connection');
      await page
        .getByRole('status')
        .filter({ hasText: /正在连接|离线/ })
        .first()
        .waitFor({ state: 'detached', timeout: 45_000 });
      const box = page.getByLabel('消息', { exact: true });
      await box.fill('back again');
      await pressSend();
      await main().getByText('echo: back again').first().waitFor({ timeout: 45_000 });
    });

    const upName = `up-${marker}.txt`;
    const upBody = `upload body ${marker} ${'x'.repeat(300000)}\n`;
    // The fake Codex asks before `run <cmd>` unless its approval policy is "never".
    await step('chat-approval-mode', async () => {
      await newSession({ kind: '对话', agentLabel: 'Codex', cwd: WORK, title: `mode-${marker}`, approval: '询问' });
      const chip = page.getByRole('button', { name: /^审批模式/ });
      await chip.waitFor({ timeout: 15_000 });
      if ((await chip.getAttribute('aria-label')) !== '审批模式：询问') throw new Error(`chip says ${await chip.getAttribute('aria-label')}`);
      const box = page.getByLabel('消息', { exact: true });
      await box.fill('run touch a');
      await pressSend();
      await page.getByRole('button', { name: '允许', exact: true }).waitFor({ timeout: 20_000 });
      await shot('mode-approval-pending');
      // Full access from the chip: the waiting approval resolves, the command runs.
      await chip.click();
      await page.getByRole('menuitemradio', { name: /^完全放行/ }).click();
      await main().getByText(/ran touch a after approval/).first().waitFor({ timeout: 20_000 });
      if (await page.getByRole('button', { name: '允许', exact: true }).count()) throw new Error('approval card still shown');
      await page.waitForFunction(() => document.querySelector('[aria-label^="审批模式"]')?.getAttribute('aria-label') === '审批模式：完全放行', null, { timeout: 10_000 });
      // The next turn runs without asking, in full access.
      await box.fill('run touch b');
      await pressSend();
      await main().getByText(/ran touch b without asking \[policy=never sandbox=danger-full-access\]/).first().waitFor({ timeout: 20_000 });
      // Back to asking.
      await chip.click();
      await page.getByRole('menuitemradio', { name: /^询问/ }).click();
      await page.waitForFunction(() => document.querySelector('[aria-label^="审批模式"]')?.getAttribute('aria-label') === '审批模式：询问', null, { timeout: 10_000 });
      await box.fill('run touch c');
      await pressSend();
      await page.getByRole('button', { name: '拒绝', exact: true }).click({ timeout: 20_000 });
      await main().getByText(/skipped touch c/).first().waitFor({ timeout: 20_000 });
    });

    // A message sent while a turn runs, to a Codex without turn/steer: no error, it becomes
    // the next turn.
    await step('chat-message-during-turn', async () => {
      const box = page.getByLabel('消息', { exact: true });
      await box.fill('slow first');
      await pressSend();
      await page.getByRole('button', { name: '停止' }).first().waitFor({ timeout: 10_000 });
      await box.fill('echo second');
      await pressSend();
      await main().getByText('消息将在当前回合结束后发送').first().waitFor({ timeout: 10_000 });
      await main().getByText(/slow done: first/).first().waitFor({ timeout: 20_000 });
      await main().getByText(/echo: echo second/).first().waitFor({ timeout: 20_000 });
      if (await main().getByText(/unknown variant/).count()) throw new Error('protocol error shown');
    });

    // A new chat preselects the mode picked last time for that agent.
    await step('chat-approval-mode-remembered', async () => {
      await toSidebar();
      await page.getByRole('button', { name: '新建会话' }).first().click();
      const dlg = page.getByRole('dialog');
      await dlg.getByRole('radio', { name: '对话', exact: true }).click();
      await dlg.getByRole('button', { name: /^Codex\b/ }).and(page.locator(':not([disabled])')).click({ timeout: 15_000 });
      const picked = dlg.getByRole('radio', { name: '询问', exact: true });
      if ((await picked.getAttribute('aria-checked')) !== 'true') throw new Error('last pick (询问) not preselected');
      await dlg.getByRole('radio', { name: '完全放行', exact: true }).click();
      await dlg.getByRole('button', { name: '关闭' }).click();
      await dlg.waitFor({ state: 'detached', timeout: 5_000 });
      await page.getByRole('button', { name: '新建会话' }).first().click();
      await dlg.getByRole('button', { name: /^Codex\b/ }).and(page.locator(':not([disabled])')).click({ timeout: 15_000 });
      if ((await dlg.getByRole('radio', { name: '完全放行', exact: true }).getAttribute('aria-checked')) !== 'true') throw new Error('full access not remembered');
      await dlg.getByRole('button', { name: '关闭' }).click();
      await dlg.waitFor({ state: 'detached', timeout: 5_000 });
    });

    await step('files-upload', async () => {
      await toSidebar();
      await page.getByRole('button', { name: '文件', exact: true }).first().click();
      // Inside main: a sheet that is still animating out may show the same folder name.
      await main().getByText('work', { exact: true }).first().waitFor({ timeout: 15_000 });
      const f = join(SRC, upName);
      writeFileSync(f, upBody);
      await main().locator('input[type=file]').setInputFiles(f);
      await waitDisk(join(WORK, upName), upBody);
      await page.getByText(`已上传 ${upName}`).waitFor({ timeout: 15_000 });
      await main().getByRole('button', { name: new RegExp(upName) }).first().waitFor({ timeout: 15_000 });
    });

    await step('files-download', async () => {
      if (shareMode) {
        await rowMenu(upName, '下载');
        const got = await page.waitForFunction((nm) => window.__shared.find((f) => f.name === nm), upName, { timeout: 30_000 });
        const f = await got.jsonValue();
        if (f.text !== upBody) throw new Error(`shared content differs: ${f.text.length} vs ${upBody.length} bytes`);
        // A download that outlives its tap: the app asks for a new one, then shares.
        await page.evaluate(() => {
          window.__shared = [];
          window.__denyShareOnce = true;
        });
        await rowMenu(upName, '下载');
        const ask = page.getByRole('dialog').filter({ hasText: '下载完成' });
        await ask.getByRole('button', { name: '保存' }).click({ timeout: 15_000 });
        const again = await page.waitForFunction((nm) => window.__shared.find((f) => f.name === nm), upName, { timeout: 30_000 });
        if ((await again.jsonValue()).text !== upBody) throw new Error('content shared after the second tap differs');
        return;
      }
      const dl = page.waitForEvent('download', { timeout: 30_000 });
      await rowMenu(upName, '下载');
      const d = await dl;
      const p = join(SRC, `dl-${tag}-${upName}`);
      await d.saveAs(p);
      if (readFileSync(p, 'utf8') !== upBody) throw new Error('downloaded content differs');
    });

    await step('files-mkdir-rename-delete', async () => {
      const dir = `dir-${marker}`;
      const renamed = `renamed-${marker}.txt`;
      await page.getByRole('button', { name: '新建文件夹' }).click();
      await page.getByLabel('名称').fill(dir);
      await page.getByRole('dialog').getByRole('button', { name: '确定' }).click();
      await page.getByText(dir, { exact: true }).first().waitFor({ timeout: 15_000 });
      if (!existsSync(join(WORK, dir))) throw new Error('mkdir not on disk');
      await rowMenu(upName, '重命名');
      await page.getByLabel('名称').fill(renamed);
      await page.getByRole('dialog').getByRole('button', { name: '确定' }).click();
      await page.getByText(renamed, { exact: true }).first().waitFor({ timeout: 15_000 });
      if (!existsSync(join(WORK, renamed)) || existsSync(join(WORK, upName))) throw new Error('rename not on disk');
      await rowMenu(renamed, '删除');
      await page.getByRole('dialog').getByRole('button', { name: '删除' }).click();
      await page.getByText(renamed, { exact: true }).waitFor({ state: 'detached', timeout: 15_000 });
      if (existsSync(join(WORK, renamed))) throw new Error('delete not on disk');
    });

    // Uploading right after opening a folder must land in that folder, never in the one whose
    // listing was still on screen (the new listing may take a moment over a slow link).
    await step('files-upload-after-navigation', async () => {
      const dir = `dir-${marker}`;
      const quick = `quick-${marker}.txt`;
      const f = join(SRC, quick);
      writeFileSync(f, `quick ${marker}\n`);
      await main().getByRole('button', { name: new RegExp(`^${dir}`) }).first().click();
      await main().locator('input[type=file]').setInputFiles(f);
      await waitDisk(join(WORK, dir, quick), `quick ${marker}\n`);
      if (existsSync(join(WORK, quick))) throw new Error('upload went to the previous folder');
      await main().getByRole('button', { name: new RegExp(quick) }).first().waitFor({ timeout: 15_000 });
      await page.getByRole('button', { name: '上一级' }).click();
      await main().getByRole('button', { name: new RegExp(`^${dir}`) }).first().waitFor({ timeout: 15_000 });
    });

    await step('settings', async () => {
      await toSidebar();
      await page.getByRole('button', { name: '设置' }).click();
      await page.getByText('已配对设备').first().waitFor({ timeout: 10_000 });
    });

    await step('host-info', async () => {
      await toSidebar();
      await page.getByRole('button', { name: '主机信息' }).first().click();
      await page.getByText('已配对设备').first().waitFor({ timeout: 10_000 });
    });
  } finally {
    browsers.delete(browser);
    await browser.close().catch(() => undefined);
  }
  // Leave a clean host for the next combination (unless an interrupt is already tearing it down).
  if (!cleaning) await killAllSessions();
  writeFileSync(join(shots, 'results.json'), JSON.stringify({ results, errors }, null, 2));
  if (errors.length) console.log(`page errors (${errors.length}):\n  ${errors.slice(0, 10).join('\n  ')}`);
  return { tag, results };
}

// ---------- main ----------

const summary = [];
let fatal = null;
try {
  await startHost();
  for (const engine of engines) {
    for (const form of forms) {
      if (cleaning) break;
      try {
        summary.push(await runCombo(engine, form));
      } catch (err) {
        console.log(`ERROR ${engine}-${form}: ${err.stack ?? err}`);
        summary.push({ tag: `${engine}-${form}`, results: [{ name: 'setup', ok: false, error: String(err) }] });
      }
    }
  }
} catch (err) {
  fatal = err;
  console.error(`\nFATAL: ${err.stack ?? err}`);
} finally {
  await cleanup();
}

if (summary.length) {
  const w = Math.max(...summary.map((s) => s.tag.length), 11);
  console.log(`\n${'combination'.padEnd(w)}  passed  failed  failing steps`);
  for (const { tag, results } of summary) {
    const failed = results.filter((r) => !r.ok);
    console.log(
      `${tag.padEnd(w)}  ${String(results.length - failed.length).padStart(6)}  ${String(failed.length).padStart(6)}  ${failed.map((r) => r.name).join(', ') || '-'}`,
    );
  }
}
const anyFail = fatal || summary.some((s) => s.results.some((r) => !r.ok));
if (KEEP || process.env.SHOTS_DIR) console.log(`screenshots: ${SHOTS_ROOT}`);
console.log(anyFail ? '\nE2E FAILED' : '\nE2E PASSED');
process.exit(anyFail ? 1 : 0);
