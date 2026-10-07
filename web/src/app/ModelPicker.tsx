import { forwardRef, useEffect, useId, useLayoutEffect, useMemo, useRef, useState, type KeyboardEvent } from 'react';
import { Check, ChevronsUpDown, CornerDownLeft, Search, X } from 'lucide-react';
import { filterModels, pickerModels } from '../lib/models';
import { cx, IconButton, inputClass, Spinner, useMediaQuery } from './ui/primitives';

/** Field-like button showing the chosen model; toggles `ModelPicker`. */
export const ModelButton = forwardRef<HTMLButtonElement, { value: string; defaultModel?: string; open: boolean; disabled?: boolean; onClick: () => void }>(
  function ModelButton({ value, defaultModel, open, disabled, onClick }, ref) {
    return (
      <button
        ref={ref}
        type="button"
        disabled={disabled}
        aria-haspopup="listbox"
        aria-expanded={open}
        aria-label={`模型：${value || '默认'}`}
        onClick={onClick}
        className={cx(inputClass, 'flex items-center gap-2 text-left disabled:opacity-50', open && 'border-accent')}
      >
        <span className={cx('min-w-0 flex-1 truncate', value ? 'font-mono text-[13px] max-md:text-[15px]' : 'text-faint')}>{value || '默认'}</span>
        {!!value && value === defaultModel && <span className="shrink-0 text-[12px] text-faint">默认</span>}
        <ChevronsUpDown size={14} className="shrink-0 text-muted" />
      </button>
    );
  },
);

type Option = { key: string; kind: 'default' | 'model' | 'custom'; value: string };

/** Inline searchable model list for the new-session dialog. Text not in the list is offered as a custom id. */
export function ModelPicker({
  models,
  value,
  defaultModel,
  loading,
  onPick,
  onClose,
}: {
  models: string[];
  value: string;
  defaultModel?: string;
  /** Host info (and with it the list) is still on its way. */
  loading?: boolean;
  onPick: (model: string) => void;
  onClose: () => void;
}) {
  const fine = useMediaQuery('(pointer: fine)');
  const listId = useId();
  const input = useRef<HTMLInputElement>(null);
  const list = useRef<HTMLDivElement>(null);
  const [query, setQuery] = useState('');
  const q = query.trim();

  const ordered = useMemo(() => pickerModels(models, defaultModel, value), [models, defaultModel, value]);
  const options = useMemo((): Option[] => {
    const out: Option[] = [];
    // Without a known default the agent picks its own model when none is given.
    if (!q && !defaultModel) out.push({ key: '\u0000default', kind: 'default', value: '' });
    for (const m of filterModels(ordered, q)) out.push({ key: m, kind: 'model', value: m });
    if (q && !ordered.includes(q)) out.push({ key: '\u0000custom', kind: 'custom', value: q });
    return out;
  }, [ordered, q, defaultModel]);
  const matched = options.filter((o) => o.kind === 'model').length;

  const [active, setActive] = useState(() => Math.max(0, options.findIndex((o) => o.kind !== 'custom' && o.value === value)));
  const current = Math.min(active, options.length - 1);

  useEffect(() => {
    // Phones: keep the keyboard down so the whole list is visible.
    if (fine) input.current?.focus({ preventScroll: true });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useLayoutEffect(() => {
    list.current?.querySelector(`[data-idx="${current}"]`)?.scrollIntoView({ block: 'nearest' });
  }, [current]);

  const onKeyDown = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.nativeEvent.isComposing) return;
    if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
      e.preventDefault();
      const d = e.key === 'ArrowDown' ? 1 : -1;
      setActive((current + d + options.length) % options.length);
    } else if (e.key === 'Enter') {
      // The picker lives inside the dialog's form: Enter picks, it never creates the session.
      e.preventDefault();
      const o = options[current];
      if (o) onPick(o.value);
    } else if (e.key === 'Escape') {
      e.preventDefault();
      e.stopPropagation();
      onClose();
    }
  };

  return (
    <div className="flex flex-col overflow-hidden rounded-lg border border-line bg-bg focus-within:border-accent">
      <div className="flex h-10 items-center gap-2 border-b border-line pr-1 pl-2.5">
        <Search size={15} className="shrink-0 text-muted" />
        <input
          ref={input}
          value={query}
          onChange={(e) => {
            setQuery(e.target.value);
            setActive(0);
          }}
          onKeyDown={onKeyDown}
          placeholder="搜索或输入模型"
          role="combobox"
          aria-label="搜索模型"
          aria-expanded
          aria-controls={listId}
          aria-autocomplete="list"
          aria-activedescendant={options.length ? `${listId}-${current}` : undefined}
          autoCapitalize="off"
          autoCorrect="off"
          spellCheck={false}
          enterKeyHint="done"
          className="h-full min-w-0 flex-1 bg-transparent font-mono text-[13px] placeholder:font-sans placeholder:text-faint focus:outline-none focus-visible:outline-none"
        />
        {ordered.length > 0 && <span className="shrink-0 text-[12px] text-faint tabular-nums">{matched}</span>}
        <IconButton label="关闭" size="sm" onClick={onClose}>
          <X size={15} />
        </IconButton>
      </div>
      <div ref={list} id={listId} role="listbox" aria-label="模型" className="scroll-thin max-h-60 overflow-y-auto overscroll-contain p-1">
        {options.map((o, i) => {
          const selected = o.kind === 'default' ? !value : o.kind === 'model' && o.value === value;
          return (
            <button
              key={o.key}
              type="button"
              role="option"
              id={`${listId}-${i}`}
              data-idx={i}
              tabIndex={-1}
              aria-selected={selected}
              onMouseMove={fine && i !== current ? () => setActive(i) : undefined}
              onClick={() => onPick(o.value)}
              className={cx(
                'flex h-8 w-full min-w-0 items-center gap-2 rounded-md px-2 text-left text-[13px] max-md:h-11 max-md:text-[14px]',
                fine && i === current ? 'bg-hover' : 'active:bg-hover',
              )}
            >
              {o.kind === 'custom' ? (
                <CornerDownLeft size={14} className="shrink-0 text-muted" />
              ) : (
                <Check size={14} className={cx('shrink-0 text-accent', !selected && 'invisible')} />
              )}
              {o.kind === 'default' && <span className="min-w-0 flex-1 truncate">默认</span>}
              {o.kind === 'model' && <span className="min-w-0 flex-1 truncate font-mono">{o.value}</span>}
              {o.kind === 'custom' && (
                <span className="min-w-0 flex-1 truncate">
                  使用 <span className="font-mono">{o.value}</span>
                </span>
              )}
              {o.kind === 'model' && o.value === defaultModel && <span className="shrink-0 text-[12px] text-faint">默认</span>}
            </button>
          );
        })}
        {loading && !ordered.length && (
          <div className="flex items-center gap-2 px-2 py-1.5 text-[12.5px] text-muted">
            <Spinner size={12} /> 正在读取模型列表
          </div>
        )}
        {!loading && !ordered.length && !q && <div className="px-2 py-1.5 text-[12.5px] text-faint">主机没有提供模型列表，可直接输入模型名</div>}
      </div>
    </div>
  );
}
