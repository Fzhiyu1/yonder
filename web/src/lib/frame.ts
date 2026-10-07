// Relay binary frames: [u32 big-endian link id][payload], payload <= 65535 bytes.

export const MAX_FRAME_PAYLOAD = 65535;

export function encodeFrame(link: number, payload: Uint8Array): Uint8Array<ArrayBuffer> {
  if (!Number.isInteger(link) || link < 0 || link > 0xffffffff) throw new Error('invalid link id');
  if (payload.length > MAX_FRAME_PAYLOAD) throw new Error('frame payload too large');
  const out = new Uint8Array(4 + payload.length);
  new DataView(out.buffer).setUint32(0, link, false);
  out.set(payload, 4);
  return out;
}

export function decodeFrame(buf: ArrayBufferLike | Uint8Array): { link: number; payload: Uint8Array } {
  const bytes = buf instanceof Uint8Array ? buf : new Uint8Array(buf);
  if (bytes.length < 4) throw new Error('short frame');
  const link = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength).getUint32(0, false);
  return { link, payload: bytes.subarray(4) };
}
