import { useEffect, useState } from 'react';
import type { Artifact } from '../../lib/artifacts';
import { getConn } from '../../net/provider';
import { call } from '../../net/types';

/** Where a host-local page can be opened in the device's own browser. */
export type TailnetState =
  | { status: 'loading' }
  | { status: 'ready'; url: string; reachable: boolean }
  | { status: 'none'; reason: string };

const cache = new Map<string, Extract<TailnetState, { status: 'ready' }>>();

/**
 * The Tailscale address of a dev-server page, looked up as soon as the page is shown: iOS only
 * lets a tap open a new tab synchronously, so the address must be known before the tap.
 */
export function useTailnetUrl(host: string, a: Artifact | undefined, reload = 0): TailnetState | undefined {
  const ref = a?.kind === 'web' ? a.ref : undefined;
  const key = ref ? `${host}\u0000${ref}` : '';
  const [st, setSt] = useState<TailnetState>();
  useEffect(() => {
    if (!ref) {
      setSt(undefined);
      return;
    }
    const hit = reload ? undefined : cache.get(key);
    if (hit) {
      setSt(hit);
      return;
    }
    let live = true;
    setSt({ status: 'loading' });
    const conn = getConn(host);
    if (!conn) {
      setSt({ status: 'none', reason: '主机未连接' });
      return;
    }
    call(conn, { op: 'tailnet_url', url: ref }, 'tailnet_url', { timeoutMs: 8_000 }).then(
      (r) => {
        const v = { status: 'ready' as const, url: r.url, reachable: r.reachable };
        cache.set(key, v);
        if (live) setSt(v);
      },
      (err: unknown) => {
        const msg = err instanceof Error ? err.message : String(err);
        if (live) setSt({ status: 'none', reason: /tailscale/i.test(msg) ? '这台主机没有 Tailscale 地址' : msg });
      },
    );
    return () => {
      live = false;
    };
  }, [host, ref, key, reload]);
  return st;
}

/**
 * Open `url` in the system browser (a new tab; Safari from a home-screen app). Must run inside
 * the tap handler. A link click is used because `window.open` with `noopener` returns null
 * whether or not it worked.
 */
export function openExternal(url: string): void {
  const a = document.createElement('a');
  a.href = url;
  a.target = '_blank';
  a.rel = 'noopener noreferrer';
  document.body.appendChild(a);
  a.click();
  a.remove();
}
