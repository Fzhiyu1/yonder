import { memo } from 'react';
import { cx } from '../ui/primitives';

/** Unified diff with +/- line coloring. */
export const DiffView = memo(function DiffView({ diff, className }: { diff: string; className?: string }) {
  const lines = diff.replace(/\n$/, '').split('\n');
  return (
    <div className={cx('scroll-thin overflow-auto rounded-md border border-line bg-code font-mono text-[12px] leading-[1.55]', className)}>
      <div className="inline-block min-w-full py-1">
        {lines.map((l, i) => {
          const meta = l.startsWith('diff ') || l.startsWith('index ') || l.startsWith('--- ') || l.startsWith('+++ ');
          const hunk = l.startsWith('@@');
          const add = !meta && l.startsWith('+');
          const del = !meta && l.startsWith('-');
          return (
            <div
              key={i}
              className={cx(
                'px-3 whitespace-pre',
                add && 'bg-[color-mix(in_srgb,var(--ok)_13%,transparent)] text-[color-mix(in_srgb,var(--ok)_85%,var(--fg))]',
                del && 'bg-[color-mix(in_srgb,var(--danger)_12%,transparent)] text-[color-mix(in_srgb,var(--danger)_85%,var(--fg))]',
                hunk && 'bg-accent-soft text-accent',
                meta && 'font-semibold text-muted',
              )}
            >
              {l || ' '}
            </div>
          );
        })}
      </div>
    </div>
  );
});

export function diffStats(diff: string): { add: number; del: number } {
  let add = 0;
  let del = 0;
  for (const l of diff.split('\n')) {
    if (l.startsWith('+++') || l.startsWith('---')) continue;
    if (l.startsWith('+')) add++;
    else if (l.startsWith('-')) del++;
  }
  return { add, del };
}
