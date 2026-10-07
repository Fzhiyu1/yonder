import type { AgentKind } from '../proto/generated/AgentKind';
import type { ChatStatus } from '../proto/generated/ChatStatus';
import type { DeviceInfo } from '../proto/generated/DeviceInfo';
import type { Event } from '../proto/generated/Event';
import type { FileEntry } from '../proto/generated/FileEntry';
import type { HostHello } from '../proto/generated/HostHello';
import type { HostInfo } from '../proto/generated/HostInfo';
import type { PairPayload } from '../proto/generated/PairPayload';
import type { Request } from '../proto/generated/Request';
import type { Response } from '../proto/generated/Response';
import type { SessionInfo } from '../proto/generated/SessionInfo';
import type { SessionSpec } from '../proto/generated/SessionSpec';
import type { StoredHost } from '../storage';
import { b64ToBytes, bytesToB64, utf8Decode, utf8Encode } from '../lib/base64';
import { Emitter } from '../lib/emitter';
import { applyChatEvent, chatFromSnapshot, type ChatEvent, type ChatState } from '../store/chatReducer';
import { applyThreadEvent, threadFromPage, type ThreadState } from '../lib/thread';
import type { ChatItem } from '../proto/generated/ChatItem';
import type { ConnectionProvider } from './manager';
import {
  MOCK_AGENT_MD,
  MOCK_DEV_SERVER,
  MOCK_HISTORY,
  MOCK_REPLY_MD,
  MOCK_TEST_OUTPUT,
  MOCK_TIMELINE_CSS,
  MOCK_TIMELINE_HTML,
  MOCK_TIMELINE_JS,
  MOCK_TIMELINE_SVG,
  mockAgents,
  mockApproval,
  mockChatItems,
  mockDeploySubagent,
  mockSubagentThread,
  MOCK_SUB_THREAD,
  mockPdf,
  mockPrompt,
  mockTerminalIntro,
} from './mockData';
import { FEATURE_SUBAGENTS, RequestError, type ConnStatus, type HostConnection } from './types';

/** Items in the `attached` snapshot (the host sends the newest page only). */
const MOCK_PAGE = 40;

export function isMockMode(): boolean {
  if (typeof location === 'undefined') return false;
  return new URLSearchParams(location.search).get('mock') === '1';
}

const now = () => Date.now();
const HOUR = 3_600_000;

export const MOCK_HOSTS: StoredHost[] = [
  {
    host: 'Z1hHL-dorBeR1UaSDh1I0MDRKj5QeMs-7E57uB2IAV0',
    host_name: 'MacBook',
    relay: 'wss://relay.example.com/v1/ws',
    paired_at: now() - 30 * 24 * HOUR,
    os: 'macos',
  },
  {
    host: 'dGYGR2OrMJTg2mU20Z89weghdwbhLs7bnlh0M1lhq2w',
    host_name: 'linux-box',
    relay: 'wss://relay.example.com/v1/ws',
    paired_at: now() - 12 * 24 * HOUR,
    os: 'linux',
  },
  {
    host: 'dfGDZGdP8E6zxhN9z5QwSTQwYg6s39Q_CbiXMU4-4Sg',
    host_name: '5070',
    relay: 'wss://relay.example.com/v1/ws',
    paired_at: now() - 3 * 24 * HOUR,
    os: 'windows',
  },
];

interface MockFile {
  entry: FileEntry;
  data?: Uint8Array;
}

interface TermState {
  out: Uint8Array[];
  offset: number;
  line: string;
  cwd: string;
}

const delay = (ms: number) => new Promise((r) => setTimeout(r, ms));

function fp(pub: string): string {
  let h = 0x811c9dc5;
  const out: string[] = [];
  for (let round = 0; round < 4; round++) {
    for (const c of pub) h = Math.imul(h ^ (c.charCodeAt(0) + round), 16777619) >>> 0;
    out.push((h & 0xffff).toString(16).padStart(4, '0'));
  }
  return out.join('-');
}

class MockHost implements HostConnection {
  status: ConnStatus;
  error?: string;
  hostHello?: HostHello;
  info: HostInfo;

  private events = new Emitter<Event>();
  private statusEm = new Emitter<ConnStatus>();
  private reconnectedEm = new Emitter<void>();
  private sessions = new Map<string, SessionInfo>();
  private chats = new Map<string, ChatState>();
  /** Sub-agent threads per chat (the chat state itself leaves their items out, like the host). */
  private threads = new Map<string, Map<string, ThreadState>>();
  private terms = new Map<string, TermState>();
  private attached = new Set<string>();
  private files = new Map<string, MockFile>();
  private uploads = new Map<string, Uint8Array[]>();
  private timers = new Map<string, ReturnType<typeof setTimeout>[]>();
  private devices: DeviceInfo[];

  constructor(
    readonly host: string,
    name: string,
    os: string,
    online: boolean,
  ) {
    const home = os === 'windows' ? 'C:\\Users\\me' : os === 'linux' ? '/home/me' : '/Users/me';
    this.status = online ? 'online' : 'offline';
    this.error = online ? undefined : 'host_offline';
    this.info = {
      name,
      hostname: name.toLowerCase() + (os === 'macos' ? '.local' : ''),
      os,
      arch: os === 'macos' ? 'aarch64' : 'x86_64',
      version: '0.1.0',
      home,
      shell: os === 'windows' ? 'pwsh.exe' : os === 'linux' ? '/bin/bash' : '/bin/zsh',
      agents: mockAgents(os),
      recent_dirs: os === 'windows' ? [] : [`${home}/code/yonder`, `${home}/code/blog`, `${home}/run/tmp`],
      permissions: ['sessions', 'files'],
      fs_roots: [home],
      path_sep: os === 'windows' ? '\\' : '/',
      fingerprint: fp(host),
      vapid_public: 'BMock',
    };
    this.hostHello = online
      ? { protocol: 1, ok: true, host_name: name, os, version: '0.1.0', permissions: ['sessions', 'files'], features: [FEATURE_SUBAGENTS] }
      : undefined;
    this.devices = [
      { public: 'dWebThisDevice000000000000000000000000000000', name: '当前浏览器', client: 'web', paired_at: now() - 20 * 24 * HOUR, last_seen: now(), permissions: ['sessions', 'files'], current: true },
      { public: 'dIphone0000000000000000000000000000000000000', name: 'iPhone', client: 'ios', paired_at: now() - 25 * 24 * HOUR, last_seen: now() - 2 * HOUR, permissions: ['sessions', 'files'], current: false },
      { public: 'dCli00000000000000000000000000000000000000000', name: 'yonder-cli (linux-box)', client: 'cli', paired_at: now() - 9 * 24 * HOUR, last_seen: now() - 50 * HOUR, permissions: ['sessions'], current: false },
    ];
    if (online) {
      this.seedFiles();
      this.seedSessions(os);
    }
  }

  // ---- HostConnection ----

  onEvent(fn: (e: Event) => void) {
    return this.events.on(fn);
  }
  onStatus(fn: (s: ConnStatus) => void) {
    return this.statusEm.on(fn);
  }
  onReconnected(fn: () => void) {
    return this.reconnectedEm.on(fn);
  }
  wake() {}
  close() {
    for (const ts of this.timers.values()) ts.forEach(clearTimeout);
  }

  sendInput(session: string, data: Uint8Array): void {
    const s = this.sessions.get(session);
    const t = this.terms.get(session);
    if (!s || !t || s.state === 'exited') return;
    for (const ch of utf8Decode(data)) this.termKey(session, t, ch);
  }

  sendResize(session: string, cols: number, rows: number): void {
    const s = this.sessions.get(session);
    if (s) this.sessions.set(session, { ...s, cols, rows });
  }

  sendFocus(): void {}

  async request(req: Request): Promise<Response> {
    if (this.status !== 'online') throw new RequestError('unavailable', '主机离线');
    await delay(req.op.startsWith('fs_') ? 60 + Math.random() * 80 : 40 + Math.random() * 60);
    return this.handle(req);
  }

  // ---- request handling ----

  private handle(req: Request): Response {
    switch (req.op) {
      case 'ping':
        return { kind: 'pong', ts: now() };
      case 'host_info':
        return { kind: 'host_info', info: this.info };
      case 'list_sessions':
        return { kind: 'sessions', sessions: this.sortedSessions() };
      case 'create_session':
        return { kind: 'session', session: this.createSession(req.spec) };
      case 'attach':
        return this.attach(req.session, req.since);
      case 'chat_older': {
        const chat = this.chats.get(req.session);
        const end = chat ? chat.order.indexOf(req.before) : -1;
        if (!chat || end < 0) throw new RequestError('not_found', `item ${req.before}`);
        const start = Math.max(0, end - (req.limit ?? MOCK_PAGE));
        return { kind: 'chat_older', items: chat.order.slice(start, end).map((i) => chat.byId[i]), more: start > 0 };
      }
      case 'chat_thread': {
        const chat = this.chats.get(req.session);
        if (!chat) throw new RequestError('not_found', `session ${req.session}`);
        const t = this.threads.get(req.session)?.get(req.thread);
        if (!t) return { kind: 'chat_thread', items: [], more: false, seq: chat.seq };
        const end = req.before ? t.order.indexOf(req.before) : t.order.length;
        if (end < 0) return { kind: 'chat_thread', items: [], more: false, seq: chat.seq };
        const start = Math.max(0, end - (req.limit ?? 200));
        return { kind: 'chat_thread', items: t.order.slice(start, end).map((i) => t.byId[i]), more: start > 0, seq: chat.seq };
      }
      case 'detach':
        this.attached.delete(req.session);
        return { kind: 'ok' };
      case 'kill':
        this.kill(req.session);
        return { kind: 'ok' };
      case 'remove': {
        const s = this.need(req.session);
        if (s.state !== 'exited' && s.state !== 'failed') throw new RequestError('invalid', '会话仍在运行');
        this.sessions.delete(req.session);
        this.chats.delete(req.session);
        this.terms.delete(req.session);
        this.emit({ ev: 'session_removed', session: req.session });
        return { kind: 'ok' };
      }
      case 'rename':
        return { kind: 'session', session: this.update(req.session, { title: req.title }) };
      case 'continue_as_chat': {
        const s = this.need(req.session);
        this.kill(req.session);
        const created = this.createSession({ kind: 'chat', agent: s.agent, cwd: s.cwd, resume: s.agent_session, title: s.title });
        return { kind: 'session', session: created };
      }
      case 'chat_send':
        this.chatSend(req.session, req.text, req.attachments);
        return { kind: 'ok' };
      case 'chat_interrupt':
        this.clearTimers(req.session);
        this.chatEmit(req.session, { ev: 'chat_status', session: req.session, seq: 0, status: 'idle' });
        return { kind: 'ok' };
      case 'approval_respond':
        this.approvalRespond(req.session, req.approval, req.option);
        return { kind: 'ok' };
      case 'set_approval_mode': {
        const s = this.need(req.session);
        if (s.kind !== 'chat' || s.approval === undefined) throw new RequestError('unsupported', `${s.agent} 没有审批模式`);
        if (s.state === 'exited') throw new RequestError('invalid', '会话已结束');
        const updated = this.update(req.session, { approval: req.mode });
        // Full access answers what is pending, like the real adapters do.
        if (req.mode === 'yolo') {
          for (const a of this.chats.get(req.session)?.approvals ?? []) {
            if (a.kind !== 'question' && a.kind !== 'permission') this.approvalRespond(req.session, a.id, 'allow');
          }
        }
        return { kind: 'session', session: updated };
      }
      case 'set_chat_model': {
        const s = this.need(req.session);
        if (s.kind !== 'chat' || s.state === 'exited') throw new RequestError('invalid', '会话已结束');
        return { kind: 'session', session: this.update(req.session, { model: req.model }) };
      }
      case 'tailnet_url': {
        const u = new URL(req.url);
        if (!/^(localhost|127\.0\.0\.1|\[::1\])$/.test(u.hostname)) throw new RequestError('forbidden', 'only loopback');
        u.hostname = '100.64.0.10';
        return { kind: 'tailnet_url', url: u.href, reachable: u.port !== '3000' };
      }
      case 'http_fetch': {
        const u = new URL(req.url);
        if (!/^(localhost|127\.0\.0\.1|\[::1\])$/.test(u.hostname)) throw new RequestError('forbidden', 'only loopback');
        const page = u.port === '5173' ? MOCK_DEV_SERVER[u.pathname] : undefined;
        const body = utf8Encode(page ? page[1] : 'Not Found');
        return { kind: 'http_response', status: page ? 200 : 404, headers: [['content-type', page ? page[0] : 'text/plain']], data: bytesToB64(body) };
      }
      case 'agent_history': {
        const all = MOCK_HISTORY(this.info.home).filter((h) => (!req.agent || h.agent === req.agent) && (req.all || !h.hidden));
        const folders = new Map<string, number>();
        for (const h of all) if (h.cwd) folders.set(h.cwd, (folders.get(h.cwd) ?? 0) + 1);
        const words = (req.query ?? '').toLowerCase().split(/\s+/).filter(Boolean);
        const list = all.filter(
          (h) => (!req.cwd || h.cwd === req.cwd) && words.every((w) => `${h.title}\n${h.preview ?? ''}\n${h.cwd ?? ''}`.toLowerCase().includes(w)),
        );
        const offset = Number(req.cursor ?? 0);
        const limit = req.limit ?? 50;
        const page = list.slice(offset, offset + limit).map(({ hidden: _h, ...rest }) => rest);
        return {
          kind: 'agent_history',
          sessions: page,
          next_cursor: offset + page.length < list.length ? String(offset + page.length) : undefined,
          folders: offset === 0 ? [...folders].map(([path, count]) => ({ path, count })) : [],
          errors: [],
        };
      }
      case 'agent_preview': {
        const h = MOCK_HISTORY(this.info.home).find((x) => x.id === req.id);
        if (!h) throw new RequestError('not_found', '会话不存在');
        const items = [
          { id: `${h.id}-p0`, kind: 'user' as const, status: 'completed' as const, text: h.preview ?? h.title, paths: [], ts: 0 },
          { id: `${h.id}-p1`, kind: 'agent' as const, status: 'completed' as const, text: MOCK_AGENT_MD, paths: [], ts: 0 },
          { id: `${h.id}-p2`, kind: 'user' as const, status: 'completed' as const, text: '好，按这个改，改完跑一下测试', paths: [], ts: 0 },
          { id: `${h.id}-p3`, kind: 'agent' as const, status: 'completed' as const, text: '已改完，`cargo test` 全部通过（42 个）。', paths: [], ts: 0 },
        ];
        return { kind: 'agent_preview', items, truncated: true };
      }
      case 'fs_home':
        return { kind: 'path', path: this.info.home };
      case 'fs_list':
        return { kind: 'dir', listing: this.list(req.path, req.hidden) };
      case 'fs_stat': {
        const f = this.files.get(req.path);
        if (!f) throw new RequestError('not_found', '路径不存在');
        return { kind: 'stat', entry: f.entry };
      }
      case 'fs_read': {
        const f = this.files.get(req.path);
        if (!f || f.entry.kind !== 'file') throw new RequestError('not_found', '文件不存在');
        const data = f.data ?? new Uint8Array(f.entry.size).fill(0x2e);
        const chunk = data.subarray(req.offset, req.offset + Math.min(req.len, 1 << 20));
        return { kind: 'file_chunk', path: req.path, offset: req.offset, data: bytesToB64(chunk), eof: req.offset + chunk.length >= data.length, size: data.length };
      }
      case 'fs_write':
        return this.write(req.path, req.offset, req.data, req.finish, req.overwrite);
      case 'fs_mkdir': {
        if (this.files.has(req.path)) throw new RequestError('exists', '已存在同名项目');
        this.addFile(req.path, 'dir');
        return { kind: 'ok' };
      }
      case 'fs_rename': {
        const f = this.files.get(req.from);
        if (!f) throw new RequestError('not_found', '路径不存在');
        if (this.files.has(req.to) && !req.overwrite) throw new RequestError('exists', '目标已存在');
        for (const [p, v] of [...this.files]) {
          if (p === req.from || p.startsWith(req.from + this.info.path_sep)) {
            this.files.delete(p);
            const np = req.to + p.slice(req.from.length);
            this.files.set(np, { ...v, entry: { ...v.entry, path: np, name: np.slice(np.lastIndexOf(this.info.path_sep) + 1) } });
          }
        }
        return { kind: 'ok' };
      }
      case 'fs_delete': {
        if (!this.files.has(req.path)) throw new RequestError('not_found', '路径不存在');
        for (const p of [...this.files.keys()]) {
          if (p === req.path || p.startsWith(req.path + this.info.path_sep)) this.files.delete(p);
        }
        return { kind: 'ok' };
      }
      case 'upload_temp':
        return { kind: 'path', path: `/tmp/yonder-uploads/${Date.now().toString(36)}-${req.name}` };
      case 'list_devices':
        return { kind: 'devices', devices: this.devices };
      case 'revoke_device':
        this.devices = this.devices.filter((d) => d.public !== req.device);
        return { kind: 'ok' };
      case 'notify_test':
        setTimeout(() => this.events.emit({ ev: 'notice', level: 'info', message: `${this.info.name}：测试通知已发送` }), 300);
        return { kind: 'ok' };
      case 'push_subscribe':
      case 'push_unsubscribe':
        return { kind: 'ok' };
      default:
        throw new RequestError('unsupported', `不支持的操作 ${req.op}`);
    }
  }

  private need(id: string): SessionInfo {
    const s = this.sessions.get(id);
    if (!s) throw new RequestError('not_found', '会话不存在');
    return s;
  }

  private sortedSessions(): SessionInfo[] {
    return [...this.sessions.values()].sort((a, b) => b.updated_at - a.updated_at);
  }

  private emit(e: Event) {
    this.events.emit(e);
  }

  private update(id: string, patch: Partial<SessionInfo>): SessionInfo {
    const s = { ...this.need(id), ...patch, updated_at: patch.updated_at ?? now() };
    this.sessions.set(id, s);
    this.emit({ ev: 'session_updated', session: s });
    return s;
  }

  private later(session: string, ms: number, fn: () => void) {
    const t = setTimeout(fn, ms);
    const list = this.timers.get(session) ?? [];
    list.push(t);
    this.timers.set(session, list);
  }

  private clearTimers(session: string) {
    this.timers.get(session)?.forEach(clearTimeout);
    this.timers.delete(session);
  }

  // ---- sessions ----

  private newId(): string {
    return `s_${Math.random().toString(36).slice(2, 10)}`;
  }

  private baseSession(spec: SessionSpec, kind: 'chat' | 'terminal'): SessionInfo {
    const agent: AgentKind = spec.agent ?? (kind === 'chat' ? 'codex' : 'shell');
    const cwd = spec.cwd || this.info.home;
    const command =
      spec.command ?? (agent === 'shell' ? [this.info.shell, '-l'] : agent === 'custom' ? ['sh'] : [agent === 'claude' ? 'claude' : agent]);
    const gated = kind === 'chat' && (agent === 'codex' || agent === 'claude');
    return {
      id: this.newId(),
      kind,
      agent,
      title: spec.title || (kind === 'chat' ? (spec.prompt?.slice(0, 40) || '新对话') : command.join(' ')),
      command,
      cwd,
      origin: 'remote',
      state: 'running',
      pid: 40000 + Math.floor(Math.random() * 9999),
      created_at: now(),
      updated_at: now(),
      cols: spec.cols ?? 80,
      rows: spec.rows ?? 24,
      clients: 1,
      agent_session: kind === 'chat' || agent !== 'shell' ? (spec.resume ?? `019a${Math.random().toString(16).slice(2, 10)}-mock`) : undefined,
      chat_status: kind === 'chat' ? 'starting' : undefined,
      pending_approvals: 0,
      model: spec.model,
      approval: gated ? (spec.approval ?? this.info.agents.find((a) => a.agent === agent)?.default_approval ?? 'ask') : undefined,
      approval_live: gated,
    };
  }

  private createSession(spec: SessionSpec): SessionInfo {
    const kind = spec.kind ?? 'terminal';
    const s = this.baseSession(spec, kind);
    this.sessions.set(s.id, s);
    if (kind === 'chat') {
      this.chats.set(s.id, chatFromSnapshot({ items: [], approvals: [], status: 'starting', seq: 0, truncated: false }));
      this.emit({ ev: 'session_updated', session: s });
      this.later(s.id, 300, () => {
        this.chatEmit(s.id, { ev: 'chat_status', session: s.id, seq: 0, status: 'idle' });
        if (spec.resume) {
          this.chatEmit(s.id, { ev: 'chat_item', session: s.id, seq: 0, item: { id: 'sys-resume', kind: 'system', status: 'completed', text: `已恢复会话 ${spec.resume}`, paths: [], ts: now() } });
        }
        if (spec.prompt) this.chatSend(s.id, spec.prompt, []);
      });
    } else {
      const t: TermState = { out: [], offset: 0, line: '', cwd: s.cwd };
      this.terms.set(s.id, t);
      this.emit({ ev: 'session_updated', session: s });
      this.termWrite(s.id, t, s.agent === 'shell' || s.agent === 'custom' ? mockPrompt(s.cwd, this.info.name) : this.agentBanner(s));
    }
    return s;
  }

  private agentBanner(s: SessionInfo): string {
    const name = s.agent === 'claude' ? 'Claude Code' : s.agent === 'codex' ? 'Codex' : 'pi';
    return `\x1b[38;5;208m╭──────────────────────────────────────────╮\x1b[0m\r\n\x1b[38;5;208m│\x1b[0m \x1b[1m✻ ${name}\x1b[0m                                \x1b[38;5;208m│\x1b[0m\r\n\x1b[38;5;208m│\x1b[0m   cwd: ${s.cwd.padEnd(33).slice(0, 33)}\x1b[38;5;208m│\x1b[0m\r\n\x1b[38;5;208m╰──────────────────────────────────────────╯\x1b[0m\r\n\r\n\x1b[2m> 试试 "解释 relay.rs 的重连逻辑"\x1b[0m\r\n\r\n\x1b[1m>\x1b[0m `;
  }

  private kill(id: string) {
    const s = this.need(id);
    if (s.state === 'exited') return;
    this.clearTimers(id);
    const t = this.terms.get(id);
    if (t) this.termWrite(id, t, '\r\n\x1b[2m[进程已退出，代码 0]\x1b[0m\r\n');
    if (this.chats.has(id)) {
      const c = this.chats.get(id)!;
      for (const a of c.approvals) this.chatEmit(id, { ev: 'approval_resolved', session: id, seq: 0, approval: a.id, option: 'abort' });
      this.chatEmit(id, { ev: 'chat_status', session: id, seq: 0, status: 'exited' });
    }
    this.update(id, { state: 'exited', exit_code: 0, chat_status: this.chats.has(id) ? 'exited' : undefined, pending_approvals: 0, clients: 0 });
  }

  private attach(id: string, since?: number): Response {
    const s = this.need(id);
    this.attached.add(id);
    const chat = this.chats.get(id);
    if (chat) {
      // Like the host: only the newest page, older items via `chat_older`.
      const order = chat.order.slice(-MOCK_PAGE);
      return {
        kind: 'attached',
        session: s,
        chat: {
          items: order.map((i) => chat.byId[i]),
          approvals: chat.approvals,
          status: chat.status,
          seq: chat.seq,
          truncated: chat.truncated || order.length < chat.order.length,
        },
      };
    }
    const t = this.terms.get(id)!;
    const all = concat(t.out);
    const resume = since !== undefined && since <= t.offset && since >= 0;
    const data = resume ? all.subarray(since) : all;
    return { kind: 'attached', session: s, terminal: { reset: !resume, data: bytesToB64(data), offset: t.offset, cols: s.cols, rows: s.rows } };
  }

  // ---- chat simulation ----

  private chatEmit(session: string, e: ChatEvent) {
    const c = this.chats.get(session);
    if (!c) return;
    const ev = e.ev === 'chat_snapshot' ? e : ({ ...e, seq: c.seq + 1 } as ChatEvent);
    const { state } = applyChatEvent(c, ev);
    this.chats.set(session, state);
    const thread = ev.ev === 'chat_item' ? ev.item.thread : ev.ev === 'chat_delta' ? ev.thread : undefined;
    if (thread) {
      const ts = this.threads.get(session) ?? new Map<string, ThreadState>();
      this.threads.set(session, ts);
      ts.set(thread, applyThreadEvent(ts.get(thread) ?? threadFromPage([], false, 0), ev, thread));
    }
    if (this.attached.has(session)) this.emit(ev);
    const s = this.sessions.get(session);
    if (!s) return;
    const patch: Partial<SessionInfo> = {};
    if (e.ev === 'chat_status') patch.chat_status = e.status;
    if (e.ev === 'approval_requested' || e.ev === 'approval_resolved') patch.pending_approvals = state.approvals.length;
    if (e.ev === 'chat_item' && e.item.kind === 'agent' && e.item.text) patch.preview = e.item.text.replace(/\s+/g, ' ').slice(0, 120);
    if (Object.keys(patch).length) this.update(session, patch);
  }

  private setStatus(session: string, status: ChatStatus) {
    this.chatEmit(session, { ev: 'chat_status', session, seq: 0, status });
  }

  private streamText(session: string, itemId: string, field: 'text' | 'output', parts: string[], start: number, step: number, thread?: string): number {
    let t = start;
    for (const p of parts) {
      this.later(session, t, () => this.chatEmit(session, { ev: 'chat_delta', session, seq: 0, item: itemId, field, delta: p, thread }));
      t += step;
    }
    return t;
  }

  private threadItem(session: string, thread: string, id: string): ChatItem | undefined {
    return this.threads.get(session)?.get(thread)?.byId[id];
  }

  /** The sub-agent of the linux-box chat continues after its approval was answered. */
  private subagentRespond(session: string, thread: string, itemId: string | undefined, allowed: boolean, stop: boolean) {
    const item = itemId ? this.threadItem(session, thread, itemId) : undefined;
    const card = Object.values(this.chats.get(session)!.byId).find((i) => i.subagent?.id === thread);
    const finishCard = (status: 'done' | 'interrupted', reply?: string) => {
      const cur = Object.values(this.chats.get(session)!.byId).find((i) => i.subagent?.id === thread) ?? card;
      if (cur?.subagent) this.chatEmit(session, { ev: 'chat_item', session, seq: 0, item: { ...cur, status: status === 'done' ? 'completed' : 'declined', subagent: { ...cur.subagent, status, reply } } });
    };
    if (stop) {
      if (item) this.chatEmit(session, { ev: 'chat_item', session, seq: 0, item: { ...item, status: 'declined' } });
      finishCard('interrupted');
      this.setStatus(session, 'idle');
      return;
    }
    this.setStatus(session, 'working');
    let t = 300;
    if (item && allowed) {
      t = this.streamText(session, item.id, 'output', ['# configuration file /etc/nginx/nginx.conf:\n', 'http {\n    include /etc/nginx/sites-enabled/*;\n', '}\n# configuration file /etc/nginx/sites-enabled/relay.conf:\n', '    proxy_set_header Connection "upgrade";\n    proxy_read_timeout 300s;\n'], 300, 500, thread);
      this.later(session, t, () => {
        const cur = this.threadItem(session, thread, item.id)!;
        this.chatEmit(session, { ev: 'chat_item', session, seq: 0, item: { ...cur, status: 'completed', exit_code: 0, duration_ms: 2100 } });
      });
    } else if (item) {
      this.chatEmit(session, { ev: 'chat_item', session, seq: 0, item: { ...item, status: 'declined' } });
    }
    const reply = allowed ? 'Upgrade 与 Connection 头都已转发，proxy_read_timeout 为 300 秒，满足要求。' : '没有权限读取完整配置；站点文件里 Upgrade 头已转发，Connection 头和超时未能确认。';
    const mid = `yk_reply_${Date.now().toString(36)}`;
    t = this.streamText(session, mid, 'text', reply.match(/[\s\S]{1,6}/g) ?? [], t + 300, 60, thread);
    this.later(session, t, () => {
      const cur = this.threadItem(session, thread, mid)!;
      this.chatEmit(session, { ev: 'chat_item', session, seq: 0, item: { ...cur, status: 'completed' } });
      finishCard('done', reply);
    });
    this.later(session, t + 600, () => {
      const c = this.chats.get(session)!;
      const y3 = c.byId.y3;
      if (y3) this.chatEmit(session, { ev: 'chat_item', session, seq: 0, item: { ...y3, status: 'completed', text: '子智能体检查完了 nginx 配置。' } });
      this.chatEmit(session, { ev: 'chat_item', session, seq: 0, item: { id: `y_done_${Date.now().toString(36)}`, kind: 'agent', status: 'completed', text: allowed ? 'nginx 反代配置没问题，可以继续部署 relay。' : '部分配置未能确认，部署前请手动检查 `proxy_read_timeout`。', paths: [], ts: now() } });
      this.setStatus(session, 'idle');
    });
  }

  private chatSend(session: string, text: string, attachments: string[]) {
    const s = this.need(session);
    if (s.state === 'exited') throw new RequestError('invalid', '会话已结束');
    const uid = `u_${Date.now().toString(36)}`;
    this.chatEmit(session, { ev: 'chat_item', session, seq: 0, item: { id: uid, kind: 'user', status: 'completed', text, paths: attachments, ts: now() } });
    this.setStatus(session, 'working');
    const rid = `a_${Date.now().toString(36)}`;
    const chunks = MOCK_REPLY_MD.match(/[\s\S]{1,6}/g) ?? [];
    let t = this.streamText(session, rid, 'text', chunks, 500, 35);
    this.later(session, t, () => {
      const c = this.chats.get(session)!;
      this.chatEmit(session, { ev: 'chat_item', session, seq: 0, item: { ...c.byId[rid], status: 'completed' } });
    });
    t += 200;
    const cid = `c_${Date.now().toString(36)}`;
    this.later(session, t, () => {
      this.chatEmit(session, { ev: 'chat_item', session, seq: 0, item: { id: cid, kind: 'command', status: 'in_progress', title: 'cargo test -p yonder-relay -- --test-threads=1', output: '', paths: [], ts: now() } });
      this.chatEmit(session, { ev: 'approval_requested', session, seq: 0, approval: mockApproval(now(), `ap_${cid}`, cid) });
      this.setStatus(session, 'awaiting_approval');
    });
  }

  private approvalRespond(session: string, approvalId: string, option: string) {
    const c = this.chats.get(session);
    const ap = c?.approvals.find((a) => a.id === approvalId);
    if (!c || !ap) throw new RequestError('not_found', '审批已失效');
    const opt = ap.options.find((o) => o.id === option);
    this.chatEmit(session, { ev: 'approval_resolved', session, seq: 0, approval: approvalId, option });
    if (ap.thread) {
      this.subagentRespond(session, ap.thread, ap.item, opt?.kind === 'allow' || opt?.kind === 'allow_always', opt?.kind === 'abort');
      return;
    }
    const itemId = ap.item;
    const item = itemId ? this.chats.get(session)!.byId[itemId] : undefined;
    if (!opt || opt.kind === 'deny' || opt.kind === 'abort') {
      if (item) this.chatEmit(session, { ev: 'chat_item', session, seq: 0, item: { ...item, status: 'declined' } });
      this.setStatus(session, opt?.kind === 'abort' ? 'idle' : 'working');
      if (opt?.kind === 'deny') {
        this.later(session, 400, () => {
          this.chatEmit(session, { ev: 'chat_item', session, seq: 0, item: { id: `a_${Date.now().toString(36)}`, kind: 'agent', status: 'completed', text: '好的，不运行测试。改动已保留在工作区，你可以稍后手动执行 `cargo test -p yonder-relay`。', paths: [], ts: now() } });
          this.setStatus(session, 'idle');
        });
      }
      return;
    }
    this.setStatus(session, 'working');
    if (!itemId) return;
    const started = now();
    let t = this.streamText(session, itemId, 'output', MOCK_TEST_OUTPUT, 300, 450);
    this.later(session, t, () => {
      const cur = this.chats.get(session)!.byId[itemId];
      this.chatEmit(session, { ev: 'chat_item', session, seq: 0, item: { ...cur, status: 'completed', exit_code: 0, duration_ms: now() - started } });
    });
    t += 300;
    const aid = `a_${Date.now().toString(36)}_done`;
    t = this.streamText(session, aid, 'text', ['7 个测试全部通过。', '空闲超时与客户端 ping 超时都已生效，', '锁屏恢复后会在 10 秒内自动重连并用 `since` 补齐消息。'], t, 250);
    this.later(session, t, () => {
      const cur = this.chats.get(session)!.byId[aid];
      this.chatEmit(session, { ev: 'chat_item', session, seq: 0, item: { ...cur, status: 'completed' } });
      this.setStatus(session, 'idle');
    });
  }

  // ---- terminal simulation ----

  private termWrite(session: string, t: TermState, text: string) {
    const bytes = utf8Encode(text);
    const offset = t.offset;
    t.out.push(bytes);
    t.offset += bytes.length;
    if (this.attached.has(session)) this.emit({ ev: 'pty_output', session, offset, data: bytesToB64(bytes) });
    const last = text.replace(/\x1b\[[0-9;]*m/g, '').split(/\r?\n/).filter((l) => l.trim()).pop();
    const s = this.sessions.get(session);
    if (s && last) this.sessions.set(session, { ...s, preview: last.slice(0, 120), updated_at: now() });
  }

  private termKey(session: string, t: TermState, ch: string) {
    const s = this.sessions.get(session)!;
    const prompt = s.agent === 'shell' || s.agent === 'custom' ? mockPrompt(t.cwd, this.info.name) : '\x1b[1m>\x1b[0m ';
    if (ch === '\r') {
      const cmd = t.line.trim();
      t.line = '';
      let out = '\r\n';
      if (s.agent !== 'shell' && s.agent !== 'custom') {
        out += cmd ? `\x1b[2m⏺ 正在思考…\x1b[0m\r\n\x1b[37m${cmd.length > 20 ? '好的，我来看看。' : '收到。'}\x1b[0m\r\n\r\n` : '';
      } else if (cmd === 'clear') {
        out = '\x1b[2J\x1b[H';
      } else if (cmd === 'pwd') {
        out += t.cwd + '\r\n';
      } else if (cmd === 'ls') {
        out += '\x1b[1;34mcrates\x1b[0m  \x1b[1;34mdocs\x1b[0m  \x1b[1;34mios\x1b[0m  \x1b[1;34mweb\x1b[0m  Cargo.lock  Cargo.toml  README.md\r\n';
      } else if (cmd === 'date') {
        out += new Date().toString() + '\r\n';
      } else if (cmd.startsWith('echo ')) {
        out += cmd.slice(5) + '\r\n';
      } else if (cmd === 'exit') {
        this.termWrite(session, t, '\r\nlogout\r\n');
        this.kill(session);
        return;
      } else if (cmd) {
        out += `zsh: command not found: ${cmd.split(' ')[0]}\r\n`;
      }
      this.termWrite(session, t, out + prompt);
    } else if (ch === '\x7f' || ch === '\b') {
      if (t.line.length) {
        t.line = t.line.slice(0, -1);
        this.termWrite(session, t, '\b \b');
      }
    } else if (ch === '\x03') {
      t.line = '';
      this.termWrite(session, t, '^C\r\n' + prompt);
    } else if (ch === '\x04') {
      if (!t.line) {
        this.termWrite(session, t, '\r\nlogout\r\n');
        this.kill(session);
      }
    } else if (ch === '\x0c') {
      this.termWrite(session, t, '\x1b[2J\x1b[H' + prompt + t.line);
    } else if (ch >= ' ' && ch !== '\x1b') {
      t.line += ch;
      this.termWrite(session, t, ch);
    }
  }

  // ---- files ----

  private addFile(path: string, kind: 'file' | 'dir', data?: Uint8Array, mtime = now(), size?: number) {
    const sep = this.info.path_sep;
    const name = path.slice(path.lastIndexOf(sep) + 1);
    this.files.set(path, {
      entry: { name, path, kind, size: kind === 'dir' ? 0 : (size ?? data?.length ?? 0), mtime, readonly: false, hidden: name.startsWith('.') },
      data,
    });
  }

  private seedFiles() {
    const h = this.info.home;
    const d = (p: string, age = 2) => this.addFile(`${h}${p}`, 'dir', undefined, now() - age * HOUR);
    const f = (p: string, text: string, age = 3) => this.addFile(`${h}${p}`, 'file', utf8Encode(text), now() - age * HOUR);
    const big = (p: string, size: number, age = 20) => this.addFile(`${h}${p}`, 'file', undefined, now() - age * HOUR, size);
    d('/code', 1);
    d('/code/yonder', 1);
    d('/code/yonder/crates', 1);
    d('/code/yonder/docs', 5);
    f('/code/yonder/docs/relay-timeline.svg', MOCK_TIMELINE_SVG, 1);
    f('/code/yonder/docs/relay-report.pdf', mockPdf(['Relay test report', 'Idle timeout: 90 s', 'Reconnect: OK']), 1);
    f('/code/yonder/docs/timeline.html', MOCK_TIMELINE_HTML, 1);
    f('/code/yonder/docs/timeline.css', MOCK_TIMELINE_CSS, 1);
    f('/code/yonder/docs/timeline.js', MOCK_TIMELINE_JS, 1);
    d('/code/yonder/web', 1);
    d('/code/yonder/.git', 1);
    f('/code/yonder/README.md', '# yonder\n\nDrive CLI coding agents on your own computers from your phone.\n\n- End-to-end encrypted (Noise IK)\n- Relay sees ciphertext only\n', 30);
    f('/code/yonder/Cargo.toml', '[workspace]\nmembers = ["crates/*"]\nresolver = "2"\n', 48);
    f('/code/yonder/docs/client-protocol.md', '# Client protocol guide\n\nHow a client talks to a yonder host.\n', 5);
    f('/code/yonder/.gitignore', '/target\n.DS_Store\n', 200);
    d('/code/blog', 72);
    d('/Documents', 30);
    d('/Downloads', 4);
    d('/Pictures', 90);
    d('/.config', 100);
    d('/.ssh', 400);
    f('/notes.md', '# 待办\n\n- [x] 中继空闲断开\n- [ ] Web Push 在 iOS 上验证\n- [ ] 文件管理器批量上传\n', 6);
    f(
      '/Pictures/diagram.svg',
      '<svg xmlns="http://www.w3.org/2000/svg" width="240" height="120" viewBox="0 0 240 120"><rect width="240" height="120" fill="#f4f4f5"/><rect x="12" y="40" width="60" height="40" rx="6" fill="#2563eb"/><rect x="90" y="40" width="60" height="40" rx="6" fill="#71717a"/><rect x="168" y="40" width="60" height="40" rx="6" fill="#059669"/><text x="42" y="65" font-size="11" text-anchor="middle" fill="#fff" font-family="sans-serif">phone</text><text x="120" y="65" font-size="11" text-anchor="middle" fill="#fff" font-family="sans-serif">relay</text><text x="198" y="65" font-size="11" text-anchor="middle" fill="#fff" font-family="sans-serif">host</text></svg>',
      90,
    );
    big('/Downloads/yonder-host-0.1.0-aarch64-apple-darwin.tar.gz', 6_842_113, 4);
    big('/Downloads/会议录音 0926.m4a', 18_220_551, 26);
    f('/Documents/周报-0926.md', '## 本周\n\n- yonder 中继上线\n- Web 客户端 PWA\n', 30);
  }

  private list(path: string, hidden: boolean) {
    const sep = this.info.path_sep;
    const dir = this.files.get(path);
    if (path !== this.info.home && (!dir || dir.entry.kind !== 'dir')) throw new RequestError('not_found', '文件夹不存在');
    const entries = [...this.files.values()]
      .map((f) => f.entry)
      .filter((e) => e.path.slice(0, e.path.lastIndexOf(sep)) === path && (hidden || !e.hidden))
      .sort((a, b) => (a.kind === 'dir' ? 0 : 1) - (b.kind === 'dir' ? 0 : 1) || a.name.localeCompare(b.name));
    const parent = path === this.info.home ? undefined : path.slice(0, path.lastIndexOf(sep)) || sep;
    return { path, parent, entries, truncated: false };
  }

  private write(path: string, offset: number, data: string, finish: boolean, overwrite: boolean): Response {
    if (offset === 0) {
      if (this.files.has(path) && !overwrite) throw new RequestError('exists', '目标文件已存在');
      this.uploads.set(path, []);
    }
    const parts = this.uploads.get(path);
    if (!parts) throw new RequestError('invalid', '上传未开始');
    const bytes = b64ToBytes(data);
    const have = parts.reduce((n, p) => n + p.length, 0);
    if (have !== offset) throw new RequestError('invalid', 'offset 不连续');
    parts.push(bytes);
    if (finish) {
      this.uploads.delete(path);
      this.addFile(path, 'file', concat(parts));
    }
    return { kind: 'ok' };
  }

  // ---- seed sessions ----

  private seedSessions(os: string) {
    const h = this.info.home;
    const t = now();
    if (os === 'macos') {
      const chatId = 's_chat_relay';
      const items = mockChatItems(t);
      const approval = mockApproval(t);
      this.sessions.set(chatId, {
        id: chatId, kind: 'chat', agent: 'codex', title: '修复锁屏后中继连接假在线', command: ['codex', 'app-server'], cwd: `${h}/code/yonder`,
        origin: 'remote', state: 'running', created_at: t - 15 * 60_000, updated_at: t - 30_000, cols: 80, rows: 24, clients: 1,
        agent_session: '019a4d5e-8f10-7d21-b3c4-5e6f7a8b9c0d', chat_status: 'awaiting_approval', pending_approvals: 1, model: 'gpt-5.5', approval: 'ask', approval_live: true,
        preview: '接下来跑一遍中继的集成测试确认没有回归。',
      });
      this.chats.set(chatId, chatFromSnapshot({ items, approvals: [approval], status: 'awaiting_approval', seq: 41, truncated: false }));
      this.threads.set(chatId, new Map([[MOCK_SUB_THREAD, threadFromPage(mockSubagentThread(t), false, 41)]]));

      const termId = 's_term_zsh';
      this.sessions.set(termId, {
        id: termId, kind: 'terminal', agent: 'shell', title: 'zsh', command: ['/bin/zsh', '-l'], cwd: `${h}/code/yonder`, origin: 'local',
        state: 'running', pid: 51234, created_at: t - 3 * HOUR, updated_at: t - 4 * 60_000, cols: 100, rows: 30, clients: 0, pending_approvals: 0, approval_live: false,
        preview: 'Finished `release` profile [optimized] target(s) in 41.27s',
      });
      const term: TermState = { out: [], offset: 0, line: '', cwd: `${h}/code/yonder` };
      this.terms.set(termId, term);
      const intro = utf8Encode(mockTerminalIntro(term.cwd, 'MacBook'));
      term.out.push(intro);
      term.offset = intro.length;

      const claudeId = 's_term_claude';
      this.sessions.set(claudeId, {
        id: claudeId, kind: 'terminal', agent: 'claude', title: 'claude', command: ['claude'], cwd: `${h}/code/blog`, origin: 'local',
        state: 'running', pid: 51500, created_at: t - 5 * HOUR, updated_at: t - 2 * HOUR, cols: 100, rows: 30, clients: 0, pending_approvals: 0, approval_live: false,
        agent_session: 'c3f1a2b4-5d6e-4f70-8a9b-0c1d2e3f4a5b', preview: '文章目录结构已经整理好',
      });
      const ct: TermState = { out: [], offset: 0, line: '', cwd: `${h}/code/blog` };
      this.terms.set(claudeId, ct);
      const banner = utf8Encode(this.agentBanner(this.sessions.get(claudeId)!));
      ct.out.push(banner);
      ct.offset = banner.length;

      const doneId = 's_chat_upload';
      this.sessions.set(doneId, {
        id: doneId, kind: 'chat', agent: 'claude', title: '为文件管理器加上传进度', command: ['claude'], cwd: `${h}/code/yonder/web`, origin: 'remote',
        state: 'exited', exit_code: 0, created_at: t - 26 * HOUR, updated_at: t - 25 * HOUR, cols: 80, rows: 24, clients: 0,
        agent_session: 'b7e2c9d1-0a3f-4e5b-9c8d-7f6e5d4c3b2a', chat_status: 'exited', pending_approvals: 0, model: 'sonnet', approval: 'auto', approval_live: false,
        preview: '上传进度与覆盖确认已完成，测试通过。',
      });
      this.chats.set(doneId, chatFromSnapshot({
        items: [
          { id: 'x1', kind: 'user', status: 'completed', text: '给文件管理器的上传加上进度条，已存在时询问是否覆盖。', paths: [], ts: t - 26 * HOUR },
          { id: 'x2', kind: 'plan', status: 'completed', text: '- [x] 256 KiB 分块上传\n- [x] 进度条\n- [x] `exists` 时确认覆盖\n- [x] 单元测试', paths: [], ts: t - 26 * HOUR },
          { id: 'x3', kind: 'tool', status: 'completed', title: 'Edit web/src/app/files/FileManager.tsx', output: 'Applied 3 edits', paths: ['web/src/app/files/FileManager.tsx'], ts: t - 25.5 * HOUR },
          { id: 'x4', kind: 'web_search', status: 'completed', title: 'MDN navigator.share files iOS Safari', paths: [], ts: t - 25.4 * HOUR },
          { id: 'x5', kind: 'error', status: 'failed', text: '第一次运行 `pnpm test` 超时（120 s），已重试。', paths: [], ts: t - 25.3 * HOUR },
          { id: 'x6', kind: 'agent', status: 'completed', text: '上传进度与覆盖确认已完成，测试通过。', paths: [], ts: t - 25 * HOUR },
          { id: 'x7', kind: 'system', status: 'completed', text: '会话已结束', paths: [], ts: t - 25 * HOUR },
        ],
        approvals: [], status: 'exited', seq: 88, truncated: true,
      }));
    } else {
      const id = 's_chat_deploy';
      const sub = mockDeploySubagent(t);
      this.sessions.set(id, {
        id, kind: 'chat', agent: 'claude', title: '部署中继到 relay-1', command: ['claude'], cwd: `${h}/deploy`, origin: 'remote', state: 'running',
        created_at: t - 50 * 60_000, updated_at: t - 6 * 60_000, cols: 80, rows: 24, clients: 0, agent_session: 'e1d2c3b4-a596-4877-8899-aabbccddeeff',
        chat_status: 'awaiting_approval', pending_approvals: 1, model: 'opus', approval: 'ask', approval_live: true, preview: sub.approval.title,
      });
      this.chats.set(id, chatFromSnapshot({
        items: [
          { id: 'y1', kind: 'user', status: 'completed', text: '把 yonder-relay 部署到 relay-1，走现有 nginx 反代。', paths: [], ts: t - 50 * 60_000 },
          { id: 'y2', kind: 'command', status: 'completed', title: 'nginx -t', output: 'nginx: configuration file /etc/nginx/nginx.conf test is successful', exit_code: 0, duration_ms: 88, paths: [], ts: t - 40 * 60_000 },
          { id: 'y3', kind: 'agent', status: 'in_progress', text: '让一个子智能体去核对 WebSocket 升级头和超时设置。', paths: [], ts: t - 7 * 60_000 },
          sub.card,
        ],
        approvals: [sub.approval], status: 'awaiting_approval', seq: 12, truncated: false,
      }));
      this.threads.set(id, new Map([[sub.card.id, threadFromPage(sub.thread, false, 12)]]));
      const tid = 's_term_htop';
      this.sessions.set(tid, {
        id: tid, kind: 'terminal', agent: 'shell', title: 'bash', command: ['/bin/bash', '-l'], cwd: h, origin: 'remote', state: 'exited', exit_code: 130,
        created_at: t - 30 * HOUR, updated_at: t - 29 * HOUR, cols: 80, rows: 24, clients: 0, pending_approvals: 0, approval_live: false, preview: 'logout',
      });
      const tt: TermState = { out: [utf8Encode(`${mockPrompt(h, 'linux-box')}journalctl -u yonder-host -n 3\r\nSep 26 08:01:12 linux-box yonder-host[912]: relay connected\r\nSep 26 08:01:12 linux-box yonder-host[912]: watching 2 devices\r\nSep 26 08:02:40 linux-box yonder-host[912]: link opened from iPhone\r\n${mockPrompt(h, 'linux-box')}exit\r\nlogout\r\n`)], offset: 0, line: '', cwd: h };
      tt.offset = tt.out[0].length;
      this.terms.set(tid, tt);
    }
  }
}

function concat(parts: Uint8Array[]): Uint8Array {
  const n = parts.reduce((a, p) => a + p.length, 0);
  const out = new Uint8Array(n);
  let o = 0;
  for (const p of parts) {
    out.set(p, o);
    o += p.length;
  }
  return out;
}

export class MockProvider implements ConnectionProvider {
  private hosts = new Map<string, MockHost>();
  private listeners = new Set<(host: string, conn: HostConnection) => void>();

  onConnection(fn: (host: string, conn: HostConnection) => void) {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  }

  get(host: string) {
    return this.hosts.get(host);
  }

  sync(hosts: StoredHost[]): void {
    for (const h of hosts) {
      if (this.hosts.has(h.host)) continue;
      const conn = new MockHost(h.host, h.host_name, h.os ?? 'linux', h.os !== 'windows');
      this.hosts.set(h.host, conn);
      this.listeners.forEach((fn) => fn(h.host, conn));
    }
    for (const [k, v] of this.hosts) {
      if (!hosts.some((h) => h.host === k)) {
        v.close();
        this.hosts.delete(k);
      }
    }
  }

  async pair(p: PairPayload) {
    await delay(600);
    if (p.exp <= Date.now()) throw new RequestError('pair_token_invalid', '配对码无效或已过期，请在主机上重新生成二维码');
    const conn = new MockHost(p.host, p.host_name, 'linux', true);
    this.hosts.set(p.host, conn);
    this.listeners.forEach((fn) => fn(p.host, conn));
    return { hello: conn.hostHello!, conn };
  }
}
