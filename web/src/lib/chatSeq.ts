export type SeqStep = 'ignore' | 'apply' | 'gap';

/** Apply the per-session sequence rule shared by chat and sub-agent views. */
export function seqStep(current: number, incoming: number): SeqStep {
  if (incoming <= current) return 'ignore';
  if (incoming > current + 1) return 'gap';
  return 'apply';
}
