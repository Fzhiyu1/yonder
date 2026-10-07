import { create } from 'zustand';
import { getConn } from '../net/provider';
import { call, FEATURE_SUBAGENTS, supportsFeature, type HostConnection } from '../net/types';
import { applyThreadEventResult, prependThread, threadFromPage, type ThreadState } from '../lib/thread';
import { isChatEvent, type ChatEvent } from './chatReducer';
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
    set((s) => {
      const thread = s.open[chat];
      const views = { ...s.views };
      if (thread) delete views[threadKey(chat, thread)];
      return { open: { ...s.open, [chat]: undefined }, views };
    });
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
  return watchThreadOnConnection(conn, session, chat, thread);
}

/** Kept separate so compatibility behavior can be tested without a live relay provider. */
export function watchThreadOnConnection(conn: HostConnection, session: string, chat: string, thread: string): () => void {
  const key = threadKey(chat, thread);
  let disposed = false;
  let loading = false;
  let early: ChatEvent[] = [];

  const load = async () => {
    if (loading || disposed) return;
    loading = true;
    early = [];
    useSubagents.getState().set(key, (v) => ({ ...v, loading: !v.order, error: undefined }));
    try {
      if (!supportsFeature(conn, FEATURE_SUBAGENTS)) {
        useSubagents.getState().set(key, (v) => ({ ...v, loading: false, error: '主机版本过旧，无法查看子智能体的过程' }));
        return;
      }
      const res = await call(conn, { op: 'chat_thread', session, thread, limit: PAGE }, 'chat_thread');
      if (disposed) return;
      let st = threadFromPage(res.items, res.more, res.seq);
      let reload = false;
      for (const e of early) {
        const result = applyThreadEventResult(st, e, thread);
        st = result.state;
        reload ||= result.gap || result.reload;
      }
      early = [];
      useSubagents.getState().set(key, () => ({ ...st, loading: false }));
      if (reload) void load();
    } catch (err) {
      if (disposed) return;
      useSubagents.getState().set(key, (v) => ({ ...v, loading: false, error: (err as Error).message }));
    } finally {
      loading = false;
    }
  };

  const offEvent = conn.onEvent((e) => {
    if (!isChatEvent(e) || e.session !== session) return;
    if (loading) {
      early.push(e);
      return;
    }
    let reload = false;
    useSubagents.getState().set(key, (v) => {
      if (!v.order) return v;
      const result = applyThreadEventResult(v as ThreadState, e, thread);
      reload = result.gap || result.reload;
      return result.state === v ? v : { ...v, ...result.state };
    });
    if (reload) void load();
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
  if (!conn || !supportsFeature(conn, FEATURE_SUBAGENTS) || !v?.more || v.loadingOlder || before === undefined) return;
  useSubagents.getState().set(key, (x) => ({ ...x, loadingOlder: true }));
  try {
    const res = await call(conn, { op: 'chat_thread', session, thread, before, limit: PAGE }, 'chat_thread');
    useSubagents.getState().set(key, (x) => (x.order?.[0] === before ? { ...x, ...prependThread(x as ThreadState, res.items, res.more), loadingOlder: false } : { ...x, loadingOlder: false }));
  } catch (err) {
    useSubagents.getState().set(key, (x) => ({ ...x, loadingOlder: false }));
    toastError('加载更早的内容失败', err);
  }
}
