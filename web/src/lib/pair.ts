import type { PairPayload } from '../proto/generated/PairPayload';
import { b64urlToText } from './base64';

export class PairParseError extends Error {}

/** Extracts the `pair=` payload from a full link, a hash fragment or the bare payload. */
export function extractPairParam(input: string): string | null {
  const s = input.trim();
  if (!s) return null;
  const m = s.match(/[#&?]pair=([A-Za-z0-9_\-=%]+)/);
  if (m) return decodeURIComponent(m[1]);
  if (s.startsWith('pair=')) return s.slice(5);
  if (/^[A-Za-z0-9_\-]+=*$/.test(s)) return s;
  return null;
}

export function parsePairPayload(input: string): PairPayload {
  const raw = extractPairParam(input);
  if (!raw) throw new PairParseError('不是有效的配对链接');
  let obj: unknown;
  try {
    obj = JSON.parse(b64urlToText(raw.replace(/=+$/, '')));
  } catch {
    throw new PairParseError('配对数据无法解析');
  }
  if (!obj || typeof obj !== 'object') throw new PairParseError('配对数据无法解析');
  const p = obj as Record<string, unknown>;
  const str = (k: string) => typeof p[k] === 'string' && (p[k] as string).length > 0;
  if (p.v !== 1) throw new PairParseError('不支持的配对版本');
  if (!str('relay') || !str('host') || !str('token') || typeof p.host_name !== 'string' || typeof p.exp !== 'number') {
    throw new PairParseError('配对数据缺少字段');
  }
  if (!/^wss?:\/\//.test(p.relay as string)) throw new PairParseError('中继地址无效');
  return {
    v: 1,
    relay: p.relay as string,
    host: p.host as string,
    host_name: p.host_name as string,
    token: p.token as string,
    exp: p.exp as number,
  };
}

export function isPairExpired(p: PairPayload, now = Date.now()): boolean {
  return p.exp <= now;
}

export function pairLink(p: PairPayload, origin: string): string {
  const json = JSON.stringify(p);
  const b64 = btoa(String.fromCharCode(...new TextEncoder().encode(json)))
    .replace(/\+/g, '-')
    .replace(/\//g, '_')
    .replace(/=+$/, '');
  return `${origin}/#pair=${b64}`;
}
