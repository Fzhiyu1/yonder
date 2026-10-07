import { lazy, Suspense, useEffect, useMemo, useState } from 'react';
import { ChevronDown, Copy, ExternalLink, Link2, MoreHorizontal, PanelRightClose, RotateCw, Share, X } from 'lucide-react';
import type { Artifact } from '../../lib/artifacts';
import { copyText } from '../../lib/clipboard';
import { middleTruncate } from '../../lib/format';
import { useUi } from '../../store/ui';
import { useViewer, viewerOf } from '../../store/viewer';
import { saveBlob } from '../files/transfer';
import { Markdown } from '../chat/Markdown';
import { cx, IconButton, Menu, type MenuItem, Spinner, useIsDesktop } from '../ui/primitives';
import { openExternal, useTailnetUrl, type TailnetState } from './external';
import { KIND_TONE, KindIcon } from './kinds';
import { getHostFile, useHostFile, useObjectUrl, type LoadState } from './load';

const ImageView = lazy(() => import('./ImageView').then((m) => ({ default: m.ImageView })));
const PdfView = lazy(() => import('./PdfView').then((m) => ({ default: m.PdfView })));
const PageView = lazy(() => import('./PageView').then((m) => ({ default: m.PageView })));

const busy = (label = '正在加载') => (
  <div className="flex h-full items-center justify-center gap-2 text-[13px] text-muted">
    <Spinner /> {label}
  </div>
);

function Progress({ st }: { st: Extract<LoadState, { status: 'loading' }> }) {
  const pct = st.total ? Math.round((st.got / st.total) * 100) : 0;
  return (
    <div className="flex h-full flex-col items-center justify-center gap-3 text-[13px] text-muted">
      <Spinner size={18} />
      <div className="h-1 w-40 overflow-hidden rounded-full bg-active">
        <div className="h-full bg-accent transition-[width]" style={{ width: `${pct}%` }} />
      </div>
      <span className="tabular-nums">{st.total ? `${(st.got / 1048576).toFixed(1)} / ${(st.total / 1048576).toFixed(1)} MB` : '正在读取'}</span>
    </div>
  );
}

function Failed({ message }: { message: string }) {
  const outside = /outside the allowed folders/i.test(message);
  return (
    <div className="flex h-full flex-col items-center justify-center gap-1 p-6 text-center">
      <div className="text-[14px] font-medium">无法打开</div>
      <div className="max-w-sm text-[13px] break-words text-muted">{outside ? '这个文件不在主机允许访问的文件夹内（主机配置 fs_roots，默认只有主目录）。' : message}</div>
    </div>
  );
}

function TextBody({ blob, name }: { blob: Blob; name: string }) {
  const [text, setText] = useState<string>();
  useEffect(() => {
    let live = true;
    void blob
      .slice(0, 2 * 1024 * 1024)
      .text()
      .then((t) => live && setText(t));
    return () => {
      live = false;
    };
  }, [blob]);
  if (text === undefined) return busy();
  if (/\.(md|markdown)$/i.test(name))
    return (
      <div className="scroll-thin h-full overflow-y-auto overscroll-contain">
        <Markdown text={text} className="mx-auto max-w-3xl px-4 py-4 text-[14.5px]" />
      </div>
    );
  return <pre className="scroll-thin h-full overflow-auto overscroll-contain bg-code px-3 py-3 font-mono text-[12.5px] leading-[1.55] whitespace-pre-wrap break-all">{text}</pre>;
}

function FileBody({ host, a, reload }: { host: string; a: Artifact; reload: number }) {
  const st = useHostFile(host, a.ref, reload);
  const blob = st.status === 'ready' ? st.blob : undefined;
  const url = useObjectUrl(a.kind === 'image' ? blob : undefined);
  if (st.status === 'loading') return <Progress st={st} />;
  if (st.status === 'error') return <Failed message={st.message} />;
  if (a.kind === 'image') return url ? <ImageView url={url} name={a.name} /> : busy();
  if (a.kind === 'pdf') return <PdfView blob={st.blob} name={a.name} />;
  return <TextBody blob={st.blob} name={a.name} />;
}

function Body({ host, a, reload }: { host: string; a: Artifact; reload: number }) {
  return (
    <Suspense fallback={busy()}>
      {a.kind === 'web' || a.kind === 'html' ? <PageView host={host} a={a} reload={reload} /> : <FileBody host={host} a={a} reload={reload} />}
    </Suspense>
  );
}

async function shareArtifact(host: string, a: Artifact) {
  try {
    await saveBlob(await getHostFile(host, a.ref), a.name);
  } catch (err) {
    useUi.getState().toast('error', `分享失败：${err instanceof Error ? err.message : String(err)}`);
  }
}

async function copy(text: string, done: string) {
  if (await copyText(text)) useUi.getState().toast('success', done);
}

function browserHint(t: TailnetState | undefined): string {
  if (!t || t.status === 'loading') return '正在查找主机的 Tailscale 地址';
  if (t.status === 'none') return t.reason;
  return t.reachable ? `经 Tailscale：${t.url}` : '端口只在主机本机监听，需让开发服务器监听 0.0.0.0';
}

/** Actions on the shown artifact: open in the system browser, copy, share. */
function actionsFor(host: string, a: Artifact, tailnet: TailnetState | undefined): MenuItem[] {
  if (a.kind === 'web') {
    const ready = tailnet?.status === 'ready' ? tailnet : undefined;
    return [
      { label: '在浏览器打开', icon: <ExternalLink size={15} />, hint: browserHint(tailnet), disabled: !ready, onSelect: () => ready && openExternal(ready.url) },
      { label: '复制 Tailscale 地址', icon: <Link2 size={15} />, hidden: !ready, onSelect: () => ready && void copy(ready.url, '已复制地址') },
      { label: '复制原地址', icon: <Copy size={15} />, onSelect: () => void copy(a.ref, '已复制地址') },
    ];
  }
  return [
    { label: '分享或保存', icon: <Share size={15} />, hint: '可用其他 App 打开', onSelect: () => void shareArtifact(host, a) },
    { label: '复制路径', icon: <Copy size={15} />, onSelect: () => void copy(a.ref, '已复制路径') },
  ];
}

/**
 * Artifacts opened from one chat. A full-height window over the chat on phones and tablets, a
 * panel next to it on wide screens. The chat stays mounted underneath, so its scroll position
 * and live updates survive opening and closing the viewer.
 */
export function Viewer({ host, chat }: { host: string; chat: string }) {
  const v = useViewer(useMemo(() => viewerOf(chat), [chat]));
  const reloads = useViewer((s) => s.reloads);
  const { hide, show, closeTab, reload } = useViewer.getState();
  const desktop = useIsDesktop();
  // Tabs that were shown once stay mounted (pages keep their state when switching tabs).
  const [seen, setSeen] = useState<Set<string>>(() => new Set());
  const active = v.tabs.find((t) => t.ref === v.active) ?? v.tabs[v.tabs.length - 1];
  const tailnet = useTailnetUrl(host, v.open ? active : undefined, active ? (reloads[active.ref] ?? 0) : 0);

  useEffect(() => {
    if (v.open && active && !seen.has(active.ref)) setSeen((s) => new Set(s).add(active.ref));
  }, [v.open, active, seen]);

  useEffect(() => {
    if (!v.open || desktop) return;
    const onKey = (e: KeyboardEvent) => e.key === 'Escape' && hide(chat);
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [v.open, desktop, chat, hide]);

  if (!v.open || !active) return null;

  return (
    <section
      aria-label="产物查看器"
      className={cx(
        'flex min-h-0 flex-col bg-bg',
        desktop ? 'h-full w-[min(52%,880px)] min-w-[380px] shrink-0 border-l border-line' : 'viewer-sheet app-band safe-top fixed inset-x-0 z-40',
      )}
    >
      <div className="shrink-0 border-b border-line">
        <div className="flex h-11 items-center gap-1 pr-1 pl-1 max-md:h-12">
          <IconButton label={desktop ? '收起查看器' : '返回对话'} onClick={() => hide(chat)}>
            {desktop ? <PanelRightClose size={17} /> : <ChevronDown size={20} />}
          </IconButton>
          <div className="flex min-w-0 flex-1 flex-col justify-center">
            <div className="truncate text-[14px] leading-5 font-semibold">{active.name}</div>
            <div className="truncate font-mono text-[11.5px] leading-4 text-faint" title={active.ref}>
              {middleTruncate(active.ref, desktop ? 72 : 40)}
            </div>
          </div>
          <IconButton label="重新加载" onClick={() => reload(active.ref)}>
            <RotateCw size={16} />
          </IconButton>
          <Menu
            width={272}
            label={active.name}
            trigger={(p) => (
              <IconButton label="更多" {...p}>
                <MoreHorizontal size={18} />
              </IconButton>
            )}
            items={actionsFor(host, active, tailnet)}
          />
        </div>
        {v.tabs.length > 1 && (
          <div role="tablist" aria-label="已打开的产物" className="scroll-none flex gap-1 overflow-x-auto px-2 pb-1.5">
            {v.tabs.map((t) => {
              const on = t.ref === active.ref;
              return (
                <div
                  key={t.ref}
                  className={cx(
                    'group flex h-8 max-w-[200px] shrink-0 items-center rounded-md border text-[12.5px] max-md:h-9',
                    on ? 'border-line-strong bg-active text-fg' : 'border-transparent text-muted hover:bg-hover',
                  )}
                >
                  <button type="button" role="tab" aria-selected={on} title={t.ref} onClick={() => show(chat, t.ref)} className="flex h-full min-w-0 items-center gap-1.5 pl-2">
                    <KindIcon kind={t.kind} size={13} className={cx('shrink-0', KIND_TONE[t.kind])} />
                    <span className="truncate">{t.name}</span>
                  </button>
                  <button
                    type="button"
                    title="关闭"
                    aria-label={`关闭 ${t.name}`}
                    onClick={() => closeTab(chat, t.ref)}
                    className="flex h-full w-7 shrink-0 items-center justify-center text-faint hover:text-fg"
                  >
                    <X size={12} />
                  </button>
                </div>
              );
            })}
          </div>
        )}
      </div>
      <div className="relative min-h-0 flex-1">
        {v.tabs
          .filter((t) => seen.has(t.ref) || t.ref === active.ref)
          .map((t) => (
            <div key={t.ref} className="absolute inset-0" hidden={t.ref !== active.ref}>
              <Body host={host} a={t} reload={reloads[t.ref] ?? 0} />
            </div>
          ))}
      </div>
    </section>
  );
}
