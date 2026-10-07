import type { AgentSessionSummary } from '../proto/generated/AgentSessionSummary';
import type { AgentAvailability } from '../proto/generated/AgentAvailability';
import type { Approval } from '../proto/generated/Approval';
import type { ChatItem } from '../proto/generated/ChatItem';

export const MOCK_DIFF = `diff --git a/crates/yonder-relay/src/conn.rs b/crates/yonder-relay/src/conn.rs
index 3f2a1c0..8b4e9d2 100644
--- a/crates/yonder-relay/src/conn.rs
+++ b/crates/yonder-relay/src/conn.rs
@@ -41,12 +41,18 @@ impl Conn {
     pub async fn run(mut self) -> Result<()> {
-        let mut ping = interval(Duration::from_secs(30));
+        let mut ping = interval(PING_INTERVAL);
+        let mut idle_deadline = Instant::now() + IDLE_TIMEOUT;
         loop {
             select! {
                 msg = self.ws.next() => {
-                    let Some(msg) = msg else { break };
+                    let Some(msg) = msg else {
+                        debug!(peer = %self.id, "websocket closed");
+                        break;
+                    };
+                    idle_deadline = Instant::now() + IDLE_TIMEOUT;
                     self.handle(msg?).await?;
                 }
                 _ = ping.tick() => self.ws.send(Message::Ping(vec![])).await?,
+                _ = sleep_until(idle_deadline) => break,
             }
         }
`;

export const MOCK_AGENT_MD = `已定位问题：中继在连接空闲时不会主动断开，客户端的应用层 \`ping\` 也没有超时检测，所以手机切到后台再回来时会卡在“已连接”状态。

改动如下：

| 位置 | 变更 | 说明 |
| --- | --- | --- |
| \`yonder-relay/src/conn.rs\` | 空闲超时 90 s | 与协议文档一致 |
| \`web/src/net/relay.ts\` | 25 s ping + 10 s pong 超时 | 超时即重连 |
| \`web/src/net/link.ts\` | 断线后重握手 | 重新 attach 时带 \`since\` |

关键逻辑：

\`\`\`rust
const PING_INTERVAL: Duration = Duration::from_secs(30);
const IDLE_TIMEOUT: Duration = Duration::from_secs(90);
\`\`\`

接下来跑一遍中继的集成测试确认没有回归。`;

export const MOCK_REPLY_MD = `收到。我先看一下相关代码，然后给出修改方案。

1. 读取 \`web/src/net/relay.ts\` 中的重连逻辑
2. 确认退避序列：0.5 s → 1 s → 2 s → 4 s … 上限 15 s
3. 补充 \`visibilitychange\` 与 \`online\` 事件的立即重连

\`\`\`ts
const BACKOFF = [500, 1000, 2000, 4000, 8000, 15000];
\`\`\`

需要运行测试来验证，稍等。`;

export function mockChatItems(now: number): ChatItem[] {
  const t = (m: number) => now - m * 60_000;
  return [
    {
      id: 'u1',
      kind: 'user',
      status: 'completed',
      text: '手机锁屏再打开后，会话列表一直显示“已连接”但收不到新消息。帮我查一下中继的空闲断开和客户端重连逻辑，顺便把截图里的报错也看看。',
      paths: ['/tmp/yonder-uploads/IMG_2041.png'],
      ts: t(14),
    },
    {
      id: 'r1',
      kind: 'reasoning',
      status: 'completed',
      text: '需要先确认中继是否有空闲超时；再看 web 客户端的 ping/pong 处理和 visibilitychange。截图显示的是 WebSocket 在后台被系统挂起后没有触发 onclose。',
      paths: [],
      ts: t(13),
    },
    {
      id: 'c1',
      kind: 'command',
      status: 'completed',
      title: 'rg -n "interval|ping" crates/yonder-relay/src',
      output:
        'crates/yonder-relay/src/conn.rs:42:        let mut ping = interval(Duration::from_secs(30));\ncrates/yonder-relay/src/conn.rs:56:                _ = ping.tick() => self.ws.send(Message::Ping(vec![])).await?,\ncrates/yonder-relay/src/main.rs:88:    // TODO: idle timeout',
      exit_code: 0,
      duration_ms: 212,
      paths: [],
      ts: t(12),
    },
    {
      id: 'c1b',
      kind: 'command',
      status: 'completed',
      title: 'sed -n 1,80p web/src/net/relay.ts',
      output: 'export class RelayLink {\n  private ping?: ReturnType<typeof setInterval>;\n  …',
      exit_code: 0,
      duration_ms: 18,
      paths: [],
      ts: t(12),
    },
    {
      id: 'v1',
      kind: 'tool',
      status: 'completed',
      title: '查看图片',
      paths: ['/Users/me/code/yonder/docs/relay-timeline.svg'],
      ts: t(11),
    },
    {
      id: 'f1',
      kind: 'file_change',
      status: 'completed',
      title: '修改 1 个文件',
      diff: MOCK_DIFF,
      paths: ['crates/yonder-relay/src/conn.rs'],
      ts: t(10),
    },
    {
      id: 'a1',
      kind: 'agent',
      status: 'completed',
      text: MOCK_AGENT_MD,
      paths: [],
      ts: t(9),
    },
    {
      id: 'a2',
      kind: 'agent',
      status: 'completed',
      text: '测试报告在 `~/code/yonder/docs/relay-report.pdf`，交互式时间线写成了 `docs/timeline.html`，开发服务器已在 http://localhost:5173/ 运行。',
      paths: [],
      ts: t(2),
    },
    {
      id: 'c2',
      kind: 'command',
      status: 'in_progress',
      title: 'cargo test -p yonder-relay -- --test-threads=1',
      output: '',
      paths: [],
      ts: t(1),
    },
  ];
}

export function mockApproval(now: number, id = 'ap1', item = 'c2'): Approval {
  return {
    id,
    kind: 'command',
    title: '运行中继集成测试',
    command: 'cargo test -p yonder-relay -- --test-threads=1',
    cwd: '/Users/me/code/yonder',
    reason: '需要网络端口 127.0.0.1:0 与临时目录写权限',
    detail: '{\n  "sandbox": "workspace-write",\n  "network": true,\n  "timeout_ms": 120000\n}',
    options: [
      { id: 'allow', label: '允许', kind: 'allow' },
      { id: 'allow_always', label: '本会话始终允许', kind: 'allow_always' },
      { id: 'deny', label: '拒绝', kind: 'deny' },
      { id: 'abort', label: '拒绝并停止', kind: 'abort' },
    ],
    item,
    ts: now - 30_000,
  };
}

export const MOCK_TEST_OUTPUT = [
  '   Compiling yonder-proto v0.1.0 (/Users/me/code/yonder/crates/yonder-proto)\n',
  '   Compiling yonder-relay v0.1.0 (/Users/me/code/yonder/crates/yonder-relay)\n',
  '    Finished `test` profile [unoptimized + debuginfo] target(s) in 6.82s\n',
  '     Running tests/relay.rs\n\nrunning 7 tests\n',
  'test auth_rejects_bad_proof ... ok\ntest watch_presence ... ok\ntest open_offline_host ... ok\n',
  'test idle_timeout_closes ... ok\ntest link_frames_roundtrip ... ok\ntest close_link ... ok\ntest ping_pong ... ok\n',
  '\ntest result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; finished in 1.94s\n',
];

export function mockAgents(os: string): AgentAvailability[] {
  return [
    {
      agent: 'codex',
      available: true,
      version: '0.46.0',
      path: os === 'windows' ? 'C:\\Users\\me\\AppData\\Roaming\\npm\\codex.cmd' : '/opt/homebrew/bin/codex',
      chat: true,
      models: ['gpt-5.5', 'gpt-5.5-codex', 'gpt-5.6-sol'],
      default_model: 'gpt-5.5',
    },
    {
      agent: 'claude',
      available: true,
      version: '2.1.3',
      chat: true,
      models: ['opus', 'sonnet', 'haiku'],
      default_model: 'sonnet',
    },
    { agent: 'pi', available: os !== 'linux', version: '0.9.1', chat: true, models: [], default_model: undefined },
  ];
}

// Pre-rendered terminal history with colors.
export function mockTerminalIntro(cwd: string, host: string): string {
  const p = mockPrompt(cwd, host);
  return [
    `Last login: Sat Sep 27 09:12:44 on ttys004\r\n`,
    `${p}git status -sb\r\n`,
    `\x1b[32m## main\x1b[0m...\x1b[31morigin/main\x1b[0m [ahead 2]\r\n`,
    ` \x1b[31mM\x1b[0m crates/yonder-relay/src/conn.rs\r\n`,
    ` \x1b[31mM\x1b[0m web/src/net/relay.ts\r\n`,
    `\x1b[31m??\x1b[0m docs/adr/0004-web-push.md\r\n`,
    `${p}cargo build --release -p yonder-host\r\n`,
    `\x1b[1;32m   Compiling\x1b[0m yonder-proto v0.1.0\r\n`,
    `\x1b[1;32m   Compiling\x1b[0m yonder-host v0.1.0\r\n`,
    `\x1b[1;32m    Finished\x1b[0m \`release\` profile [optimized] target(s) in 41.27s\r\n`,
    p,
  ].join('');
}

export function mockPrompt(cwd: string, host: string): string {
  const short = cwd.replace(/^\/Users\/me|^\/home\/me/, '~');
  return `\x1b[1;36m${host}\x1b[0m \x1b[1;34m${short}\x1b[0m \x1b[35m❯\x1b[0m `;
}

const H = 3_600_000;

/** Agent history of the mock hosts (newest first). */
export function MOCK_HISTORY(home: string): Array<AgentSessionSummary & { hidden?: boolean }> {
  const now = Date.now();
  const sep = home.includes('\\') ? '\\' : '/';
  const p = (...parts: string[]) => [home, ...parts].join(sep);
  const rows: Array<[AgentSessionSummary['agent'], string, string | undefined, string, number, string, boolean?]> = [
    ['codex', '同步 Codex 配置到艺龙', '把本机的 codex 配置、skill 和 MCP 同步到翼龙上', p('run', 'yonder'), 0.4, 'desktop'],
    ['claude', '中继断线重连', undefined, p('run', 'yonder'), 2, 'cli'],
    ['codex', '检查项目与官网现状', '看看这个 benchmark 有没有写死答案', p('run', 'arcbench'), 20, 'desktop'],
    ['pi', '规划交流', '51 号 issue 的 5 条怎么分工', p('run', 'ktv'), 26, 'cli'],
    ['codex', 'Run exactly this shell command and nothing else', undefined, '/tmp/yonder-e2e-x', 30, 'yonder', true],
    ['codex', '模拟器调度', '把 51 第 2、4、5 条的取证排进模拟器队列', p('run', 'ktv'), 40, 'desktop'],
    ['claude', '超级美声开发', '实现超级美声 API 并部署到线上', p('run', 'sing-demo'), 3 * 24, 'sdk'],
    ['codex', 'PTY 快照渲染与回放', undefined, p('run', 'yonder'), 5 * 24, 'yonder'],
    ['codex', '给 host 加 Windows 服务安装', undefined, p('run', 'yonder'), 9 * 24, 'cli'],
    ['pi', '会话 JSONL 脱敏', '扫描会话里的密钥并生成脱敏 fork', p('run', 'pi-session-lab'), 18 * 24, 'cli'],
    ['claude', '论文格式检查', undefined, p('Documents'), 40 * 24, 'cli'],
  ];
  const out: Array<AgentSessionSummary & { hidden?: boolean }> = rows.map(([agent, title, preview, cwd, hours, source, hidden], i) => ({
    id: `019a${(0x1000 + i).toString(16)}-mock-history`,
    agent,
    title,
    preview,
    cwd,
    updated_at: now - hours * H,
    source,
    model: agent === 'codex' ? 'gpt-5.5' : agent === 'claude' ? 'opus' : 'deepseek/v4',
    hidden,
    active: hours < 0.02,
  }));
  // Enough older ones to page through.
  for (let i = 0; i < 60; i++) {
    out.push({ id: `019b${i.toString(16).padStart(4, '0')}-mock-old`, agent: 'codex', title: `旧会话 ${i + 1}`, cwd: p('run', 'tmp'), updated_at: now - (45 + i) * 24 * H, source: 'desktop', model: 'gpt-5.5', active: false });
  }
  return out;
}

/** A small valid PDF with one text line per page (xref offsets computed). */
export function mockPdf(pages: string[]): string {
  const objs: string[] = [];
  const kids = pages.map((_, i) => `${3 + i * 2} 0 R`).join(' ');
  objs.push('<< /Type /Catalog /Pages 2 0 R >>');
  objs.push(`<< /Type /Pages /Kids [${kids}] /Count ${pages.length} >>`);
  const font = 3 + pages.length * 2;
  pages.forEach((text, i) => {
    const stream = `BT /F1 28 Tf 60 720 Td (${text}) Tj ET\n0.15 0.39 0.92 rg 60 600 480 60 re f`;
    objs.push(`<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 ${font} 0 R >> >> /Contents ${4 + i * 2} 0 R >>`);
    objs.push(`<< /Length ${stream.length} >>\nstream\n${stream}\nendstream`);
  });
  objs.push('<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>');
  let out = '%PDF-1.4\n';
  const offs: number[] = [];
  objs.forEach((o, i) => {
    offs.push(out.length);
    out += `${i + 1} 0 obj\n${o}\nendobj\n`;
  });
  const xref = out.length;
  out += `xref\n0 ${objs.length + 1}\n0000000000 65535 f \n${offs.map((o) => `${String(o).padStart(10, '0')} 00000 n \n`).join('')}`;
  out += `trailer\n<< /Size ${objs.length + 1} /Root 1 0 R >>\nstartxref\n${xref}\n%%EOF\n`;
  return out;
}

export const MOCK_TIMELINE_SVG =
  '<svg xmlns="http://www.w3.org/2000/svg" width="640" height="300" viewBox="0 0 640 300"><rect width="640" height="300" fill="#f8fafc"/><line x1="40" y1="150" x2="600" y2="150" stroke="#94a3b8" stroke-width="2"/><circle cx="90" cy="150" r="14" fill="#2563eb"/><circle cx="250" cy="150" r="14" fill="#0d9488"/><circle cx="410" cy="150" r="14" fill="#f59e0b"/><circle cx="560" cy="150" r="14" fill="#dc2626"/><g font-family="sans-serif" font-size="15" fill="#0f172a" text-anchor="middle"><text x="90" y="200">connect</text><text x="250" y="200">idle 30s</text><text x="410" y="200">ping</text><text x="560" y="200">timeout</text></g></svg>';

export const MOCK_TIMELINE_HTML = `<!doctype html><html lang="zh"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>中继时间线</title><link rel="stylesheet" href="timeline.css"></head><body><h1>中继连接时间线</h1><ol id="list"></ol><script src="timeline.js"></script></body></html>`;
export const MOCK_TIMELINE_CSS = `body{font-family:-apple-system,system-ui,sans-serif;margin:24px;color:#0f172a}h1{font-size:22px}li{padding:8px 0;border-bottom:1px solid #e2e8f0}`;
export const MOCK_TIMELINE_JS = `for (const [t, e] of [['0s','connect'],['30s','ping'],['60s','ping'],['90s','idle timeout']]) { const li = document.createElement('li'); li.textContent = t + ' · ' + e; document.getElementById('list').append(li); }`;

/** Pages of the fake dev server at http://localhost:5173. */
export const MOCK_DEV_SERVER: Record<string, [string, string]> = {
  '/': ['text/html', '<!doctype html><html><head><meta name="viewport" content="width=device-width,initial-scale=1"><link rel="stylesheet" href="/src/app.css"></head><body><div id="app"></div><script type="module" src="/src/main.js"></script></body></html>'],
  '/src/app.css': ['text/css', 'body{margin:0;font-family:-apple-system,system-ui,sans-serif}#app{padding:24px}h1{color:#2563eb}'],
  '/src/main.js': ['text/javascript', 'import { title } from "./title.js"; document.getElementById("app").innerHTML = "<h1>" + title + "</h1><p>Vite dev server</p>";'],
  '/src/title.js': ['text/javascript', 'export const title = "yonder dev preview";'],
};
