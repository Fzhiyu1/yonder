import { describe, expect, it } from 'vitest';
import type { ChatItem } from '../proto/generated/ChatItem';
import { applyChatEvent, chatFromSnapshot, emptyChat, keepOlder, prependOlder } from './chatReducer';

const item = (id: string, extra: Partial<ChatItem> = {}): ChatItem => ({
  id,
  kind: 'agent',
  status: 'completed',
  paths: [],
  ts: 1,
  ...extra,
});

const base = () =>
  chatFromSnapshot({ items: [item('a', { text: 'hi' })], approvals: [], status: 'idle', seq: 5, truncated: false });

describe('applyChatEvent', () => {
  it('replaces everything on snapshot', () => {
    const s = applyChatEvent(base(), {
      ev: 'chat_snapshot',
      session: 's',
      snapshot: { items: [item('b')], approvals: [], status: 'working', seq: 42, truncated: true },
    }).state;
    expect(s.order).toEqual(['b']);
    expect(s.seq).toBe(42);
    expect(s.status).toBe('working');
    expect(s.truncated).toBe(true);
  });

  it('ignores events with seq <= current', () => {
    const s0 = base();
    const r = applyChatEvent(s0, { ev: 'chat_item', session: 's', seq: 5, item: item('x') });
    expect(r.state).toBe(s0);
    expect(r.gap).toBe(false);
    const r2 = applyChatEvent(s0, { ev: 'chat_status', session: 's', seq: 3, status: 'working' });
    expect(r2.state.status).toBe('idle');
  });

  it('reports gaps without changing state', () => {
    const s0 = base();
    const r = applyChatEvent(s0, { ev: 'chat_item', session: 's', seq: 7, item: item('x') });
    expect(r.gap).toBe(true);
    expect(r.state).toBe(s0);
  });

  it('appends new items and replaces existing ones in place', () => {
    let s = applyChatEvent(base(), { ev: 'chat_item', session: 's', seq: 6, item: item('b', { text: 'x' }) }).state;
    s = applyChatEvent(s, { ev: 'chat_item', session: 's', seq: 7, item: item('a', { text: 'changed' }) }).state;
    expect(s.order).toEqual(['a', 'b']);
    expect(s.byId.a.text).toBe('changed');
    expect(s.seq).toBe(7);
  });

  it('applies text and output deltas', () => {
    let s = applyChatEvent(base(), { ev: 'chat_delta', session: 's', seq: 6, item: 'a', field: 'text', delta: ' there' }).state;
    expect(s.byId.a.text).toBe('hi there');
    s = applyChatEvent(s, { ev: 'chat_delta', session: 's', seq: 7, item: 'a', field: 'output', delta: 'out' }).state;
    expect(s.byId.a.output).toBe('out');
  });

  it('creates an in-progress agent item for unknown delta targets', () => {
    const s = applyChatEvent(base(), { ev: 'chat_delta', session: 's', seq: 6, item: 'new', field: 'text', delta: 'yo' }).state;
    expect(s.order).toEqual(['a', 'new']);
    expect(s.byId.new.kind).toBe('agent');
    expect(s.byId.new.status).toBe('in_progress');
    expect(s.byId.new.text).toBe('yo');
  });

  it('tracks approvals and status', () => {
    const approval = { id: 'p1', kind: 'command' as const, title: 't', options: [], ts: 1 };
    let s = applyChatEvent(base(), { ev: 'approval_requested', session: 's', seq: 6, approval }).state;
    s = applyChatEvent(s, { ev: 'chat_status', session: 's', seq: 7, status: 'awaiting_approval', detail: 'd' }).state;
    expect(s.approvals.map((a) => a.id)).toEqual(['p1']);
    expect(s.status).toBe('awaiting_approval');
    expect(s.statusDetail).toBe('d');
    s = applyChatEvent(s, { ev: 'approval_resolved', session: 's', seq: 8, approval: 'p1', option: 'allow' }).state;
    expect(s.approvals).toEqual([]);
  });

  it('drops incremental events before the first attach', () => {
    const s0 = emptyChat();
    const r = applyChatEvent(s0, { ev: 'chat_item', session: 's', seq: 0, item: item('x') });
    expect(r.state).toBe(s0);
    expect(r.gap).toBe(false);
  });
});

describe('paging', () => {
  const snap = (ids: string[], truncated: boolean) =>
    chatFromSnapshot({ items: ids.map((id) => item(id)), approvals: [], status: 'idle', seq: 9, truncated });

  it('prepends older items once and keeps the more flag', () => {
    const s = prependOlder(snap(['c', 'd'], true), [item('a'), item('b'), item('c')], false);
    expect(s.order).toEqual(['a', 'b', 'c', 'd']);
    expect(s.truncated).toBe(false);
  });

  it('keeps loaded older items across a newest-page snapshot', () => {
    const loaded = prependOlder(snap(['c', 'd'], true), [item('a'), item('b')], true);
    const next = keepOlder(loaded, snap(['c', 'd', 'e'], true));
    expect(next.order).toEqual(['a', 'b', 'c', 'd', 'e']);
    expect(next.truncated).toBe(true);
    // A snapshot that does not line up replaces the view.
    expect(keepOlder(loaded, snap(['x'], true)).order).toEqual(['x']);
  });
});
