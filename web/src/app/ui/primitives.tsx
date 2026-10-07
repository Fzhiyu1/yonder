import { forwardRef, useEffect, useId, useLayoutEffect, useRef, useState, type ButtonHTMLAttributes, type PointerEvent as ReactPointerEvent, type ReactNode } from 'react';
import { createPortal } from 'react-dom';
import { Check, Loader2, X } from 'lucide-react';

export function cx(...parts: Array<string | false | null | undefined>): string {
  return parts.filter(Boolean).join(' ');
}

export function Spinner({ size = 14, className }: { size?: number; className?: string }) {
  return <Loader2 size={size} className={cx('shrink-0 animate-spin', className)} aria-hidden />;
}

type IconButtonProps = ButtonHTMLAttributes<HTMLButtonElement> & {
  label: string;
  size?: 'sm' | 'md';
  active?: boolean;
};

export const IconButton = forwardRef<HTMLButtonElement, IconButtonProps>(function IconButton(
  { label, size = 'md', active, className, children, ...rest },
  ref,
) {
  return (
    <button
      ref={ref}
      type="button"
      title={label}
      aria-label={label}
      className={cx(
        'inline-flex shrink-0 items-center justify-center rounded-md text-muted transition-colors hover:bg-hover hover:text-fg disabled:opacity-40 disabled:hover:bg-transparent',
        size === 'sm' ? 'h-8 w-8 max-md:h-11 max-md:w-11' : 'h-9 w-9 max-md:h-11 max-md:w-11',
        active && 'bg-active text-fg',
        className,
      )}
      {...rest}
    >
      {children}
    </button>
  );
});

type ButtonProps = ButtonHTMLAttributes<HTMLButtonElement> & {
  variant?: 'primary' | 'secondary' | 'ghost' | 'danger' | 'danger-soft';
  size?: 'sm' | 'md';
  busy?: boolean;
  icon?: ReactNode;
};

export function Button({ variant = 'secondary', size = 'md', busy, icon, className, children, disabled, ...rest }: ButtonProps) {
  return (
    <button
      type="button"
      disabled={disabled || busy}
      className={cx(
        'inline-flex min-w-0 items-center justify-center gap-1.5 rounded-md border font-medium whitespace-nowrap transition-colors disabled:opacity-50',
        size === 'sm' ? 'h-8 px-2.5 text-[13px] max-md:h-11 max-md:px-3' : 'h-9 px-3.5 text-sm max-md:h-11',
        variant === 'primary' && 'border-accent bg-accent text-accent-fg hover:brightness-110',
        variant === 'secondary' && 'border-line-strong bg-bg text-fg hover:bg-hover',
        variant === 'ghost' && 'border-transparent text-muted hover:bg-hover hover:text-fg',
        variant === 'danger' && 'border-danger bg-danger text-white hover:brightness-110',
        variant === 'danger-soft' && 'border-line-strong bg-bg text-danger hover:bg-danger-soft',
        className,
      )}
      {...rest}
    >
      {busy ? <Spinner size={14} /> : icon}
      {children !== undefined && <span className="truncate">{children}</span>}
    </button>
  );
}

export function StatusDot({ status, className }: { status: 'online' | 'connecting' | 'offline' | 'error'; className?: string }) {
  const color =
    status === 'online' ? 'bg-ok' : status === 'connecting' ? 'bg-warn pulse-dot' : status === 'error' ? 'bg-danger' : 'bg-faint';
  const label = status === 'online' ? '在线' : status === 'connecting' ? '连接中' : status === 'error' ? '错误' : '离线';
  return <span title={label} aria-label={label} className={cx('inline-block h-2 w-2 shrink-0 rounded-full', color, className)} />;
}

export function Segmented<T extends string>({
  value,
  onChange,
  options,
  className,
  size = 'md',
}: {
  value: T;
  onChange: (v: T) => void;
  options: Array<{ value: T; label: ReactNode; disabled?: boolean; title?: string }>;
  className?: string;
  size?: 'sm' | 'md';
}) {
  return (
    <div role="radiogroup" className={cx('inline-flex rounded-md border border-line bg-panel p-0.5', className)}>
      {options.map((o) => (
        <button
          key={o.value}
          type="button"
          role="radio"
          aria-checked={value === o.value}
          title={o.title}
          disabled={o.disabled}
          onClick={() => onChange(o.value)}
          className={cx(
            'inline-flex min-w-0 flex-1 items-center justify-center gap-1.5 rounded-[5px] px-3 whitespace-nowrap transition-colors disabled:opacity-40',
            size === 'sm' ? 'h-7 text-[13px] max-md:h-10' : 'h-8 text-sm max-md:h-11',
            value === o.value ? 'bg-bg text-fg shadow-[0_1px_2px_rgb(0_0_0/0.08)] dark:bg-active' : 'text-muted hover:text-fg',
          )}
        >
          {o.label}
        </button>
      ))}
    </div>
  );
}

export function Toggle({ checked, onChange, label, disabled }: { checked: boolean; onChange: (v: boolean) => void; label: string; disabled?: boolean }) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      aria-label={label}
      title={label}
      disabled={disabled}
      onClick={() => onChange(!checked)}
      className={cx(
        'relative inline-flex h-6 w-10 shrink-0 items-center rounded-full border transition-colors disabled:opacity-40',
        checked ? 'border-accent bg-accent' : 'border-line-strong bg-active',
      )}
    >
      <span className={cx('absolute h-[18px] w-[18px] rounded-full bg-white shadow transition-transform', checked ? 'translate-x-[19px]' : 'translate-x-[2px]')} />
    </button>
  );
}

/**
 * Labeled form field. `group` is for composite controls (segmented buttons etc.): a
 * `<label>` would name only the first button inside it with the whole group's text.
 */
export function Field({ label, children, hint, group }: { label: string; children: ReactNode; hint?: ReactNode; group?: boolean }) {
  const id = useId();
  if (group) {
    return (
      <div role="group" aria-labelledby={id} className="flex min-w-0 flex-col gap-1.5">
        <span id={id} className="text-[13px] font-medium text-muted">
          {label}
        </span>
        {children}
        {hint && <span className="text-xs text-faint">{hint}</span>}
      </div>
    );
  }
  return (
    <label className="flex min-w-0 flex-col gap-1.5">
      <span className="text-[13px] font-medium text-muted">{label}</span>
      {children}
      {hint && <span className="text-xs text-faint">{hint}</span>}
    </label>
  );
}

export const inputClass =
  'h-9 w-full min-w-0 rounded-md border border-line-strong bg-bg px-2.5 text-sm text-fg placeholder:text-faint focus:border-accent focus:outline-none max-md:h-11';

export function useMediaQuery(q: string): boolean {
  const [m, setM] = useState(() => (typeof window !== 'undefined' ? window.matchMedia(q).matches : false));
  useEffect(() => {
    const mq = window.matchMedia(q);
    const on = () => setM(mq.matches);
    mq.addEventListener('change', on);
    on();
    return () => mq.removeEventListener('change', on);
  }, [q]);
  return m;
}

export const useIsMobile = () => useMediaQuery('(max-width: 767px)');
export const useIsDesktop = () => useMediaQuery('(min-width: 1024px)');

/** Keeps something mounted for `ms` after `open` turns false, so it can animate out. */
export function usePresence(open: boolean, ms = 240): boolean {
  const [present, setPresent] = useState(open);
  useEffect(() => {
    if (open) {
      setPresent(true);
      return;
    }
    const t = setTimeout(() => setPresent(false), ms);
    return () => clearTimeout(t);
  }, [open, ms]);
  return open || present;
}

/** The part of the layout viewport that is visible (the soft keyboard covers the rest on iOS). */
export function visibleBand(): { top: number; bottom: number } {
  const vv = window.visualViewport;
  if (!vv || Math.abs(vv.scale - 1) > 0.01) return { top: 0, bottom: window.innerHeight };
  return { top: vv.offsetTop, bottom: vv.offsetTop + vv.height };
}

/**
 * Modal dialog on desktop, bottom sheet on mobile (drag the top edge down to close). Sized to
 * the visible viewport, so its footer stays above the iOS keyboard.
 */
export function Sheet({
  open,
  onClose,
  title,
  children,
  footer,
  width = 560,
  bodyClassName,
}: {
  open: boolean;
  onClose: () => void;
  title: ReactNode;
  children: ReactNode;
  footer?: ReactNode;
  width?: number;
  bodyClassName?: string;
}) {
  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose();
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [open, onClose]);
  const present = usePresence(open);
  const panel = useRef<HTMLDivElement>(null);
  const drag = useRef<{ y: number; dy: number } | null>(null);
  if (!present) return null;
  const state = open ? 'open' : 'closed';

  const dragStart = (e: ReactPointerEvent<HTMLDivElement>) => {
    if (e.pointerType === 'mouse' || !open || (e.target as HTMLElement).closest('button, input, a')) return;
    drag.current = { y: e.clientY, dy: 0 };
    e.currentTarget.setPointerCapture(e.pointerId);
  };
  const dragMove = (e: ReactPointerEvent<HTMLDivElement>) => {
    const d = drag.current;
    if (!d || !panel.current) return;
    d.dy = Math.max(0, e.clientY - d.y);
    panel.current.style.transition = 'none';
    panel.current.style.transform = `translateY(${d.dy}px)`;
  };
  const dragEnd = () => {
    const d = drag.current;
    drag.current = null;
    if (!d || !panel.current) return;
    // From the dragged position the panel either slides out (closed state) or back.
    panel.current.style.transition = '';
    panel.current.style.transform = '';
    if (d.dy > 72) onClose();
  };

  return createPortal(
    <div
      data-state={state}
      // Animating out: not clickable or focusable any more.
      inert={!open}
      className="sheet-overlay app-band fixed inset-x-0 z-50 flex items-end justify-center bg-overlay md:items-center md:p-6"
      onMouseDown={(e) => open && e.target === e.currentTarget && onClose()}
    >
      <div
        ref={panel}
        role="dialog"
        aria-modal="true"
        aria-hidden={!open || undefined}
        data-state={state}
        style={{ maxWidth: width }}
        className="sheet-panel safe-bottom flex max-h-[94%] w-full flex-col rounded-t-xl border border-line bg-bg shadow-pop md:max-h-[86%] md:rounded-lg"
      >
        <div className="shrink-0 touch-none border-b border-line" onPointerDown={dragStart} onPointerMove={dragMove} onPointerUp={dragEnd} onPointerCancel={dragEnd}>
          <div className="flex justify-center pt-1.5 md:hidden" aria-hidden>
            <span className="h-1 w-9 rounded-full bg-line-strong" />
          </div>
          <div className="flex h-12 items-center gap-2 pr-2 pl-4 max-md:h-11">
            <h2 className="min-w-0 flex-1 truncate text-[15px] font-semibold">{title}</h2>
            <IconButton label="关闭" onClick={onClose}>
              <X size={18} />
            </IconButton>
          </div>
        </div>
        <div className={cx('scroll-thin min-h-0 flex-1 overflow-y-auto overscroll-contain p-4', bodyClassName)}>{children}</div>
        {footer && <div className="flex shrink-0 flex-wrap items-center justify-end gap-2 border-t border-line px-4 py-3 max-md:py-2.5">{footer}</div>}
      </div>
    </div>,
    document.body,
  );
}

export interface MenuItem {
  label: string;
  icon?: ReactNode;
  onSelect: () => void;
  danger?: boolean;
  disabled?: boolean;
  hidden?: boolean;
  /** Second line under the label. */
  hint?: string;
  /** A choice among the items (radio): shows a check when true. */
  checked?: boolean;
  /** Short tag at the end of the row. */
  tag?: string;
}

/** Dropdown menu anchored to a trigger button. */
export function Menu({
  trigger,
  items,
  align = 'end',
  width = 220,
  label,
}: {
  trigger: (props: { onClick: () => void; ref: React.Ref<HTMLButtonElement>; 'aria-expanded': boolean; 'aria-haspopup': 'menu' }) => ReactNode;
  items: MenuItem[];
  align?: 'start' | 'end';
  width?: number;
  /** Heading inside the menu. */
  label?: string;
}) {
  const [open, setOpen] = useState(false);
  const btn = useRef<HTMLButtonElement>(null);
  const pop = useRef<HTMLDivElement>(null);
  const [pos, setPos] = useState<{ top: number; left: number; up: boolean }>();
  const visible = items.filter((i) => !i.hidden);
  const radio = visible.some((i) => i.checked !== undefined);

  // Measure the rendered menu, then place it below the trigger, or above when it does not fit
  // in the visible part of the screen (keyboard up, trigger near the bottom).
  useLayoutEffect(() => {
    if (!open || !btn.current || !pop.current) {
      setPos(undefined);
      return;
    }
    const r = btn.current.getBoundingClientRect();
    const h = pop.current.offsetHeight;
    const band = visibleBand();
    const up = r.bottom + h + 8 > band.bottom && r.top - h - 8 >= band.top;
    let left = align === 'end' ? r.right - width : r.left;
    left = Math.max(8, Math.min(left, window.innerWidth - width - 8));
    setPos({ top: up ? r.top - h - 4 : r.bottom + 4, left, up });
  }, [open, align, visible.length, width]);

  useEffect(() => {
    if (!open) return;
    const close = (e: Event) => {
      if (pop.current?.contains(e.target as Node) || btn.current?.contains(e.target as Node)) return;
      setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => e.key === 'Escape' && setOpen(false);
    const onResize = () => setOpen(false);
    document.addEventListener('pointerdown', close);
    window.addEventListener('keydown', onKey);
    window.addEventListener('resize', onResize);
    return () => {
      document.removeEventListener('pointerdown', close);
      window.removeEventListener('keydown', onKey);
      window.removeEventListener('resize', onResize);
    };
  }, [open]);

  return (
    <>
      {trigger({ onClick: () => setOpen((o) => !o), ref: btn, 'aria-expanded': open, 'aria-haspopup': 'menu' })}
      {open &&
        createPortal(
          <div
            ref={pop}
            role="menu"
            aria-label={label}
            data-up={pos?.up || undefined}
            style={{ top: pos?.top ?? 0, left: pos?.left ?? 0, width, visibility: pos ? undefined : 'hidden' }}
            className="menu-pop fixed z-[60] rounded-lg border border-line bg-bg p-1 shadow-pop"
          >
            {label && <div className="px-2.5 pt-1.5 pb-1 text-[12px] font-medium text-muted">{label}</div>}
            {visible.map((it) => (
              <button
                key={it.label}
                type="button"
                role={radio ? 'menuitemradio' : 'menuitem'}
                aria-checked={radio ? !!it.checked : undefined}
                disabled={it.disabled}
                onClick={() => {
                  setOpen(false);
                  it.onSelect();
                }}
                className={cx(
                  'flex w-full items-center gap-2.5 rounded-md px-2.5 text-left text-sm disabled:opacity-40',
                  it.hint ? 'min-h-12 py-1.5' : 'min-h-9 max-md:min-h-11',
                  it.danger ? 'text-danger hover:bg-danger-soft' : 'text-fg hover:bg-hover',
                )}
              >
                <span className="flex w-4 shrink-0 justify-center text-current opacity-80">{it.icon}</span>
                <span className="min-w-0 flex-1">
                  <span className="flex items-center gap-2">
                    <span className="truncate">{it.label}</span>
                    {it.tag && <span className="shrink-0 rounded border border-line px-1 text-[11px] leading-4 text-muted">{it.tag}</span>}
                  </span>
                  {it.hint && <span className="block text-[12.5px] leading-4 break-all text-muted">{it.hint}</span>}
                </span>
                {radio && <Check size={15} className={cx('shrink-0 text-accent', !it.checked && 'invisible')} />}
              </button>
            ))}
          </div>,
          document.body,
        )}
    </>
  );
}

export function EmptyState({ icon, title, children }: { icon?: ReactNode; title: string; children?: ReactNode }) {
  return (
    <div className="flex h-full min-h-40 flex-col items-center justify-center gap-2 p-6 text-center text-muted">
      {icon && <div className="text-faint">{icon}</div>}
      <div className="text-sm font-medium text-fg">{title}</div>
      {children}
    </div>
  );
}

export function Kbd({ children }: { children: ReactNode }) {
  return <kbd className="rounded border border-line bg-panel px-1 font-mono text-[11px] text-muted">{children}</kbd>;
}
