import { create } from 'zustand';
import type { SessionInfo } from '../proto/generated/SessionInfo';
import { getConn } from '../net/provider';
import { call } from '../net/types';
import { applyChatEvent, chatFromSnapshot, emptyChat, isChatEvent, keepOlder, prependOlder, type ChatState } from './chatReducer';
import { useHosts } from './hosts';
import { toastError } from './ui';

export interface PendingMessage {
  id: string;
  text: string;
  attachments: string[];
  ts: number;
  failed?: boolean;
}

export interface ChatView extends ChatState {
  pending: PendingMessage[];
  loading: boolean;
  /** Fetching items above the oldest one shown. */
  loadingOlder?: boolean;
  error?: string;
}

interface ChatStore {
  chats: Record<string, ChatView>;
  set(key: string, fn: (c: ChatView) => ChatView): void;
}

export const chatKey = (host: string, session: string) => `${host}/${session}`;

const blank = (): ChatView => ({ ...emptyChat(), pending: [], loading: true });

export const useChats = create<ChatStore>((set) => ({
  chats: {},
  set(key, fn) {
    set((s) => ({ chats: { ...s.chats, [key]: fn(s.chats[key] ?? blank()) } }));
  },
}));

/** Drop optimistic bubbles once the host echoes a matching user item. */
function reconcile(c: ChatView): ChatView {
  if (!c.pending.length) return c;
  const userTexts = new Set(
    c.order
      .map((id) => c.byId[id])
      .filter((i) => i.kind === 'user' && i.ts >= Math.min(...c.pending.map((p) => p.ts)) - 60_000)
      .map((i) => (i.text ?? '').trim()),
  );
  const pending = c.pending.filter((p) => p.failed || !userTexts.has(p.text.trim()));
  return pending.length === c.pending.length ? c : { ...c, pending };
}

/**
 * Attaches to a chat session and keeps its view in the store until the returned cleanup
 * runs. Handles seq gaps and link reconnects by re-attaching.
 */
export function attachChat(host: string, session: string): () => void {
  const key = chatKey(host, session);
  const conn = getConn(host);
  const store = useChats.getState();
  if (!conn) {
    store.set(key, (c) => ({ ...c, loading: false, error: '主机未连接' }));
    return () => undefined;
  }
  let disposed = false;
  let attaching = false;
  let queued: Parameters<typeof applyChatEvent>[1][] = [];

  const attach = async () => {
    if (attaching || disposed) return;
    attaching = true;
    queued = [];
    try {
      const res = await call(conn, { op: 'attach', session }, 'attached');
      if (disposed) return;
      useHosts.getState().upsertSession(host, res.session as SessionInfo);
      const snap = res.chat ?? { items: [], approvals: [], status: 'starting' as const, seq: 0, truncated: false };
      let state = chatFromSnapshot(snap);
      let gap = false;
      for (const ev of queued) {
        const r = applyChatEvent(state, ev);
        state = r.state;
        gap ||= r.gap;
      }
      queued = [];
      const fresh = state;
      useChats.getState().set(key, (c) => reconcile({ ...c, ...keepOlder(c, fresh), loading: false, error: undefined }));
      attaching = false;
      if (gap) void attach();
    } catch (err) {
      attaching = false;
      if (disposed) return;
      useChats.getState().set(key, (c) => ({ ...c, loading: false, error: (err as Error).message }));
    }
  };

  const offEvent = conn.onEvent((e) => {
    if (!isChatEvent(e) || e.session !== session) return;
    if (attaching) {
      queued.push(e);
      return;
    }
    let gap = false;
    useChats.getState().set(key, (c) => {
      const r = applyChatEvent(c, e);
      gap = r.gap;
      return r.state === c ? c : reconcile({ ...c, ...r.state });
    });
    if (gap) void attach();
  });
  const offRe = conn.onReconnected(() => void attach());
  const offStatus = conn.onStatus((s) => {
    if (s === 'online' && !useChats.getState().chats[key]?.attached) void attach();
  });
  if (conn.status === 'online') void attach();
  else useChats.getState().set(key, (c) => ({ ...c, loading: c.attached ? false : true }));

  return () => {
    disposed = true;
    offEvent();
    offRe();
    offStatus();
    if (conn.status === 'online') void conn.request({ op: 'detach', session }).catch(() => undefined);
    useChats.getState().set(key, (c) => ({ ...c, attached: false, seq: -1 }));
  };
}

/** Fetches the page of items before the oldest one shown (scrolling up). */
export async function loadOlder(host: string, session: string): Promise<void> {
  const key = chatKey(host, session);
  const c = useChats.getState().chats[key];
  const conn = getConn(host);
  const before = c?.order[0];
  if (!conn || !c || !c.truncated || c.loadingOlder || before === undefined) return;
  useChats.getState().set(key, (x) => ({ ...x, loadingOlder: true }));
  try {
    const res = await call(conn, { op: 'chat_older', session, before, limit: 40 }, 'chat_older');
    useChats.getState().set(key, (x) => (x.order[0] === before ? { ...x, ...prependOlder(x, res.items, res.more), loadingOlder: false } : { ...x, loadingOlder: false }));
  } catch (err) {
    // Hosts before paging do not know `chat_older`: nothing more to show.
    const unknown = /unknown variant|chat_older/.test((err as Error).message);
    useChats.getState().set(key, (x) => ({ ...x, loadingOlder: false, truncated: unknown ? false : x.truncated }));
    if (!unknown) toastError('加载更早的消息失败', err);
  }
}

export async function sendChat(host: string, session: string, text: string, attachments: string[]): Promise<void> {
  const conn = getConn(host);
  const key = chatKey(host, session);
  const id = `p_${Date.now().toString(36)}`;
  useChats.getState().set(key, (c) => ({ ...c, pending: [...c.pending, { id, text, attachments, ts: Date.now() }] }));
  try {
    if (!conn) throw new Error('主机未连接');
    await conn.request({ op: 'chat_send', session, text, attachments });
  } catch (err) {
    useChats.getState().set(key, (c) => ({ ...c, pending: c.pending.map((p) => (p.id === id ? { ...p, failed: true } : p)) }));
    toastError('发送失败', err);
  }
}

export function discardPending(host: string, session: string, id: string): void {
  useChats.getState().set(chatKey(host, session), (c) => ({ ...c, pending: c.pending.filter((p) => p.id !== id) }));
}
