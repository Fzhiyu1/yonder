import type { Approval } from '../proto/generated/Approval';
import type { ChatItem } from '../proto/generated/ChatItem';
import type { ChatSnapshot } from '../proto/generated/ChatSnapshot';
import type { ChatStatus } from '../proto/generated/ChatStatus';
import type { Event } from '../proto/generated/Event';
import { seqStep } from '../lib/chatSeq';

export interface ChatState {
  /** Item ids in display order. */
  order: string[];
  byId: Record<string, ChatItem>;
  approvals: Approval[];
  status: ChatStatus;
  statusDetail?: string;
  /** Seq of the last applied event; -1 before the first attach. */
  seq: number;
  truncated: boolean;
  attached: boolean;
}

export type ChatEvent = Extract<
  Event,
  { ev: 'chat_item' | 'chat_delta' | 'chat_snapshot' | 'chat_status' | 'approval_requested' | 'approval_resolved' }
>;

export interface ApplyResult {
  state: ChatState;
  /** A seq gap was detected; the caller must re-attach. State is unchanged. */
  gap: boolean;
}

export function emptyChat(): ChatState {
  return { order: [], byId: {}, approvals: [], status: 'starting', seq: -1, truncated: false, attached: false };
}

export function chatFromSnapshot(s: ChatSnapshot): ChatState {
  const byId: Record<string, ChatItem> = {};
  const order: string[] = [];
  for (const it of s.items) {
    if (it.thread) continue;
    if (!byId[it.id]) order.push(it.id);
    byId[it.id] = it;
  }
  return {
    order,
    byId,
    approvals: [...s.approvals],
    status: s.status,
    seq: s.seq,
    truncated: s.truncated,
    attached: true,
  };
}

/** Items fetched by scrolling up go in front; ids already shown are skipped. */
export function prependOlder(state: ChatState, items: ChatItem[], more: boolean): ChatState {
  const fresh = items.filter((it) => !(it.id in state.byId));
  const byId = { ...state.byId };
  for (const it of fresh) byId[it.id] = it;
  return { ...state, order: [...fresh.map((it) => it.id), ...state.order], byId, truncated: more };
}

/**
 * A new snapshot (re-attach) carries only the newest page: keep the older items the client
 * already loaded when they line up with it.
 */
export function keepOlder(prev: ChatState, next: ChatState): ChatState {
  const first = next.order[0];
  const at = first === undefined ? -1 : prev.order.indexOf(first);
  if (at <= 0) return next;
  const older = prev.order.slice(0, at).filter((id) => !(id in next.byId));
  const byId = { ...next.byId };
  for (const id of older) byId[id] = prev.byId[id];
  return { ...next, order: [...older, ...next.order], byId, truncated: prev.truncated };
}

export function isChatEvent(e: Event): e is ChatEvent {
  return (
    e.ev === 'chat_item' ||
    e.ev === 'chat_delta' ||
    e.ev === 'chat_snapshot' ||
    e.ev === 'chat_status' ||
    e.ev === 'approval_requested' ||
    e.ev === 'approval_resolved'
  );
}

function upsert(state: ChatState, item: ChatItem): Pick<ChatState, 'order' | 'byId'> {
  const exists = item.id in state.byId;
  return {
    order: exists ? state.order : [...state.order, item.id],
    byId: { ...state.byId, [item.id]: item },
  };
}

/**
 * Applies one chat event following the protocol's seq rules:
 * `seq <= current` is ignored, `seq > current + 1` is a gap (re-attach), snapshots replace.
 * Items of a sub-agent's thread (`thread` set) only advance the seq: they belong to the
 * sub-agent's read-only view (store/subagents.ts), not to the chat.
 */
export function applyChatEvent(state: ChatState, event: ChatEvent): ApplyResult {
  if (event.ev === 'chat_snapshot') return { state: keepOlder(state, chatFromSnapshot(event.snapshot)), gap: false };
  if (!state.attached) return { state, gap: false };
  const step = seqStep(state.seq, event.seq);
  if (step === 'ignore') return { state, gap: false };
  if (step === 'gap') return { state, gap: true };
  const seq = event.seq;
  if ((event.ev === 'chat_item' && event.item.thread) || (event.ev === 'chat_delta' && event.thread)) return { state: { ...state, seq }, gap: false };
  switch (event.ev) {
    case 'chat_item':
      return { state: { ...state, ...upsert(state, event.item), seq }, gap: false };
    case 'chat_delta': {
      const prev = state.byId[event.item];
      const base: ChatItem = prev ?? {
        id: event.item,
        kind: 'agent',
        status: 'in_progress',
        paths: [],
        ts: Date.now(),
      };
      const next: ChatItem =
        event.field === 'text'
          ? { ...base, text: (base.text ?? '') + event.delta }
          : { ...base, output: (base.output ?? '') + event.delta };
      return { state: { ...state, ...upsert(state, next), seq }, gap: false };
    }
    case 'chat_status':
      return { state: { ...state, status: event.status, statusDetail: event.detail, seq }, gap: false };
    case 'approval_requested': {
      const others = state.approvals.filter((a) => a.id !== event.approval.id);
      return { state: { ...state, approvals: [...others, event.approval], seq }, gap: false };
    }
    case 'approval_resolved':
      return {
        state: { ...state, approvals: state.approvals.filter((a) => a.id !== event.approval), seq },
        gap: false,
      };
  }
}
