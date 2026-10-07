import { describe, expect, it } from 'vitest';
import type { ChatItem } from '../proto/generated/ChatItem';
import { applyThreadEvent, prependThread, threadFromPage } from './thread';

const item = (id: string, extra: Partial<ChatItem> = {}): ChatItem => ({ id, kind: 'agent', status: 'completed', paths: [], ts: 1, thread: 'kid', ...extra });

describe('sub-agent thread view', () => {
  it('applies only newer events of its own thread', () => {
    const s0 = threadFromPage([item('a', { text: 'hi' })], false, 10);
    expect(applyThreadEvent(s0, { ev: 'chat_item', session: 's', seq: 9, item: item('b') }, 'kid')).toBe(s0);
    expect(applyThreadEvent(s0, { ev: 'chat_item', session: 's', seq: 11, item: item('b', { thread: 'other' }) }, 'kid')).toBe(s0);
    expect(applyThreadEvent(s0, { ev: 'chat_item', session: 's', seq: 11, item: item('b', { thread: undefined }) }, 'kid')).toBe(s0);
    const s1 = applyThreadEvent(s0, { ev: 'chat_item', session: 's', seq: 11, item: item('b') }, 'kid');
    expect(s1.order).toEqual(['a', 'b']);
    expect(s1.seq).toBe(11);
    const s2 = applyThreadEvent(s1, { ev: 'chat_delta', session: 's', seq: 12, item: 'c', field: 'output', delta: 'out', thread: 'kid' }, 'kid');
    const s3 = applyThreadEvent(s2, { ev: 'chat_delta', session: 's', seq: 13, item: 'c', field: 'output', delta: 'put', thread: 'kid' }, 'kid');
    expect(s3.byId.c.output).toBe('output');
    expect(s3.byId.c.kind).toBe('command');
    expect(applyThreadEvent(s3, { ev: 'chat_delta', session: 's', seq: 14, item: 'c', field: 'text', delta: 'x' }, 'kid')).toBe(s3);
    const s4 = applyThreadEvent(s3, { ev: 'chat_item', session: 's', seq: 15, item: item('a', { text: 'hello' }) }, 'kid');
    expect(s4.order).toEqual(['a', 'b', 'c']);
    expect(s4.byId.a.text).toBe('hello');
  });

  it('prepends older pages without duplicates', () => {
    const s0 = threadFromPage([item('b'), item('c')], true, 3);
    const s1 = prependThread(s0, [item('a'), item('b')], false);
    expect(s1.order).toEqual(['a', 'b', 'c']);
    expect(s1.more).toBe(false);
  });
});
