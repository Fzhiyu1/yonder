import { useMemo, useState } from 'react';
import { Layers } from 'lucide-react';
import { chatArtifacts } from '../../lib/artifacts';
import type { ChatItem } from '../../proto/generated/ChatItem';
import { useViewer } from '../../store/viewer';
import { cx, EmptyState, IconButton, Segmented, Sheet } from '../ui/primitives';
import { useArtifactCtx } from './context';
import { KIND_LABEL, KIND_TONE, KindIcon } from './kinds';

type Filter = 'all' | 'image' | 'doc' | 'web';

/** Header button: every artifact of the chat, newest first. */
export function ArtifactsButton({ items }: { items: ChatItem[] }) {
  const ctx = useArtifactCtx();
  const [open, setOpen] = useState(false);
  const [filter, setFilter] = useState<Filter>('all');
  const opened = useViewer((s) => (ctx ? (s.byChat[ctx.chat]?.tabs.length ?? 0) : 0));
  const all = useMemo(() => (ctx ? chatArtifacts(items, ctx.home, ctx.cwd) : []), [items, ctx]);
  if (!ctx) return null;
  const list = all.filter((a) => filter === 'all' || (filter === 'image' ? a.kind === 'image' : filter === 'web' ? a.kind === 'web' || a.kind === 'html' : a.kind === 'pdf' || a.kind === 'text'));
  const count = all.length;
  return (
    <>
      <IconButton label={`产物（${count}）`} onClick={() => (opened && !count ? useViewer.getState().show(ctx.chat) : setOpen(true))} className="relative">
        <Layers size={17} />
        {count > 0 && (
          <span className="absolute top-1 right-0.5 min-w-4 rounded-full bg-accent px-1 text-[10px] leading-4 font-semibold text-accent-fg tabular-nums max-md:top-1.5 max-md:right-1">
            {count > 99 ? '99+' : count}
          </span>
        )}
      </IconButton>
      <Sheet open={open} onClose={() => setOpen(false)} title="产物" width={520} bodyClassName="p-0">
        <div className="sticky top-0 z-10 border-b border-line bg-bg px-3 py-2">
          <Segmented<Filter>
            size="sm"
            className="w-full"
            value={filter}
            onChange={setFilter}
            options={[
              { value: 'all', label: '全部' },
              { value: 'image', label: '图片' },
              { value: 'doc', label: '文档' },
              { value: 'web', label: '网页' },
            ]}
          />
        </div>
        {!list.length ? (
          <EmptyState icon={<Layers size={22} />} title={count ? '没有这类产物' : '这个对话还没有产物'} />
        ) : (
          <ul className="p-1.5">
            {list.map((a) => {
              const disabled = !ctx.files && a.kind !== 'web';
              return (
                <li key={a.ref}>
                  <button
                    type="button"
                    disabled={disabled}
                    onClick={() => {
                      setOpen(false);
                      ctx.open(a);
                    }}
                    className="flex min-h-12 w-full min-w-0 items-center gap-3 rounded-md px-2.5 py-1.5 text-left hover:bg-hover disabled:opacity-50 max-md:min-h-14"
                  >
                    <span className={cx('flex h-8 w-8 shrink-0 items-center justify-center rounded-md bg-panel', KIND_TONE[a.kind])}>
                      <KindIcon kind={a.kind} size={16} />
                    </span>
                    <span className="flex min-w-0 flex-1 flex-col">
                      <span className="truncate text-[14px] font-medium">{a.name}</span>
                      <span className="truncate font-mono text-[11.5px] text-faint" title={a.ref}>
                        {a.ref}
                      </span>
                    </span>
                    <span className="shrink-0 text-[12px] text-muted">{KIND_LABEL[a.kind]}</span>
                  </button>
                </li>
              );
            })}
          </ul>
        )}
      </Sheet>
    </>
  );
}
