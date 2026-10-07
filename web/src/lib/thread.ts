import type { ChatItem } from '../proto/generated/ChatItem';
import type { Event } from '../proto/generated/Event';

/** The read-only view of one sub-agent's thread. */
export interface ThreadState {
  order: string[];
  byId: Record<string, ChatItem>;
  /** Seq of the chat event last reflected; live events at or below it are already in. */
  seq: number;
  /** Older items can be fetched. */
  more: boolean;
}

export function threadFromPage(items: ChatItem[], more: boolean, seq: number): ThreadState {
  const byId: Record<string, ChatItem> = {};
  const order: string[] = [];
  for (const it of items) {
    if (!byId[it.id]) order.push(it.id);
    byId[it.id] = it;
  }
  return { order, byId, seq, more };
}

/** Older items go in front; ids already shown are skipped. */
export function prependThread(state: ThreadState, items: ChatItem[], more: boolean): ThreadState {
  const fresh = items.filter((it) => !(it.id in state.byId));
  const byId = { ...state.byId };
  for (const it of fresh) byId[it.id] = it;
  return { ...state, order: [...fresh.map((it) => it.id), ...state.order], byId, more };
}

/**
 * Applies a live chat event of this session to the view of sub-agent `thread`: its `chat_item`
 * / `chat_delta` events (those carrying `thread`) newer than the page. Anything else comes back
 * unchanged.
 */
export function applyThreadEvent(state: ThreadState, e: Event, thread: string): ThreadState {
  if (e.ev === 'chat_item') {
    if (e.item.thread !== thread || e.seq <= state.seq) return state;
    const exists = e.item.id in state.byId;
    return { ...state, seq: e.seq, order: exists ? state.order : [...state.order, e.item.id], byId: { ...state.byId, [e.item.id]: e.item } };
  }
  if (e.ev === 'chat_delta') {
    if (e.thread !== thread || e.seq <= state.seq) return state;
    const prev = state.byId[e.item];
    const base: ChatItem = prev ?? { id: e.item, kind: e.field === 'output' ? 'command' : 'agent', status: 'in_progress', paths: [], ts: Date.now(), thread };
    const next: ChatItem = e.field === 'text' ? { ...base, text: (base.text ?? '') + e.delta } : { ...base, output: (base.output ?? '') + e.delta };
    return { ...state, seq: e.seq, order: prev ? state.order : [...state.order, e.item], byId: { ...state.byId, [e.item]: next } };
  }
  return state;
}
