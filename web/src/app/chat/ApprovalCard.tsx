import { useState } from 'react';
import { Bot, ChevronDown, ChevronRight, FileDiff, HelpCircle, KeyRound, SquareTerminal, Wrench } from 'lucide-react';
import type { Approval } from '../../proto/generated/Approval';
import type { ApprovalOption } from '../../proto/generated/ApprovalOption';
import { getConn } from '../../net/provider';
import { toastError } from '../../store/ui';
import { Button, cx } from '../ui/primitives';
import { DiffView } from './Diff';

const KIND_ICON = {
  command: SquareTerminal,
  file_change: FileDiff,
  tool: Wrench,
  permission: KeyRound,
  question: HelpCircle,
} as const;

const KIND_LABEL = { command: '运行命令', file_change: '修改文件', tool: '调用工具', permission: '请求权限', question: '问题' } as const;

function variantFor(o: ApprovalOption): 'primary' | 'secondary' | 'danger-soft' | 'ghost' {
  if (o.kind === 'allow') return 'primary';
  if (o.kind === 'abort') return 'danger-soft';
  if (o.kind === 'deny') return 'secondary';
  return 'secondary';
}

export function ApprovalCard({ host, session, approval }: { host: string; session: string; approval: Approval }) {
  const [busy, setBusy] = useState<string>();
  const [open, setOpen] = useState(false);
  const Icon = KIND_ICON[approval.kind] ?? HelpCircle;
  const hasDetail = !!(approval.detail || approval.diff);
  const choices = approval.options.filter((o) => o.kind === 'choice');
  const actions = approval.options.filter((o) => o.kind !== 'choice');
  const order = { allow: 0, allow_always: 1, deny: 2, abort: 3, choice: 4 } as const;
  actions.sort((a, b) => order[a.kind] - order[b.kind]);

  const respond = async (o: ApprovalOption) => {
    const conn = getConn(host);
    if (!conn || busy) return;
    setBusy(o.id);
    try {
      await conn.request({ op: 'approval_respond', session, approval: approval.id, option: o.id });
    } catch (err) {
      toastError('审批失败', err);
      setBusy(undefined);
    }
  };

  return (
    <div className="rounded-lg border border-warn/50 bg-bg shadow-[0_1px_0_rgb(0_0_0/0.02)]">
      <div className="flex items-start gap-2.5 px-3 pt-2.5">
        <span className="mt-0.5 flex h-6 w-6 shrink-0 items-center justify-center rounded-md bg-warn-soft text-warn">
          <Icon size={15} />
        </span>
        <div className="min-w-0 flex-1">
          <div className="flex min-w-0 items-center gap-1 text-[12px] text-muted">
            {approval.thread && (
              <span className="inline-flex min-w-0 items-center gap-1 text-accent">
                <Bot size={12} className="shrink-0" />
                <span className="truncate">{approval.thread_name ? `子智能体 ${approval.thread_name}` : '子智能体'}</span>
                <span className="text-faint">·</span>
              </span>
            )}
            <span className="shrink-0">{KIND_LABEL[approval.kind] ?? '审批'}</span>
          </div>
          <div className="text-[14px] font-medium break-words">{approval.title}</div>
        </div>
      </div>
      <div className="flex flex-col gap-1.5 px-3 pt-2">
        {approval.command && (
          <pre className="scroll-thin max-h-32 overflow-auto rounded-md border border-line bg-code px-2.5 py-1.5 font-mono text-[12.5px] whitespace-pre-wrap break-all">
            <span className="text-faint">$ </span>
            {approval.command}
          </pre>
        )}
        {approval.cwd && (
          <div className="truncate font-mono text-[12px] text-muted" title={approval.cwd}>
            {approval.cwd}
          </div>
        )}
        {approval.reason && <div className="text-[13px] break-words text-muted">{approval.reason}</div>}
        {hasDetail && (
          <div>
            <button type="button" onClick={() => setOpen((o) => !o)} className="-ml-1 inline-flex h-7 items-center gap-1 rounded px-1 text-[12.5px] text-muted hover:text-fg max-md:h-11 max-md:px-2">
              {open ? <ChevronDown size={14} /> : <ChevronRight size={14} />}
              {approval.diff ? '查看改动' : '详情'}
            </button>
            {open && (
              <div className="mt-1 flex flex-col gap-1.5">
                {approval.diff && <DiffView diff={approval.diff} className="max-h-72" />}
                {approval.detail && (
                  <pre className="scroll-thin max-h-48 overflow-auto rounded-md border border-line bg-code px-2.5 py-1.5 font-mono text-[12px] whitespace-pre-wrap break-all">{approval.detail}</pre>
                )}
              </div>
            )}
          </div>
        )}
      </div>
      {choices.length > 0 && (
        <div className="flex flex-col gap-1 px-3 pt-2">
          {choices.map((o) => (
            <button
              key={o.id}
              type="button"
              disabled={!!busy}
              onClick={() => respond(o)}
              className={cx('flex min-h-9 items-center rounded-md border border-line-strong px-3 text-left text-[13.5px] hover:bg-hover disabled:opacity-50 max-md:min-h-11', busy === o.id && 'border-accent')}
            >
              <span className="break-words">{o.label}</span>
            </button>
          ))}
        </div>
      )}
      <div className="flex flex-wrap gap-1.5 px-3 pt-2.5 pb-3">
        {actions.map((o) => (
          <Button key={o.id} size="sm" variant={variantFor(o)} busy={busy === o.id} disabled={!!busy && busy !== o.id} onClick={() => respond(o)}>
            {o.label}
          </Button>
        ))}
      </div>
    </div>
  );
}
