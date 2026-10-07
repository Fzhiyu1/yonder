// Standard base64 is used for binary payloads (terminal bytes, file chunks);
// base64url (no padding) for keys and the pairing payload.

const CHUNK = 0x8000;

export function bytesToB64(bytes: Uint8Array): string {
  let bin = '';
  for (let i = 0; i < bytes.length; i += CHUNK) {
    bin += String.fromCharCode.apply(null, Array.from(bytes.subarray(i, i + CHUNK)));
  }
  return btoa(bin);
}

export function b64ToBytes(b64: string): Uint8Array {
  const clean = b64.replace(/\s+/g, '');
  const bin = atob(clean);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

export function bytesToB64url(bytes: Uint8Array): string {
  return bytesToB64(bytes).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
}

export function b64urlToBytes(s: string): Uint8Array {
  let b64 = s.trim().replace(/-/g, '+').replace(/_/g, '/');
  const pad = b64.length % 4;
  if (pad === 1) throw new Error('invalid base64url length');
  if (pad) b64 += '='.repeat(4 - pad);
  return b64ToBytes(b64);
}

const enc = new TextEncoder();
const dec = new TextDecoder();

export function utf8Encode(s: string): Uint8Array {
  return enc.encode(s);
}

export function utf8Decode(b: Uint8Array): string {
  return dec.decode(b);
}

export function textToB64(s: string): string {
  return bytesToB64(utf8Encode(s));
}

export function b64urlToText(s: string): string {
  return utf8Decode(b64urlToBytes(s));
}
