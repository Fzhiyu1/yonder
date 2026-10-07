import { describe, expect, it } from 'vitest';
import type { ChatItem } from '../proto/generated/ChatItem';
import type { Event } from '../proto/generated/Event';
import type { Response } from '../proto/generated/Response';
import type { HostConnection } from '../net/types';
import { threadKey, useSubagents, watchThreadOnConnection } from './subagents';

const item = (id: string): ChatItem => ({ id, kind: 'agent', status: 'completed', paths: [], ts: 1, thread: 'kid' });

describe('sub-agent store compatibility and lifecycle', () => {
  it('does not call chat_thread on a host without the capability', async () => {
    let calls = 0;
    const conn = {
      status: 'online',
      hostHello: { protocol: 1, ok: true, host_name: 'old', os: 'linux', version: '0.1.0', permissions: [] },
      request: async () => {
        calls++;
        throw new Error('chat_thread must not be called');
      },
      onEvent: () => () => undefined,
      onReconnected: () => () => undefined,
    } as unknown as HostConnection;
    useSubagents.setState({ views: {}, open: {} });
    const stop = watchThreadOnConnection(conn, 's', 'h/s', 'kid');
    await Promise.resolve();
    await Promise.resolve();
    expect(calls).toBe(0);
    expect(useSubagents.getState().views[threadKey('h/s', 'kid')]?.error).toBe('主机版本过旧，无法查看子智能体的过程');
    stop();
  });

  it('evicts the closed thread view', () => {
    const key = threadKey('h/s', 'kid');
    useSubagents.setState({
      open: { 'h/s': 'kid' },
      views: { [key]: { loading: false, order: ['a'], byId: { a: item('a') }, seq: 1, sessionSeq: 1, more: false } },
    });
    useSubagents.getState().hide('h/s');
    expect(useSubagents.getState().views[key]).toBeUndefined();
  });

  it('does not lose a reload requested while the initial page is loading', async () => {
    let resolveFirst!: (response: Response) => void;
    let calls = 0;
    let onEvent: ((event: Event) => void) | undefined;
    const conn = {
      status: 'online',
      hostHello: { protocol: 1, ok: true, host_name: 'new', os: 'linux', version: '0.1.0', permissions: ['sessions'], features: ['subagents'] },
      request: async () => {
        calls++;
        if (calls === 1) {
          return new Promise<Response>((resolve) => {
            resolveFirst = resolve;
          });
        }
        return {
          kind: 'chat_thread',
          items: [item('latest')],
          more: false,
          seq: 12,
        };
      },
      onEvent: (fn: (event: Event) => void) => {
        onEvent = fn;
        return () => undefined;
      },
      onReconnected: () => () => undefined,
    } as unknown as HostConnection;
    useSubagents.setState({ views: {}, open: {} });
    const stop = watchThreadOnConnection(conn, 's', 'h/s', 'kid');
    await Promise.resolve();
    onEvent?.({ ev: 'chat_item', session: 's', seq: 12, item: item('live') });
    resolveFirst({ kind: 'chat_thread', items: [item('initial')], more: false, seq: 10 });
    await new Promise((resolve) => setTimeout(resolve, 0));
    await new Promise((resolve) => setTimeout(resolve, 0));

    expect(calls).toBe(2);
    expect(useSubagents.getState().views[threadKey('h/s', 'kid')]?.order).toEqual(['latest']);
    stop();
  });
});
