import { b64ToBytes, bytesToB64 } from '../../lib/base64';
import type { HostConnection } from '../../net/types';
import { call, RequestError } from '../../net/types';
import { useUi } from '../../store/ui';

export const UPLOAD_CHUNK = 256 * 1024;
export const DOWNLOAD_CHUNK = 1024 * 1024;

/**
 * Chunked upload via fs_write. Throws RequestError('exists') when the target exists and
 * `overwrite` is false; the caller asks the user and restarts with overwrite.
 */
export async function uploadFile(
  conn: HostConnection,
  path: string,
  file: Blob,
  overwrite: boolean,
  onProgress: (sent: number) => void,
  signal?: AbortSignal,
): Promise<void> {
  let offset = 0;
  const size = file.size;
  do {
    if (signal?.aborted) throw new RequestError('aborted', '已取消');
    const end = Math.min(offset + UPLOAD_CHUNK, size);
    const chunk = new Uint8Array(await file.slice(offset, end).arrayBuffer());
    const finish = end >= size;
    await conn.request({ op: 'fs_write', path, offset, data: bytesToB64(chunk), finish, overwrite });
    offset = end;
    onProgress(offset);
  } while (offset < size);
}

export async function downloadFile(
  conn: HostConnection,
  path: string,
  onProgress: (got: number, total: number) => void,
  signal?: AbortSignal,
): Promise<Uint8Array<ArrayBuffer>[]> {
  const parts: Uint8Array<ArrayBuffer>[] = [];
  let offset = 0;
  for (;;) {
    if (signal?.aborted) throw new RequestError('aborted', '已取消');
    const res = await call(conn, { op: 'fs_read', path, offset, len: DOWNLOAD_CHUNK }, 'file_chunk');
    const bytes = b64ToBytes(res.data) as Uint8Array<ArrayBuffer>;
    parts.push(bytes);
    offset += bytes.length;
    onProgress(offset, res.size);
    if (res.eof || bytes.length === 0) break;
  }
  return parts;
}

const MIME: Record<string, string> = {
  png: 'image/png',
  jpg: 'image/jpeg',
  jpeg: 'image/jpeg',
  gif: 'image/gif',
  webp: 'image/webp',
  svg: 'image/svg+xml',
  bmp: 'image/bmp',
  ico: 'image/x-icon',
  avif: 'image/avif',
  pdf: 'application/pdf',
  txt: 'text/plain',
  md: 'text/markdown',
  json: 'application/json',
};

export function mimeFor(name: string): string {
  const ext = name.split('.').pop()?.toLowerCase() ?? '';
  return MIME[ext] ?? 'application/octet-stream';
}

export function isImage(name: string): boolean {
  return mimeFor(name).startsWith('image/');
}

const TEXT_EXT = new Set(
  'txt md markdown json jsonc yaml yml toml ini cfg conf log csv tsv xml html htm css scss less js mjs cjs ts tsx jsx rs go py rb java kt swift c h cc cpp hpp cs sh bash zsh fish ps1 bat sql lua php pl r dart vue svelte env gitignore dockerfile makefile lock'.split(' '),
);

export function isTextName(name: string): boolean {
  const lower = name.toLowerCase();
  if (TEXT_EXT.has(lower)) return true;
  if (lower.startsWith('.') && !lower.slice(1).includes('.')) return true;
  const ext = lower.includes('.') ? lower.split('.').pop()! : '';
  return TEXT_EXT.has(ext);
}

export function looksBinary(bytes: Uint8Array): boolean {
  const n = Math.min(bytes.length, 4096);
  for (let i = 0; i < n; i++) if (bytes[i] === 0) return true;
  return false;
}

const askToSave = (name: string) => useUi.getState().ask({ title: '下载完成', message: `「${name}」已就绪。`, confirmLabel: '保存' });

/** Save a blob to the user's device (share sheet on iOS when available). */
export async function saveBlob(blob: Blob, name: string): Promise<void> {
  const ios = /iPhone|iPad|iPod/.test(navigator.userAgent) || (/Macintosh/.test(navigator.userAgent) && navigator.maxTouchPoints > 1);
  if (ios && typeof navigator.canShare === 'function') {
    const file = new File([blob], name, { type: blob.type });
    if (navigator.canShare({ files: [file] })) {
      // share() needs a tap from the last few seconds (5 s in WebKit). A download slower than
      // that has outlived the tap that started it, so ask for a fresh one first.
      let fresh = navigator.userActivation?.isActive ?? true;
      for (let attempt = 0; attempt < 2; attempt++) {
        if (!fresh && !(await askToSave(name))) return;
        try {
          await navigator.share({ files: [file] });
          return;
        } catch (err) {
          const kind = (err as Error).name;
          if (kind === 'AbortError') return;
          if (kind !== 'NotAllowedError' || !fresh) break;
          fresh = false;
        }
      }
    }
  }
  const url = URL.createObjectURL(blob);
  const a = document.createElement('a');
  a.href = url;
  a.download = name;
  document.body.appendChild(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 30_000);
}
