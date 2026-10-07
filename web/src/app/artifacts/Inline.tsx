import { useEffect, useRef, useState } from 'react';
import { ImageOff } from 'lucide-react';
import { artifactFor, itemArtifacts, type Artifact } from '../../lib/artifacts';
import type { ChatItem } from '../../proto/generated/ChatItem';
import { cx, Spinner } from '../ui/primitives';
import { useArtifactCtx } from './context';
import { KIND_LABEL, KIND_TONE, KindIcon } from './kinds';
import { useHostFile, useObjectUrl } from './load';

/** True once the element has come near the screen (then stays true). */
function useNear<T extends Element>(): [React.RefObject<T | null>, boolean] {
  const ref = useRef<T>(null);
  const [near, setNear] = useState(false);
  useEffect(() => {
    const el = ref.current;
    if (!el || near) return;
    const io = new IntersectionObserver((es) => es.some((e) => e.isIntersecting) && setNear(true), { rootMargin: '600px 0px' });
    io.observe(el);
    return () => io.disconnect();
  }, [near]);
  return [ref, near];
}

function Thumb({ host, a }: { host: string; a: Artifact }) {
  const st = useHostFile(host, a.ref);
  const url = useObjectUrl(st.status === 'ready' ? st.blob : undefined);
  if (st.status === 'error')
    return (
      <span className="flex h-full w-full items-center justify-center gap-1.5 text-[12px] text-faint">
        <ImageOff size={14} /> 无法读取
      </span>
    );
  if (!url) return <Spinner className="text-faint" />;
  return <img src={url} alt={a.name} className="block max-h-[260px] w-auto max-w-full object-contain" draggable={false} />;
}

/** Image an agent looked at or generated, shown in place; tap opens the viewer. */
export function ImageThumbs({ item }: { item: ChatItem }) {
  const ctx = useArtifactCtx();
  const [ref, near] = useNear<HTMLDivElement>();
  if (!ctx) return null;
  const imgs = itemArtifacts(item, ctx.home, ctx.cwd).filter((a) => a.kind === 'image');
  if (!imgs.length) return null;
  return (
    <div ref={ref} className="mt-1 flex flex-wrap gap-2 pl-7 max-md:pl-6">
      {imgs.map((a) => (
        <button
          key={a.ref}
          type="button"
          title={a.ref}
          aria-label={`查看图片 ${a.name}`}
          disabled={!ctx.files}
          onClick={() => ctx.open(a)}
          className="flex min-h-24 min-w-32 items-center justify-center overflow-hidden rounded-md border border-line bg-panel disabled:cursor-default"
        >
          {ctx.files && near ? <Thumb host={ctx.host} a={a} /> : <KindIcon kind="image" size={18} className="text-faint" />}
        </button>
      ))}
    </div>
  );
}

export function ArtifactChip({ a, onOpen, disabled }: { a: Artifact; onOpen: () => void; disabled?: boolean }) {
  return (
    <button
      type="button"
      title={a.ref}
      disabled={disabled}
      onClick={onOpen}
      className="inline-flex h-9 max-w-full min-w-0 items-center gap-2 rounded-md border border-line bg-bg pr-3 pl-2 text-left text-[13px] hover:bg-hover disabled:opacity-60 max-md:h-10"
    >
      <span className={cx('flex h-6 w-6 shrink-0 items-center justify-center rounded bg-panel', KIND_TONE[a.kind])}>
        <KindIcon kind={a.kind} size={14} />
      </span>
      <span className="min-w-0 truncate font-medium">{a.name}</span>
      <span className="shrink-0 text-[12px] text-faint">{KIND_LABEL[a.kind]}</span>
    </button>
  );
}

/** Openable things an item mentions (files it wrote, paths and local URLs in the reply). */
export function ArtifactChips({ item, skipImages, className }: { item: ChatItem; skipImages?: boolean; className?: string }) {
  const ctx = useArtifactCtx();
  if (!ctx) return null;
  const list = itemArtifacts(item, ctx.home, ctx.cwd).filter((a) => !(skipImages && a.kind === 'image'));
  if (!list.length) return null;
  return (
    <div className={cx('flex flex-wrap gap-1.5', className)}>
      {list.slice(0, 6).map((a) => (
        <ArtifactChip key={a.ref} a={a} disabled={!ctx.files && a.kind !== 'web'} onOpen={() => ctx.open(a)} />
      ))}
    </div>
  );
}

/** Open `href` in the viewer when it is a host path or a host-local URL it can show. */
export function openInViewer(href: string | undefined, open?: (a: Artifact) => void): boolean {
  if (!href || !open) return false;
  const ref = href.replace(/^file:\/\//, '');
  if (/^[a-z][a-z0-9+.-]*:\/\//i.test(ref) && !/^http:\/\/(localhost|127\.0\.0\.1|\[::1\])[:/]/i.test(ref)) return false;
  const a = artifactFor(ref);
  if (!a) return false;
  open(a);
  return true;
}
