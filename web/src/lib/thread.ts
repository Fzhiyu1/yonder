import type { ChatItem } from '../proto/generated/ChatItem';
import type { Event } from '../proto/generated/Event';
import { seqStep } from './chatSeq';

/** The read-only view of one sub-agent's thread. */
export interface ThreadState {
  order: string[];
  byId: Record<string, ChatItem>;
  /** Seq of the chat event last reflected; live events at or below it are already in. */
  seq: number;
  /** Session seq, including parent-chat and other-thread events. */
  sessionSeq: number;
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
  return { order, byId, seq, sessionSeq: seq, more };
}

/** Older items go in front; ids already shown are skipped. */
export function prependThread(state: ThreadState, items: ChatItem[], more: boolean): ThreadState {
  const fresh = items.filter((it) => !(it.id in state.byId));
  const byId = { ...state.byId };
  for (const it of fresh) byId[it.id] = it;
  return { ...state, order: [...fresh.map((it) => it.id), ...state.order], byId, more };
}

export interface ThreadEventResult {
  state: ThreadState;
  gap: boolean;
  reload: boolean;
}

/**
 * Applies a live chat event of this session to the view of sub-agent `thread`: its `chat_item`
 * / `chat_delta` events (those carrying `thread`) newer than the page. Anything else comes back
 * unchanged.
 */
export function applyThreadEvent(state: ThreadState, e: Event, thread: string): ThreadState {
  return applyThreadEventResult(state, e, thread).state;
}

/**
 * Applies one event while tracking the whole chat session sequence. Events outside the open
 * thread still advance `sessionSeq`, so a later child event does not look like a gap.
 */
export function applyThreadEventResult(state: ThreadState, e: Event, thread: string): ThreadEventResult {
  if (e.ev === 'chat_snapshot') {
    if (e.snapshot.seq < state.sessionSeq) return { state, gap: false, reload: false };
    return { state: { ...state, sessionSeq: e.snapshot.seq }, gap: false, reload: true };
  }
  if (
    e.ev !== 'chat_item' &&
    e.ev !== 'chat_delta' &&
    e.ev !== 'chat_status' &&
    e.ev !== 'approval_requested' &&
    e.ev !== 'approval_resolved'
  ) {
    return { state, gap: false, reload: false };
  }
  const step = seqStep(state.sessionSeq, e.seq);
  if (step === 'ignore') return { state, gap: false, reload: false };
  if (step === 'gap') return { state, gap: true, reload: true };
  const advanced = { ...state, seq: e.seq, sessionSeq: e.seq };
  if (e.ev === 'chat_item') {
    if (e.item.thread !== thread) return { state: advanced, gap: false, reload: false };
    const exists = e.item.id in state.byId;
    return {
      state: { ...advanced, order: exists ? state.order : [...state.order, e.item.id], byId: { ...state.byId, [e.item.id]: e.item } },
      gap: false,
      reload: false,
    };
  }
  if (e.ev === 'chat_delta') {
    if (e.thread !== thread) return { state: advanced, gap: false, reload: false };
    const prev = state.byId[e.item];
    const base: ChatItem = prev ?? { id: e.item, kind: e.field === 'output' ? 'command' : 'agent', status: 'in_progress', paths: [], ts: Date.now(), thread };
    const next: ChatItem = e.field === 'text' ? { ...base, text: (base.text ?? '') + e.delta } : { ...base, output: (base.output ?? '') + e.delta };
    return {
      state: { ...advanced, order: prev ? state.order : [...state.order, e.item], byId: { ...state.byId, [e.item]: next } },
      gap: false,
      reload: false,
    };
  }
  return { state: advanced, gap: false, reload: false };
}
