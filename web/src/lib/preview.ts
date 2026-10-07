// App side of the preview origin (docs/adr/0003-artifact-viewer.md). Pages in the artifact
// viewer run on a separate origin whose service worker forwards every request, through the
// bridge page, to this window. We answer from the host over the encrypted channel, so the
// server behind the preview origin only ever serves the bridge and the worker script.
import { downloadFile, mimeFor } from '../app/files/transfer';
import type { HostConnection } from '../net/types';
import { call, RequestError } from '../net/types';
import { b64ToBytes } from './base64';

/** Where a preview's content comes from. */
export type PreviewSource =
  /** A web server on the host's loopback interface, e.g. `http://localhost:5173`. */
  | { kind: 'http'; origin: string }
  /** A directory on the host; the page and its relative assets are files under it. */
  | { kind: 'fs'; root: string; sep: '/' | '\\' };

export type PreviewRequest = { kind: 'http'; url: string } | { kind: 'fs'; path: string };

export interface PreviewReply {
  status: number;
  type: string;
  body: ArrayBuffer;
  /** Redirect target (absolute URL) for 3xx replies of `http` sources. */
  location?: string;
}

export type PreviewFetcher = (req: PreviewRequest) => Promise<PreviewReply>;

export type PreviewStatus = { state: 'ready' } | { state: 'loaded' } | { state: 'error'; message: string };

export interface PreviewHandle {
  /** URL of the bridge page to load in an iframe. */
  src: string;
  id: string;
  dispose(): void;
}

const ORIGIN_KEY = 'yonder.previewOrigin';

/** Origin serving the preview bridge, or undefined when previews are disabled. */
export function previewOrigin(): string | undefined {
  let raw: string | null | undefined;
  try {
    raw = localStorage.getItem(ORIGIN_KEY);
  } catch {
    raw = undefined;
  }
  raw = raw?.trim() || (import.meta.env.VITE_PREVIEW_ORIGIN as string | undefined)?.trim();
  if (!raw) return undefined;
  try {
    const u = new URL(raw);
    if (u.protocol !== 'https:' && u.protocol !== 'http:') return undefined;
    // A preview on the app's own origin would share its storage; refuse it.
    if (typeof location !== 'undefined' && u.origin === location.origin) return undefined;
    return u.origin;
  } catch {
    return undefined;
  }
}

/** File types a preview may load from disk. */
const FS_TYPES: Record<string, string> = {
  html: 'text/html',
  htm: 'text/html',
  css: 'text/css; charset=utf-8',
  js: 'text/javascript; charset=utf-8',
  mjs: 'text/javascript; charset=utf-8',
  json: 'application/json; charset=utf-8',
  map: 'application/json; charset=utf-8',
  svg: 'image/svg+xml',
  png: 'image/png',
  jpg: 'image/jpeg',
  jpeg: 'image/jpeg',
  gif: 'image/gif',
  webp: 'image/webp',
  avif: 'image/avif',
  ico: 'image/x-icon',
  woff: 'font/woff',
  woff2: 'font/woff2',
  ttf: 'font/ttf',
  otf: 'font/otf',
  mp4: 'video/mp4',
  webm: 'video/webm',
  mp3: 'audio/mpeg',
  wav: 'audio/wav',
  txt: 'text/plain; charset=utf-8',
  wasm: 'application/wasm',
};

function extOf(name: string): string {
  const base = name.split(/[\\/]/).pop() ?? '';
  const dot = base.lastIndexOf('.');
  return dot > 0 ? base.slice(dot + 1).toLowerCase() : '';
}

/** Content type of a preview file, undefined when the extension is not allowed. */
export function previewTypeOf(name: string): string | undefined {
  return FS_TYPES[extOf(name)];
}

/** HTML without a declared charset is decoded as UTF-8 (what agents write). */
function htmlType(body: ArrayBuffer): string {
  const head = new TextDecoder('latin1').decode(new Uint8Array(body, 0, Math.min(body.byteLength, 2048)));
  return /<meta[^>]+charset/i.test(head) ? 'text/html' : 'text/html; charset=utf-8';
}

/**
 * Host path of a preview URL path under `root`, or undefined when it must not be served:
 * escapes (`..`), hidden segments (`.git`, `.env`), separators or NUL inside a segment, and
 * extensions outside the allow list. A trailing `/` means `index.html`.
 */
export function resolveFsPath(root: string, sep: '/' | '\\', urlPath: string): string | undefined {
  let p = urlPath;
  const cut = p.search(/[?#]/);
  if (cut >= 0) p = p.slice(0, cut);
  if (!p.startsWith('/')) return undefined;
  if (p.endsWith('/')) p += 'index.html';
  const segs: string[] = [];
  for (const raw of p.split('/').slice(1)) {
    let seg: string;
    try {
      seg = decodeURIComponent(raw);
    } catch {
      return undefined;
    }
    if (seg === '') return undefined;
    if (seg.startsWith('.') || /[\\/\0:]/.test(seg)) return undefined;
    segs.push(seg);
  }
  if (segs.length === 0 || !previewTypeOf(segs[segs.length - 1])) return undefined;
  return root.replace(/[\\/]+$/, '') + sep + segs.join(sep);
}

/** Absolute URL of a preview path on an http source, undefined when it leaves the origin. */
export function resolveHttpUrl(origin: string, urlPath: string): string | undefined {
  if (!urlPath.startsWith('/') || urlPath.startsWith('//')) return undefined;
  try {
    const u = new URL(urlPath, origin);
    return u.origin === new URL(origin).origin ? u.href : undefined;
  } catch {
    return undefined;
  }
}

const enc = new TextEncoder();

function textReply(status: number, message: string): PreviewReply {
  return { status, type: 'text/plain; charset=utf-8', body: enc.encode(message).buffer as ArrayBuffer };
}

function errorReply(err: unknown): PreviewReply {
  const code = err instanceof RequestError ? err.code : '';
  const message = err instanceof Error ? err.message : String(err);
  if (code === 'not_found') return textReply(404, 'Not found');
  if (code === 'forbidden') return textReply(403, message);
  if (code === 'too_large') return textReply(413, message);
  return textReply(502, message || 'Preview request failed');
}

/** Bound on concurrent host requests per preview (a dev page can ask for 100 modules). */
const MAX_IN_FLIGHT = 6;

interface Entry {
  source: PreviewSource;
  fetcher: PreviewFetcher;
  onStatus?: (s: PreviewStatus) => void;
  active: number;
  queue: Array<() => void>;
}

const previews = new Map<string, Entry>();
let listening = false;

async function answer(entry: Entry, path: string): Promise<PreviewReply> {
  const { source } = entry;
  if (source.kind === 'http') {
    const url = resolveHttpUrl(source.origin, path);
    if (!url) return textReply(403, 'Outside the previewed server');
    const r = await entry.fetcher({ kind: 'http', url });
    if (r.status >= 300 && r.status < 400 && r.location) {
      // Keep same-server redirects inside the preview; drop the others.
      try {
        const to = new URL(r.location, url);
        if (to.origin === new URL(source.origin).origin) return { ...r, location: to.pathname + to.search };
      } catch {
        // fall through
      }
      return { ...r, location: undefined };
    }
    return r;
  }
  const file = resolveFsPath(source.root, source.sep, path);
  if (!file) return textReply(404, 'Not found');
  const r = await entry.fetcher({ kind: 'fs', path: file });
  if (r.status === 200 && /^text\/html$/.test(r.type)) return { ...r, type: htmlType(r.body) };
  return r;
}

async function serve(entry: Entry, path: string, port: MessagePort): Promise<void> {
  if (entry.active >= MAX_IN_FLIGHT) await new Promise<void>((go) => entry.queue.push(go));
  entry.active++;
  let reply: PreviewReply;
  try {
    reply = await answer(entry, path);
  } catch (err) {
    reply = errorReply(err);
  } finally {
    entry.active--;
    entry.queue.shift()?.();
  }
  let body = reply.body;
  if (!(body instanceof ArrayBuffer)) body = new ArrayBuffer(0);
  port.postMessage({ status: reply.status, type: reply.type, body, location: reply.location }, [body]);
  port.close();
}

function onMessage(e: MessageEvent): void {
  const origin = previewOrigin();
  if (!origin || e.origin !== origin) return;
  const d = e.data as { type?: unknown; id?: unknown; path?: unknown; message?: unknown } | null;
  if (!d || typeof d.id !== 'string') return;
  const entry = previews.get(d.id);
  if (!entry) return;
  switch (d.type) {
    case 'yonder-preview-fetch':
      if (typeof d.path === 'string' && e.ports[0]) void serve(entry, d.path, e.ports[0]);
      break;
    case 'yonder-preview-ready':
      entry.onStatus?.({ state: 'ready' });
      break;
    case 'yonder-preview-loaded':
      entry.onStatus?.({ state: 'loaded' });
      break;
    case 'yonder-preview-error':
      entry.onStatus?.({ state: 'error', message: typeof d.message === 'string' ? d.message : 'preview failed' });
      break;
    default:
      break;
  }
}

function randomId(): string {
  const b = crypto.getRandomValues(new Uint8Array(16));
  return Array.from(b, (x) => x.toString(16).padStart(2, '0')).join('');
}

/**
 * Registers a preview and returns the bridge URL to show in an iframe. Requests from the
 * preview are answered only while the handle is alive, only for this id, and only from
 * paths inside `source`. Throws when previews are disabled (see `previewOrigin`).
 */
export function openPreview(opts: {
  source: PreviewSource;
  /** Initial path including the query, e.g. `/` or `/report/index.html`. */
  path: string;
  fetcher: PreviewFetcher;
  onStatus?: (s: PreviewStatus) => void;
}): PreviewHandle {
  const origin = previewOrigin();
  if (!origin) throw new Error('preview origin not configured');
  if (!listening) {
    window.addEventListener('message', onMessage);
    listening = true;
  }
  const id = randomId();
  previews.set(id, { source: opts.source, fetcher: opts.fetcher, onStatus: opts.onStatus, active: 0, queue: [] });
  const q = new URLSearchParams({
    id,
    parent: location.origin,
    path: opts.path.startsWith('/') ? opts.path : `/${opts.path}`,
    // Files keep an /__p/<id>/ prefix so relative and root-relative assets stay in the
    // preview; dev servers need their real absolute paths.
    mode: opts.source.kind === 'fs' ? 'prefix' : 'root',
  });
  return {
    id,
    src: `${origin}/__preview/bridge.html?${q}`,
    dispose() {
      const e = previews.get(id);
      previews.delete(id);
      e?.queue.splice(0).forEach((go) => go());
    },
  };
}

/** Largest file a preview loads from disk. */
const MAX_FILE = 64 * 1024 * 1024;

/** Fetcher over a host connection: `http_fetch` for dev servers, chunked `fs_read` for files. */
export function makeHostFetcher(conn: HostConnection): PreviewFetcher {
  return async (req) => {
    if (req.kind === 'http') {
      const res = await call(conn, { op: 'http_fetch', url: req.url }, 'http_response', { timeoutMs: 30_000 });
      const header = (name: string) => res.headers.find(([k]) => k === name)?.[1];
      const bytes = b64ToBytes(res.data);
      const path = new URL(req.url).pathname;
      return {
        status: res.status,
        type: header('content-type') ?? previewTypeOf(path) ?? mimeFor(path),
        body: bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength) as ArrayBuffer,
        location: header('location'),
      };
    }
    const abort = new AbortController();
    let tooLarge = false;
    const parts = await downloadFile(
      conn,
      req.path,
      (_got, total) => {
        if (total > MAX_FILE) {
          tooLarge = true;
          abort.abort();
        }
      },
      abort.signal,
    ).catch((err: unknown) => {
      if (tooLarge) throw new RequestError('too_large', 'File too large to preview');
      throw err;
    });
    const size = parts.reduce((n, p) => n + p.length, 0);
    const out = new Uint8Array(size);
    let at = 0;
    for (const p of parts) {
      out.set(p, at);
      at += p.length;
    }
    return { status: 200, type: previewTypeOf(req.path) ?? mimeFor(req.path), body: out.buffer };
  };
}
