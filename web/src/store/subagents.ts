import { create } from 'zustand';
import { getConn } from '../net/provider';
import { call } from '../net/types';
import { applyThreadEvent, prependThread, threadFromPage, type ThreadState } from '../lib/thread';
import { toastError } from './ui';

export interface ThreadView extends Partial<ThreadState> {
  loading: boolean;
  loadingOlder?: boolean;
  error?: string;
}

interface SubagentStore {
  /** Sub-agent whose thread is open, per chat (chatKey). */
  open: Record<string, string | undefined>;
  /** Thread views by `${chatKey}#${thread}`. */
  views: Record<string, ThreadView>;
  show(chat: string, thread: string): void;
  hide(chat: string): void;
  set(key: string, fn: (v: ThreadView) => ThreadView): void;
}

export const threadKey = (chat: string, thread: string) => `${chat}#${thread}`;

export const useSubagents = create<SubagentStore>((set) => ({
  open: {},
  views: {},
  show(chat, thread) {
    set((s) => ({ open: { ...s.open, [chat]: thread } }));
  },
  hide(chat) {
    set((s) => ({ open: { ...s.open, [chat]: undefined } }));
  },
  set(key, fn) {
    set((s) => ({ views: { ...s.views, [key]: fn(s.views[key] ?? { loading: true }) } }));
  },
}));

const PAGE = 200;

/**
 * Loads one sub-agent's thread and keeps it live from the chat's events until the returned
 * cleanup runs. The chat itself must be attached (it receives the events). Events that arrive
 * while the page loads are applied on top of it; reconnects reload it.
 */
export function watchThread(host: string, session: string, chat: string, thread: string): () => void {
  const key = threadKey(chat, thread);
  const conn = getConn(host);
  const store = useSubagents.getState();
  if (!conn) {
    store.set(key, (v) => ({ ...v, loading: false, error: '主机未连接' }));
    return () => undefined;
  }
  let disposed = false;
  let loading = false;
  let early: Parameters<typeof applyThreadEvent>[1][] = [];

  const load = async () => {
    if (loading || disposed) return;
    loading = true;
    early = [];
    useSubagents.getState().set(key, (v) => ({ ...v, loading: !v.order, error: undefined }));
    try {
      const res = await call(conn, { op: 'chat_thread', session, thread, limit: PAGE }, 'chat_thread');
      if (disposed) return;
      let st = threadFromPage(res.items, res.more, res.seq);
      for (const e of early) st = applyThreadEvent(st, e, thread);
      early = [];
      useSubagents.getState().set(key, () => ({ ...st, loading: false }));
    } catch (err) {
      if (disposed) return;
      const unknown = /unknown variant|chat_thread/.test((err as Error).message);
      useSubagents.getState().set(key, (v) => ({ ...v, loading: false, error: unknown ? '主机版本过旧，无法查看子智能体的过程' : (err as Error).message }));
    } finally {
      loading = false;
    }
  };

  const offEvent = conn.onEvent((e) => {
    if ((e.ev !== 'chat_item' && e.ev !== 'chat_delta') || e.session !== session) return;
    if (loading) {
      early.push(e);
      return;
    }
    useSubagents.getState().set(key, (v) => {
      if (!v.order) return v;
      const next = applyThreadEvent(v as ThreadState, e, thread);
      return next === v ? v : { ...v, ...next };
    });
  });
  const offRe = conn.onReconnected(() => void load());
  void load();
  return () => {
    disposed = true;
    offEvent();
    offRe();
  };
}

/** Fetches the items before the oldest one shown. */
export async function loadOlderThread(host: string, session: string, chat: string, thread: string): Promise<void> {
  const key = threadKey(chat, thread);
  const v = useSubagents.getState().views[key];
  const conn = getConn(host);
  const before = v?.order?.[0];
  if (!conn || !v?.more || v.loadingOlder || before === undefined) return;
  useSubagents.getState().set(key, (x) => ({ ...x, loadingOlder: true }));
  try {
    const res = await call(conn, { op: 'chat_thread', session, thread, before, limit: PAGE }, 'chat_thread');
    useSubagents.getState().set(key, (x) => (x.order?.[0] === before ? { ...x, ...prependThread(x as ThreadState, res.items, res.more), loadingOlder: false } : { ...x, loadingOlder: false }));
  } catch (err) {
    useSubagents.getState().set(key, (x) => ({ ...x, loadingOlder: false }));
    toastError('加载更早的内容失败', err);
  }
}
