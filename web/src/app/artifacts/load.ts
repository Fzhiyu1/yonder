import { useEffect, useState } from 'react';
import { b64ToBytes } from '../../lib/base64';
import { getConn } from '../../net/provider';
import { call, RequestError } from '../../net/types';
import { downloadFile, mimeFor } from '../files/transfer';

/** Largest file the viewer pulls over the relay in one go. */
export const VIEW_MAX = 60 * 1024 * 1024;

const MIME_EXTRA: Record<string, string> = {
  html: 'text/html',
  htm: 'text/html',
  css: 'text/css',
  js: 'text/javascript',
  mjs: 'text/javascript',
  csv: 'text/csv',
  yaml: 'text/yaml',
  yml: 'text/yaml',
  toml: 'text/plain',
  log: 'text/plain',
  markdown: 'text/markdown',
};

export function typeOf(name: string): string {
  const ext = name.split('.').pop()?.toLowerCase() ?? '';
  return MIME_EXTRA[ext] ?? mimeFor(name);
}

/** Read a whole host file. `onProgress` gets (bytes, total). */
export async function readHostFile(host: string, path: string, onProgress?: (got: number, total: number) => void, signal?: AbortSignal): Promise<Blob> {
  const conn = getConn(host);
  if (!conn) throw new RequestError('offline', '主机未连接');
  const st = await call(conn, { op: 'fs_stat', path }, 'stat');
  if (st.entry.kind !== 'file') throw new RequestError('invalid', '不是文件');
  if (st.entry.size > VIEW_MAX) throw new RequestError('too_large', `文件过大（${Math.round(st.entry.size / 1048576)} MB）`);
  const parts = await downloadFile(conn, path, (g, t) => onProgress?.(g, t), signal);
  return new Blob(parts, { type: typeOf(path) });
}

export interface Fetched {
  status: number;
  type: string;
  body: ArrayBuffer;
  location?: string;
}

/** GET a loopback URL on the host through the encrypted channel. */
export async function hostHttpGet(host: string, url: string): Promise<Fetched> {
  const conn = getConn(host);
  if (!conn) throw new RequestError('offline', '主机未连接');
  const res = await call(conn, { op: 'http_fetch', url }, 'http_response', { timeoutMs: 20_000 });
  const header = (n: string) => res.headers.find(([k]) => k === n)?.[1];
  const bytes = b64ToBytes(res.data);
  return { status: res.status, type: header('content-type') ?? 'application/octet-stream', body: bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength) as ArrayBuffer, location: header('location') };
}

// Blobs of files already opened, so switching tabs does not refetch (LRU by bytes).
const cache = new Map<string, Blob>();
let cached = 0;
const CACHE_MAX = 120 * 1024 * 1024;

function remember(key: string, b: Blob) {
  cache.delete(key);
  cache.set(key, b);
  cached += b.size;
  for (const [k, v] of cache) {
    if (cached <= CACHE_MAX || k === key) break;
    cache.delete(k);
    cached -= v.size;
  }
}

/** A host file, from the cache when it was opened before. */
export async function getHostFile(host: string, path: string): Promise<Blob> {
  const key = `${host}\u0000${path}`;
  const hit = cache.get(key);
  if (hit) return hit;
  const b = await readHostFile(host, path);
  remember(key, b);
  return b;
}

export type LoadState = { status: 'loading'; got: number; total: number } | { status: 'ready'; blob: Blob } | { status: 'error'; message: string };

/** A host file as a Blob, cached; `reload` > 0 bypasses the cache. */
export function useHostFile(host: string, path: string, reload = 0): LoadState {
  const key = `${host}\u0000${path}`;
  const [st, setSt] = useState<LoadState>(() => {
    const b = cache.get(key);
    return b && !reload ? { status: 'ready', blob: b } : { status: 'loading', got: 0, total: 0 };
  });
  useEffect(() => {
    const hit = cache.get(key);
    if (hit && !reload) {
      setSt({ status: 'ready', blob: hit });
      return;
    }
    const ac = new AbortController();
    setSt({ status: 'loading', got: 0, total: 0 });
    readHostFile(host, path, (got, total) => !ac.signal.aborted && setSt({ status: 'loading', got, total }), ac.signal).then(
      (blob) => {
        remember(key, blob);
        if (!ac.signal.aborted) setSt({ status: 'ready', blob });
      },
      (err) => {
        if (!ac.signal.aborted) setSt({ status: 'error', message: err instanceof Error ? err.message : String(err) });
      },
    );
    return () => ac.abort();
  }, [host, key, path, reload]);
  return st;
}

/** Object URL for a Blob, revoked on change/unmount. */
export function useObjectUrl(blob?: Blob): string | undefined {
  const [url, setUrl] = useState<string>();
  useEffect(() => {
    if (!blob) return;
    const u = URL.createObjectURL(blob);
    setUrl(u);
    return () => {
      URL.revokeObjectURL(u);
      setUrl(undefined);
    };
  }, [blob]);
  return url;
}
