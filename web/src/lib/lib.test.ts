import { describe, expect, it } from 'vitest';
import { b64ToBytes, b64urlToBytes, bytesToB64, bytesToB64url, textToB64, utf8Decode } from './base64';
import { decodeFrame, encodeFrame } from './frame';
import { extractPairParam, isPairExpired, parsePairPayload, PairParseError } from './pair';
import { applyPtyOutput } from './termOffset';
import { breadcrumbs, parentPath, joinPath, baseName } from './paths';
import { filterModels, pickerModels } from './models';

describe('base64', () => {
  it('roundtrips standard base64 including large inputs', () => {
    const big = new Uint8Array(100_000).map((_, i) => i % 256);
    expect(b64ToBytes(bytesToB64(big))).toEqual(big);
    expect(bytesToB64(new Uint8Array([0xfb, 0xff]))).toBe('+/8=');
  });
  it('roundtrips base64url without padding', () => {
    const b = new Uint8Array([0xfb, 0xff, 0x01]);
    expect(bytesToB64url(b)).toBe('-_8B');
    expect(b64urlToBytes('-_8B')).toEqual(b);
    expect(b64urlToBytes('-_8')).toEqual(new Uint8Array([0xfb, 0xff]));
  });
  it('handles utf-8 text', () => {
    expect(utf8Decode(b64ToBytes(textToB64('你好 ✓')))).toBe('你好 ✓');
  });
});

describe('relay frames', () => {
  it('encodes link id big-endian', () => {
    const f = encodeFrame(0x01020304, new Uint8Array([9, 8]));
    expect(Array.from(f)).toEqual([1, 2, 3, 4, 9, 8]);
    const d = decodeFrame(f.buffer);
    expect(d.link).toBe(0x01020304);
    expect(Array.from(d.payload)).toEqual([9, 8]);
  });
  it('handles max link id and rejects oversize payloads', () => {
    expect(decodeFrame(encodeFrame(0xffffffff, new Uint8Array())).link).toBe(0xffffffff);
    expect(() => encodeFrame(1, new Uint8Array(65536))).toThrow();
    expect(() => decodeFrame(new Uint8Array(3))).toThrow();
  });
  it('decodes from a subarray view', () => {
    const f = encodeFrame(7, new Uint8Array([1]));
    const padded = new Uint8Array(f.length + 2);
    padded.set(f, 2);
    expect(decodeFrame(padded.subarray(2)).link).toBe(7);
  });
});

describe('pair payload', () => {
  const payload = { v: 1, relay: 'wss://r.example/v1/ws', host: 'abc_-', host_name: 'MacBook', token: 't1', exp: 2_000 };
  const enc = bytesToB64url(new TextEncoder().encode(JSON.stringify(payload)));
  it('parses links, fragments and bare payloads', () => {
    expect(parsePairPayload(`https://x.example/#pair=${enc}`)).toEqual(payload);
    expect(parsePairPayload(`#pair=${enc}`)).toEqual(payload);
    expect(parsePairPayload(enc)).toEqual(payload);
    expect(extractPairParam('hello world')).toBeNull();
  });
  it('rejects malformed payloads', () => {
    expect(() => parsePairPayload('#pair=@@@')).toThrow(PairParseError);
    const bad = bytesToB64url(new TextEncoder().encode(JSON.stringify({ ...payload, v: 2 })));
    expect(() => parsePairPayload(bad)).toThrow(PairParseError);
    const missing = bytesToB64url(new TextEncoder().encode(JSON.stringify({ v: 1, relay: 'wss://x' })));
    expect(() => parsePairPayload(missing)).toThrow(PairParseError);
  });
  it('checks expiry', () => {
    expect(isPairExpired(payload, 1_999)).toBe(false);
    expect(isPairExpired(payload, 2_000)).toBe(true);
  });
});

describe('pty offset bookkeeping', () => {
  const b = (n: number) => new Uint8Array(n).map((_, i) => i);
  it('writes contiguous output', () => {
    const r = applyPtyOutput(10, 10, b(5));
    expect(r).toEqual({ kind: 'write', bytes: b(5), next: 15 });
  });
  it('skips already-seen output', () => {
    expect(applyPtyOutput(10, 2, b(8)).kind).toBe('skip');
    expect(applyPtyOutput(10, 5, b(5)).kind).toBe('skip');
  });
  it('trims overlaps', () => {
    const r = applyPtyOutput(10, 8, b(5));
    expect(r.kind).toBe('write');
    if (r.kind === 'write') {
      expect(Array.from(r.bytes)).toEqual([2, 3, 4]);
      expect(r.next).toBe(13);
    }
  });
  it('detects gaps', () => {
    expect(applyPtyOutput(10, 12, b(3))).toEqual({ kind: 'gap', since: 10 });
  });
});

describe('paths', () => {
  it('builds posix breadcrumbs', () => {
    expect(breadcrumbs('/Users/me/src')).toEqual([
      { name: '/', path: '/' },
      { name: 'Users', path: '/Users' },
      { name: 'me', path: '/Users/me' },
      { name: 'src', path: '/Users/me/src' },
    ]);
    expect(parentPath('/Users')).toBe('/');
    expect(parentPath('/')).toBeNull();
    expect(joinPath('/a', 'b')).toBe('/a/b');
    expect(baseName('/a/b.txt')).toBe('b.txt');
  });
  it('builds windows breadcrumbs', () => {
    expect(breadcrumbs('C:\\Users\\me', '\\')).toEqual([
      { name: 'C:', path: 'C:\\' },
      { name: 'Users', path: 'C:\\Users' },
      { name: 'me', path: 'C:\\Users\\me' },
    ]);
    expect(parentPath('C:\\Users', '\\')).toBe('C:\\');
    expect(joinPath('C:\\', 'x', '\\')).toBe('C:\\x');
  });
});

describe('model picker', () => {
  const models = ['gpt-5.5', 'cmd/gpt-5.6-sol', 'claude-opus-5-5', 'gpt-5.6-sol', 'deepseek/deepseek-v4-pro', 'relay/claude-3-5-haiku'];
  it('returns everything for an empty query', () => {
    expect(filterModels(models, '  ')).toEqual(models);
  });
  it('matches every word, case-insensitively, anywhere in the id', () => {
    expect(filterModels(models, 'SOL')).toEqual(['cmd/gpt-5.6-sol', 'gpt-5.6-sol']);
    expect(filterModels(models, 'gpt sol')).toEqual(['cmd/gpt-5.6-sol', 'gpt-5.6-sol']);
    expect(filterModels(models, 'opus 4')).toEqual([]);
  });
  it('puts prefix matches first, also after a provider/ prefix', () => {
    expect(filterModels(models, 'claude')).toEqual(['claude-opus-5-5', 'relay/claude-3-5-haiku']);
    expect(filterModels(models, '5.6')).toEqual(['cmd/gpt-5.6-sol', 'gpt-5.6-sol']);
    expect(filterModels(models, 'deep')).toEqual(['deepseek/deepseek-v4-pro']);
    expect(filterModels(['xx-haiku', 'haiku-x'], 'haiku')).toEqual(['haiku-x', 'xx-haiku']);
  });
  it('orders the default first and keeps a custom value visible', () => {
    expect(pickerModels(['a', 'b', 'c'], 'b')).toEqual(['b', 'a', 'c']);
    expect(pickerModels(['a', 'b'], 'b', 'mine')).toEqual(['b', 'mine', 'a']);
    expect(pickerModels(['a', 'b'], undefined, 'a')).toEqual(['a', 'b']);
    expect(pickerModels(['a'], 'x', '')).toEqual(['x', 'a']);
  });
});
