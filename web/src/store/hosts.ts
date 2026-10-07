import { create } from 'zustand';
import type { HostHello } from '../proto/generated/HostHello';
import type { HostInfo } from '../proto/generated/HostInfo';
import type { PairPayload } from '../proto/generated/PairPayload';
import type { SessionInfo } from '../proto/generated/SessionInfo';
import { isMockMode, MOCK_HOSTS } from '../net/mock';
import { getProvider } from '../net/provider';
import { call, type ConnStatus, type HostConnection } from '../net/types';
import { loadHosts, saveHosts, type StoredHost } from '../storage';
import { useUi } from './ui';

export interface HostRuntime {
  status: ConnStatus;
  error?: string;
  hello?: HostHello;
  info?: HostInfo;
  sessions: Record<string, SessionInfo>;
  sessionsLoaded: boolean;
}

interface HostsState {
  loaded: boolean;
  hosts: StoredHost[];
  runtime: Record<string, HostRuntime>;
  init(): Promise<void>;
  refresh(host?: string): Promise<void>;
  pair(p: PairPayload): Promise<HostHello>;
  forget(host: string): Promise<void>;
  setLabel(host: string, label: string): Promise<void>;
  upsertSession(host: string, s: SessionInfo): void;
  removeSession(host: string, id: string): void;
}

const emptyRuntime = (): HostRuntime => ({ status: 'connecting', sessions: {}, sessionsLoaded: false });

type HostListener = (host: string, conn: HostConnection) => void;
const onlineHooks = new Set<HostListener>();

/**
 * Session changes seen per host, so a `list_sessions` answer that was produced before a
 * change (but arrives after it) cannot roll the change back.
 */
interface SessionChange {
  seq: number;
  id: string;
  session?: SessionInfo;
}
let changeSeq = 0;
const journal = new Map<string, SessionChange[]>();

function recordChange(host: string, id: string, session?: SessionInfo) {
  const list = journal.get(host) ?? [];
  list.push({ seq: ++changeSeq, id, session });
  if (list.length > 500) list.splice(0, list.length - 500);
  journal.set(host, list);
}

const newer = (a: SessionInfo | undefined, b: SessionInfo) => (!a || b.updated_at >= a.updated_at ? b : a);

/** Run `fn` each time a host link becomes online (e.g. to push subscriptions). */
export function onHostOnline(fn: HostListener): () => void {
  onlineHooks.add(fn);
  return () => onlineHooks.delete(fn);
}

export const hostLabel = (h: StoredHost, rt?: HostRuntime) => h.label || rt?.info?.name || h.host_name;

export const useHosts = create<HostsState>((set, get) => {
  const patchRt = (host: string, patch: Partial<HostRuntime>) =>
    set((s) => ({ runtime: { ...s.runtime, [host]: { ...(s.runtime[host] ?? emptyRuntime()), ...patch } } }));

  const wired = new WeakSet<HostConnection>();

  async function loadSessions(host: string, conn: HostConnection) {
    const start = changeSeq;
    const res = await call(conn, { op: 'list_sessions' }, 'sessions');
    const map: Record<string, SessionInfo> = {};
    for (const s of res.sessions) map[s.id] = s;
    for (const c of journal.get(host) ?? []) {
      if (c.seq <= start) continue;
      if (c.session) map[c.id] = newer(map[c.id], c.session);
      else delete map[c.id];
    }
    patchRt(host, { sessions: map, sessionsLoaded: true });
  }

  async function loadInfo(host: string, conn: HostConnection) {
    const info = await call(conn, { op: 'host_info' }, 'host_info');
    patchRt(host, { info: info.info });
    const stored = get().hosts.find((h) => h.host === host);
    if (stored && (stored.os !== info.info.os || stored.host_name !== info.info.name)) {
      const hosts = get().hosts.map((h) => (h.host === host ? { ...h, os: info.info.os, host_name: info.info.name } : h));
      set({ hosts });
      if (!isMockMode()) void saveHosts(hosts);
    }
  }

  async function loadHost(host: string, conn: HostConnection) {
    try {
      // host_info can take a second (agent detection); do not hold the session list for it.
      await Promise.all([loadSessions(host, conn), loadInfo(host, conn)]);
    } catch (err) {
      if (conn.status === 'online') useUi.getState().toast('error', `加载主机信息失败：${(err as Error).message}`);
    }
  }

  function wire(host: string, conn: HostConnection) {
    if (wired.has(conn)) return;
    wired.add(conn);
    const sync = () => {
      patchRt(host, { status: conn.status, error: conn.error, hello: conn.hostHello });
      if (conn.status === 'online') {
        void loadHost(host, conn);
        onlineHooks.forEach((fn) => fn(host, conn));
      }
    };
    conn.onStatus(sync);
    conn.onEvent((e) => {
      switch (e.ev) {
        case 'session_updated':
          get().upsertSession(host, e.session);
          break;
        case 'session_removed':
          get().removeSession(host, e.session);
          break;
        case 'notice':
          useUi.getState().toast(e.level, e.message);
          break;
        case 'device_paired':
          useUi.getState().toast('info', `新设备已配对：${e.device.name}`);
          break;
      }
    });
    sync();
  }

  return {
    loaded: false,
    hosts: [],
    runtime: {},

    async init() {
      if (get().loaded) return;
      const hosts = isMockMode() ? MOCK_HOSTS : await loadHosts();
      const runtime: Record<string, HostRuntime> = {};
      for (const h of hosts) runtime[h.host] = emptyRuntime();
      set({ hosts, runtime, loaded: true });
      const provider = getProvider();
      provider.onConnection((host, conn) => wire(host, conn));
      provider.sync(hosts);
    },

    async refresh(host) {
      const targets = host ? [host] : get().hosts.map((h) => h.host);
      await Promise.all(
        targets.map(async (h) => {
          const conn = getProvider().get(h);
          if (!conn) return;
          if (conn.status === 'online') await loadHost(h, conn);
          else conn.wake();
        }),
      );
    },

    async pair(p) {
      const { hello } = await getProvider().pair(p);
      const entry: StoredHost = { host: p.host, host_name: hello.host_name || p.host_name, relay: p.relay, paired_at: Date.now(), os: hello.os };
      const hosts = [...get().hosts.filter((h) => h.host !== p.host), entry];
      set({ hosts, runtime: { ...get().runtime, [p.host]: get().runtime[p.host] ?? emptyRuntime() } });
      if (!isMockMode()) await saveHosts(hosts);
      getProvider().sync(hosts);
      return hello;
    },

    async forget(host) {
      const conn = getProvider().get(host);
      if (conn?.status === 'online') await conn.request({ op: 'push_unsubscribe' }).catch(() => undefined);
      const hosts = get().hosts.filter((h) => h.host !== host);
      const runtime = { ...get().runtime };
      delete runtime[host];
      set({ hosts, runtime });
      if (!isMockMode()) await saveHosts(hosts);
      getProvider().sync(hosts);
    },

    async setLabel(host, label) {
      const hosts = get().hosts.map((h) => (h.host === host ? { ...h, label: label.trim() || undefined } : h));
      set({ hosts });
      if (!isMockMode()) await saveHosts(hosts);
    },

    upsertSession(host, s) {
      recordChange(host, s.id, s);
      const rt = get().runtime[host] ?? emptyRuntime();
      patchRt(host, { sessions: { ...rt.sessions, [s.id]: newer(rt.sessions[s.id], s) } });
    },

    removeSession(host, id) {
      recordChange(host, id);
      const rt = get().runtime[host];
      if (!rt?.sessions[id]) return;
      const sessions = { ...rt.sessions };
      delete sessions[id];
      patchRt(host, { sessions });
    },
  };
});

export function sortedSessions(rt?: HostRuntime): SessionInfo[] {
  if (!rt) return [];
  return Object.values(rt.sessions).sort((a, b) => b.updated_at - a.updated_at);
}
