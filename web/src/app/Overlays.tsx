import { AlertTriangle, CheckCircle2, Info, X, XCircle } from 'lucide-react';
import { useUi } from '../store/ui';
import { Button, cx, Sheet } from './ui/primitives';

export function Toasts() {
  const toasts = useUi((s) => s.toasts);
  const dismiss = useUi((s) => s.dismissToast);
  return (
    <div
      style={{ top: 'var(--vv-top, 0px)' }}
      className="pointer-events-none fixed inset-x-0 z-[70] flex flex-col items-center gap-2 px-3 pt-[calc(env(safe-area-inset-top)+12px)] md:items-end md:pr-4"
    >
      {toasts.map((t) => (
        <div
          key={t.id}
          role="status"
          className="rise-in pointer-events-auto flex w-full max-w-sm items-start gap-2.5 rounded-lg border border-line bg-bg px-3 py-2.5 text-sm shadow-pop"
        >
          <span
            className={cx(
              'mt-0.5 shrink-0',
              t.level === 'error' ? 'text-danger' : t.level === 'warning' ? 'text-warn' : t.level === 'success' ? 'text-ok' : 'text-accent',
            )}
          >
            {t.level === 'error' ? <XCircle size={16} /> : t.level === 'warning' ? <AlertTriangle size={16} /> : t.level === 'success' ? <CheckCircle2 size={16} /> : <Info size={16} />}
          </span>
          <span className="min-w-0 flex-1 break-words">{t.message}</span>
          <button type="button" aria-label="关闭" title="关闭" onClick={() => dismiss(t.id)} className="-my-1.5 -mr-2 inline-flex h-8 w-8 shrink-0 items-center justify-center rounded text-faint hover:text-fg">
            <X size={14} />
          </button>
        </div>
      ))}
    </div>
  );
}

export function ConfirmDialog() {
  const c = useUi((s) => s.confirm);
  const close = useUi((s) => s.closeConfirm);
  return (
    <Sheet
      open={!!c}
      onClose={() => close(false)}
      title={c?.title ?? ''}
      width={420}
      footer={
        <>
          <Button onClick={() => close(false)}>取消</Button>
          <Button variant={c?.destructive ? 'danger' : 'primary'} onClick={() => close(true)} autoFocus>
            {c?.confirmLabel ?? '确定'}
          </Button>
        </>
      }
    >
      {c?.message && <p className="text-sm break-words text-muted">{c.message}</p>}
    </Sheet>
  );
}
