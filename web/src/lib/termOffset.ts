// Bookkeeping for live pty_output against the byte offset the client already has.

export type OffsetResult =
  | { kind: 'skip' }
  | { kind: 'write'; bytes: Uint8Array; next: number }
  | { kind: 'gap'; since: number };

/**
 * `known` is the number of output bytes already written; `offset` is the global offset of
 * the first byte of `bytes`.
 */
export function applyPtyOutput(known: number, offset: number, bytes: Uint8Array): OffsetResult {
  if (offset > known) return { kind: 'gap', since: known };
  const end = offset + bytes.length;
  if (end <= known) return { kind: 'skip' };
  const trimmed = offset < known ? bytes.subarray(known - offset) : bytes;
  return { kind: 'write', bytes: trimmed, next: end };
}
