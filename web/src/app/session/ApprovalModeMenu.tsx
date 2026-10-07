import { useState } from 'react';
import { ChevronDown } from 'lucide-react';
import type { ApprovalMode } from '../../proto/generated/ApprovalMode';
import type { SessionInfo } from '../../proto/generated/SessionInfo';
import { cx, Menu, Spinner } from '../ui/primitives';
import { MODE_INFO, MODES, setApprovalMode } from './approval';

/**
 * Approval mode of a running chat, switchable in place. Sits in the composer toolbar so it is
 * one thumb tap away while approvals pile up.
 */
export function ApprovalModeMenu({ host, s, disabled }: { host: string; s: SessionInfo; disabled?: boolean }) {
  const [busy, setBusy] = useState<ApprovalMode>();
  const mode = s.approval;
  if (!mode) return null;
  const info = MODE_INFO[busy ?? mode];
  const Icon = info.icon;
  const pick = async (m: ApprovalMode) => {
    if (m === mode) return;
    setBusy(m);
    await setApprovalMode(host, s, m);
    setBusy(undefined);
  };
  return (
    <Menu
      label="审批模式"
      align="start"
      width={264}
      items={MODES.map((m) => ({
        label: MODE_INFO[m].label,
        hint: MODE_INFO[m].hint,
        icon: (() => {
          const I = MODE_INFO[m].icon;
          return <I size={16} className={MODE_INFO[m].tone} />;
        })(),
        checked: m === mode,
        tag: m !== mode && !s.approval_live ? '需重启' : undefined,
        onSelect: () => void pick(m),
      }))}
      trigger={(p) => (
        <button
          type="button"
          {...p}
          disabled={disabled || !!busy}
          title={`审批模式：${info.label}`}
          aria-label={`审批模式：${info.label}`}
          className={cx(
            'inline-flex h-8 shrink-0 items-center gap-1 rounded-md px-2 text-[13px] font-medium transition-colors hover:bg-hover disabled:opacity-50 max-md:h-11 max-md:px-2.5',
            (busy ?? mode) === 'yolo' ? 'text-warn' : (busy ?? mode) === 'auto' ? 'text-accent' : 'text-muted',
          )}
        >
          {busy ? <Spinner size={14} /> : <Icon size={15} />}
          <span>{info.short}</span>
          <ChevronDown size={13} className="opacity-60" />
        </button>
      )}
    />
  );
}
