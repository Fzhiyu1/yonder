import { useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react';
import {
  ArrowLeft,
  ArrowUp,
  Copy,
  Download,
  Eye,
  EyeOff,
  File as FileIcon,
  FileCode,
  FileImage,
  FileText,
  Folder,
  FolderPlus,
  Link2,
  MoreHorizontal,
  Pencil,
  Plus,
  RefreshCw,
  Trash2,
  Upload,
  X,
} from 'lucide-react';
import { formatDateTime, formatSize, relativeTime } from '../../lib/format';
import { breadcrumbs, joinPath, parentPath } from '../../lib/paths';
import { getConn } from '../../net/provider';
import { call, RequestError } from '../../net/types';
import type { DirListing } from '../../proto/generated/DirListing';
import type { FileEntry } from '../../proto/generated/FileEntry';
import { hostLabel, useHosts } from '../../store/hosts';
import { toastError, useUi } from '../../store/ui';
import { copyText } from '../../lib/clipboard';
import { navigate, parseHash } from '../router';
import { Button, cx, EmptyState, IconButton, inputClass, Menu, Sheet, Spinner, useIsDesktop, useIsMobile } from '../ui/primitives';
import { downloadFile, isImage, isTextName, looksBinary, mimeFor, saveBlob, uploadFile } from './transfer';

interface Transfer {
  id: string;
  name: string;
  kind: 'up' | 'down';
  done: number;
  total: number;
  abort: AbortController;
}

function EntryIcon({ e }: { e: FileEntry }) {
  if (e.kind === 'dir') return <Folder size={16} className="text-accent" />;
  if (isImage(e.name)) return <FileImage size={16} className="text-muted" />;
  if (/\.(md|txt|log|pdf|docx?)$/i.test(e.name)) return <FileText size={16} className="text-muted" />;
  if (isTextName(e.name)) return <FileCode size={16} className="text-muted" />;
  return <FileIcon size={16} className="text-muted" />;
}

function Preview({ host, entry, onClose, onDownload }: { host: string; entry: FileEntry | null; onClose: () => void; onDownload: (e: FileEntry) => void }) {
  const [state, setState] = useState<{ url?: string; text?: string; error?: string; loading: boolean }>({ loading: true });
  useEffect(() => {
    if (!entry) return;
    let url: string | undefined;
    let cancelled = false;
    setState({ loading: true });
    const conn = getConn(host);
    if (!conn) return;
    downloadFile(conn, entry.path, () => undefined)
      .then((parts) => {
        if (cancelled) return;
        const blob = new Blob(parts, { type: mimeFor(entry.name) });
        if (isImage(entry.name)) {
          url = URL.createObjectURL(blob);
          setState({ url, loading: false });
        } else {
          const all = new Uint8Array(blob.size);
          let o = 0;
          for (const p of parts) {
            all.set(p, o);
            o += p.length;
          }
          if (looksBinary(all)) setState({ error: '二进制文件，无法预览', loading: false });
          else setState({ text: new TextDecoder().decode(all), loading: false });
        }
      })
      .catch((err) => !cancelled && setState({ error: (err as Error).message, loading: false }));
    return () => {
      cancelled = true;
      if (url) URL.revokeObjectURL(url);
    };
  }, [host, entry]);

  return (
    <Sheet
      open={!!entry}
      onClose={onClose}
      title={entry?.name ?? ''}
      width={900}
      bodyClassName="p-0"
      footer={
        entry && (
          <>
            <span className="mr-auto text-[12.5px] text-muted">{formatSize(entry.size)}</span>
            <Button icon={<Download size={14} />} onClick={() => onDownload(entry)}>
              下载
            </Button>
          </>
        )
      }
    >
      {state.loading && (
        <div className="flex items-center justify-center gap-2 p-10 text-sm text-muted">
          <Spinner /> 正在读取
        </div>
      )}
      {state.error && <div className="p-6 text-center text-sm text-muted">{state.error}</div>}
      {state.url && (
        <div className="flex items-center justify-center bg-panel p-4">
          <img src={state.url} alt={entry?.name} className="max-h-[70dvh] max-w-full object-contain" />
        </div>
      )}
      {state.text !== undefined && <pre className="scroll-thin overflow-auto p-4 font-mono text-[12.5px] leading-[1.55] whitespace-pre-wrap break-all">{state.text}</pre>}
    </Sheet>
  );
}

export function FileManager({ host, path }: { host: string; path?: string }) {
  const stored = useHosts((s) => s.hosts.find((h) => h.host === host));
  const rt = useHosts((s) => s.runtime[host]);
  const desktop = useIsDesktop();
  const mobile = useIsMobile();
  const sep = rt?.info?.path_sep ?? '/';
  const [listing, setListing] = useState<DirListing>();
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string>();
  const [hidden, setHidden] = useState(false);
  const [transfers, setTransfers] = useState<Transfer[]>([]);
  const [preview, setPreview] = useState<FileEntry | null>(null);
  const [prompt, setPrompt] = useState<{ title: string; value: string; action: (v: string) => Promise<void> } | null>(null);
  const [promptBusy, setPromptBusy] = useState(false);
  const fileInput = useRef<HTMLInputElement>(null);
  const crumbBar = useRef<HTMLDivElement>(null);
  // Only the newest listing request may update the view: a slow home listing must not
  // replace a folder the user already moved to (uploads would land in the wrong place).
  const loadSeq = useRef(0);
  // App keys this view by host, so a host switch remounts it. Requests still in flight when
  // it unmounts must neither update it nor navigate back to it.
  const alive = useRef(true);
  // Latest route path, and the path requested for the listing on screen (undefined: home).
  const pathRef = useRef(path);
  pathRef.current = path;
  const listingReq = useRef<string | undefined>(undefined);
  const online = rt?.status === 'online';

  const load = useCallback(
    async (p?: string) => {
      const conn = getConn(host);
      if (!conn || conn.status !== 'online' || !alive.current) return;
      const seq = ++loadSeq.current;
      setLoading(true);
      setError(undefined);
      try {
        const target = p ?? (await call(conn, { op: 'fs_home' }, 'path')).path;
        if (seq !== loadSeq.current) return;
        const res = await call(conn, { op: 'fs_list', path: target, hidden }, 'dir');
        if (seq !== loadSeq.current) return;
        listingReq.current = p;
        setListing(res.listing);
        // Put the resolved (home or canonical) path in the route.
        if (res.listing.path !== pathRef.current) navigate({ name: 'files', host, path: res.listing.path }, true);
      } catch (err) {
        if (seq === loadSeq.current) setError((err as Error).message);
      } finally {
        if (seq === loadSeq.current) setLoading(false);
      }
    },
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [host, hidden],
  );

  // Declared before the loading effect so a StrictMode remount re-enables loads first.
  useEffect(() => {
    alive.current = true;
    return () => {
      alive.current = false;
      loadSeq.current++;
    };
  }, []);

  useEffect(() => {
    if (online && (!listing || listing.path !== path)) void load(path);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [online, path, load]);

  // The listing of the folder in the route. Right after moving to another folder the previous
  // listing is still on screen until the new one arrives; it is shown dimmed, and nothing
  // (upload, new folder, new session here) may target it.
  const current = listing && (listing.path === path || (path === undefined && listingReq.current === undefined)) ? listing : undefined;
  const cwd = current?.path;
  const cwdRef = useRef(cwd);
  cwdRef.current = cwd;
  const shown = cwd ?? path ?? listing?.path;
  const up = current ? (current.parent ?? null) : null;
  const go = (p: string) => navigate({ name: 'files', host, path: p });

  const track = (t: Transfer) => setTransfers((x) => [...x, t]);
  const progress = (id: string, done: number, total?: number) => setTransfers((x) => x.map((t) => (t.id === id ? { ...t, done, total: total ?? t.total } : t)));
  const untrack = (id: string) => setTransfers((x) => x.filter((t) => t.id !== id));

  const upload = async (files: FileList | File[]) => {
    const conn = getConn(host);
    // The folder the user is in right now. Read the route itself: a tap on a folder changes it
    // before React re-renders, so the props (and a listing still loading) can be one folder
    // behind, and the previous folder must never be the target.
    const live = parseHash(location.hash);
    const dir = (live.name === 'files' && live.host === host ? live.path : undefined) ?? cwdRef.current;
    if (!conn) return;
    if (!dir) {
      useUi.getState().toast('warning', '文件夹还在读取，请稍后再上传');
      return;
    }
    for (const f of Array.from(files)) {
      const target = joinPath(dir, f.name, sep);
      const id = `${Date.now()}-${Math.random()}`;
      const abort = new AbortController();
      track({ id, name: f.name, kind: 'up', done: 0, total: f.size, abort });
      try {
        try {
          await uploadFile(conn, target, f, false, (n) => progress(id, n), abort.signal);
        } catch (err) {
          if (!(err instanceof RequestError && err.code === 'exists')) throw err;
          const ok = await useUi.getState().ask({ title: '文件已存在', message: `「${f.name}」已存在，是否覆盖？`, confirmLabel: '覆盖', destructive: true });
          if (!ok) continue;
          progress(id, 0);
          await uploadFile(conn, target, f, true, (n) => progress(id, n), abort.signal);
        }
        useUi.getState().toast('success', `已上传 ${f.name}`);
      } catch (err) {
        if (!(err instanceof RequestError && err.code === 'aborted')) toastError(`上传 ${f.name} 失败`, err);
      } finally {
        untrack(id);
      }
    }
    if (pathRef.current === dir) void load(dir);
  };

  const download = async (e: FileEntry) => {
    const conn = getConn(host);
    if (!conn) return;
    const id = `${Date.now()}-${Math.random()}`;
    const abort = new AbortController();
    track({ id, name: e.name, kind: 'down', done: 0, total: e.size, abort });
    try {
      const parts = await downloadFile(conn, e.path, (n, total) => progress(id, n, total), abort.signal);
      await saveBlob(new Blob(parts, { type: mimeFor(e.name) }), e.name);
    } catch (err) {
      if (!(err instanceof RequestError && err.code === 'aborted')) toastError(`下载 ${e.name} 失败`, err);
    } finally {
      untrack(id);
    }
  };

  const run = async (label: string, fn: () => Promise<unknown>) => {
    try {
      await fn();
      if (cwdRef.current) await load(cwdRef.current);
    } catch (err) {
      toastError(label, err);
      throw err;
    }
  };

  const mkdir = () =>
    setPrompt({
      title: '新建文件夹',
      value: '',
      action: (name) => run('新建文件夹失败', () => getConn(host)!.request({ op: 'fs_mkdir', path: joinPath(cwd!, name, sep) })),
    });

  const rename = (e: FileEntry) =>
    setPrompt({
      title: '重命名',
      value: e.name,
      action: async (name) => {
        if (name === e.name) return;
        const to = joinPath(parentPath(e.path, sep) ?? cwd!, name, sep);
        await run('重命名失败', async () => {
          try {
            await getConn(host)!.request({ op: 'fs_rename', from: e.path, to, overwrite: false });
          } catch (err) {
            if (!(err instanceof RequestError && err.code === 'exists')) throw err;
            const ok = await useUi.getState().ask({ title: '目标已存在', message: `「${name}」已存在，是否覆盖？`, confirmLabel: '覆盖', destructive: true });
            if (ok) await getConn(host)!.request({ op: 'fs_rename', from: e.path, to, overwrite: true });
          }
        });
      },
    });

  const remove = async (e: FileEntry) => {
    const ok = await useUi.getState().ask({
      title: e.kind === 'dir' ? '删除文件夹' : '删除文件',
      message: `「${e.name}」将移到主机的废纸篓。`,
      confirmLabel: '删除',
      destructive: true,
    });
    if (ok) await run('删除失败', () => getConn(host)!.request({ op: 'fs_delete', path: e.path })).catch(() => undefined);
  };

  const open = (e: FileEntry) => {
    if (e.kind === 'dir') go(e.path);
    else if ((isImage(e.name) || isTextName(e.name)) && e.size < 1024 * 1024) setPreview(e);
    else void download(e);
  };

  const name = stored ? hostLabel(stored, rt) : '';
  const crumbs = shown ? breadcrumbs(shown, sep) : [];
  const stale = !!listing && !current;
  // Deep paths: keep the current folder (the end of the trail) in view.
  useLayoutEffect(() => {
    const el = crumbBar.current;
    if (el) el.scrollLeft = el.scrollWidth;
  }, [shown]);

  return (
    <div
      className="flex h-full min-h-0 flex-col"
      onDragOver={(e) => e.preventDefault()}
      onDrop={(e) => {
        e.preventDefault();
        if (e.dataTransfer.files.length) void upload(e.dataTransfer.files);
      }}
    >
      <header className="safe-top shrink-0 border-b border-line">
        <div className="flex h-12 items-center gap-1 px-2 md:px-4">
          {!desktop && (
            <IconButton label="返回" onClick={() => navigate({ name: 'home' })}>
              <ArrowLeft size={18} />
            </IconButton>
          )}
          <div className="min-w-0 flex-1 truncate pl-1 text-[14px] font-semibold">
            文件 <span className="font-normal text-muted">· {name}</span>
          </div>
          <IconButton label="上一级" disabled={!up || loading} onClick={() => up && go(up)}>
            <ArrowUp size={17} />
          </IconButton>
          <IconButton label="刷新" disabled={!online || loading} onClick={() => void load(cwd ?? path)}>
            <RefreshCw size={16} className={loading ? 'animate-spin' : ''} />
          </IconButton>
          <IconButton label="新建文件夹" disabled={!cwd} onClick={mkdir}>
            <FolderPlus size={17} />
          </IconButton>
          <IconButton label="上传" disabled={!cwd} onClick={() => fileInput.current?.click()}>
            <Upload size={17} />
          </IconButton>
          <IconButton label={hidden ? '隐藏隐藏文件' : '显示隐藏文件'} active={hidden} onClick={() => setHidden((h) => !h)}>
            {hidden ? <Eye size={17} /> : <EyeOff size={17} />}
          </IconButton>
          <input
            ref={fileInput}
            type="file"
            multiple
            hidden
            onChange={(e) => {
              if (e.target.files) void upload(e.target.files);
              e.target.value = '';
            }}
          />
        </div>
        <div ref={crumbBar} className="scroll-thin flex h-9 items-center gap-0.5 overflow-x-auto px-3 text-[13px] whitespace-nowrap md:px-4 max-md:h-12 max-md:text-[14px]">
          {crumbs.map((c, i) => (
            <span key={c.path} className="flex items-center">
              <button
                type="button"
                onClick={() => go(c.path)}
                className={cx('rounded px-1.5 py-1 hover:bg-hover max-md:inline-flex max-md:h-11 max-md:min-w-10 max-md:items-center max-md:justify-center max-md:py-0', i === crumbs.length - 1 ? 'font-medium text-fg' : 'text-muted')}
              >
                {c.name}
              </button>
              {i < crumbs.length - 1 && <span className="text-faint">{c.name === '/' ? '' : sep === '/' ? '/' : '\\'}</span>}
            </span>
          ))}
        </div>
      </header>

      {transfers.length > 0 && (
        <div className="shrink-0 border-b border-line bg-panel px-3 py-2 md:px-4">
          {transfers.map((t) => (
            <div key={t.id} className="flex items-center gap-2 py-0.5 text-[12.5px]">
              {t.kind === 'up' ? <Upload size={13} className="shrink-0 text-muted" /> : <Download size={13} className="shrink-0 text-muted" />}
              <span className="w-32 min-w-0 truncate md:w-56">{t.name}</span>
              <div className="h-1.5 min-w-0 flex-1 overflow-hidden rounded-full bg-active">
                <div className="h-full rounded-full bg-accent transition-[width]" style={{ width: `${t.total ? Math.min(100, (t.done / t.total) * 100) : 0}%` }} />
              </div>
              <span className="w-24 shrink-0 text-right text-muted tabular-nums">
                {formatSize(t.done)} / {formatSize(t.total)}
              </span>
              <IconButton label="取消" size="sm" onClick={() => t.abort.abort()}>
                <X size={13} />
              </IconButton>
            </div>
          ))}
        </div>
      )}

      <div className="scroll-thin min-h-0 flex-1 overflow-y-auto">
        {!online && <EmptyState title="主机不在线" />}
        {online && error && (
          <EmptyState title="无法打开文件夹">
            <span className="text-[13px] break-all">{error}</span>
            <Button size="sm" onClick={() => void load(undefined)}>
              回到主目录
            </Button>
          </EmptyState>
        )}
        {online && !error && !listing && (
          <div className="flex items-center justify-center gap-2 p-10 text-sm text-muted">
            <Spinner /> 正在读取
          </div>
        )}
        {listing && !error && (
          <table className={cx('w-full table-fixed border-collapse text-[13.5px]', stale && 'pointer-events-none opacity-50')} aria-busy={stale || undefined}>
            <thead className="sticky top-0 z-10 bg-bg text-left text-[12px] text-muted max-md:hidden">
              <tr className="border-b border-line">
                <th className="py-1.5 pl-4 font-medium">名称</th>
                <th className="w-24 py-1.5 pr-3 text-right font-medium">大小</th>
                <th className="w-40 py-1.5 pr-3 font-medium">修改时间</th>
                <th className="w-12" />
              </tr>
            </thead>
            <tbody>
              {listing.entries.map((e) => (
                <tr key={e.path} className={cx('group border-b border-line/60 hover:bg-hover', e.hidden && 'opacity-60')}>
                  <td className="p-0">
                    <button type="button" onClick={() => open(e)} className="flex h-9 w-full min-w-0 items-center gap-2.5 pl-4 text-left max-md:h-auto max-md:min-h-12 max-md:py-1.5">
                      <span className="shrink-0">
                        <EntryIcon e={e} />
                      </span>
                      <span className="min-w-0 flex-1">
                        <span className="block truncate">{e.name}</span>
                        {mobile && (
                          <span className="block truncate text-[12px] text-muted">
                            {e.kind === 'dir' ? '文件夹' : formatSize(e.size)}
                            {e.mtime ? ` · ${relativeTime(e.mtime)}` : ''}
                          </span>
                        )}
                      </span>
                    </button>
                  </td>
                  <td className="py-0 pr-3 text-right text-muted tabular-nums max-md:hidden">{e.kind === 'dir' ? '—' : formatSize(e.size)}</td>
                  <td className="truncate py-0 pr-3 text-muted tabular-nums max-md:hidden">{formatDateTime(e.mtime)}</td>
                  <td className="w-12 py-0 pr-2 text-right max-md:w-12">
                    <Menu
                      trigger={(p) => (
                        <IconButton label="更多" size="sm" className="opacity-60 group-hover:opacity-100 max-md:opacity-100" {...p}>
                          <MoreHorizontal size={16} />
                        </IconButton>
                      )}
                      items={[
                        { label: '预览', icon: <Eye size={15} />, onSelect: () => setPreview(e), hidden: e.kind === 'dir' || e.size >= 1024 * 1024 || !(isImage(e.name) || isTextName(e.name)) },
                        { label: '下载', icon: <Download size={15} />, onSelect: () => void download(e), hidden: e.kind === 'dir' },
                        { label: '重命名', icon: <Pencil size={15} />, onSelect: () => rename(e) },
                        {
                          label: '复制路径',
                          icon: <Link2 size={15} />,
                          onSelect: async () => {
                            if (await copyText(e.path)) useUi.getState().toast('success', '已复制路径');
                          },
                        },
                        { label: '在此处新建会话', icon: <Plus size={15} />, onSelect: () => useUi.getState().openNewSession({ host, cwd: e.kind === 'dir' ? e.path : (parentPath(e.path, sep) ?? cwd) }) },
                        { label: '删除', icon: <Trash2 size={15} />, onSelect: () => void remove(e), danger: true },
                      ]}
                    />
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
        {current && !error && !current.entries.length && <EmptyState title="空文件夹" />}
        {current?.truncated && <div className="p-3 text-center text-[12px] text-faint">条目过多，仅显示部分</div>}
        {current && (
          <div className="flex justify-center gap-2 p-4">
            <Button size="sm" variant="ghost" icon={<Plus size={14} />} onClick={() => useUi.getState().openNewSession({ host, cwd })}>
              在此处新建会话
            </Button>
            <Button
              size="sm"
              variant="ghost"
              icon={<Copy size={14} />}
              onClick={async () => {
                if (cwd && (await copyText(cwd))) useUi.getState().toast('success', '已复制路径');
              }}
            >
              复制当前路径
            </Button>
          </div>
        )}
      </div>

      <Preview host={host} entry={preview} onClose={() => setPreview(null)} onDownload={(e) => void download(e)} />
      <Sheet
        open={!!prompt}
        onClose={() => setPrompt(null)}
        title={prompt?.title ?? ''}
        width={420}
        footer={
          <>
            <Button onClick={() => setPrompt(null)}>取消</Button>
            <Button
              variant="primary"
              busy={promptBusy}
              disabled={!prompt?.value.trim()}
              onClick={async () => {
                if (!prompt) return;
                setPromptBusy(true);
                try {
                  await prompt.action(prompt.value.trim());
                  setPrompt(null);
                } catch {
                  /* toast shown */
                } finally {
                  setPromptBusy(false);
                }
              }}
            >
              确定
            </Button>
          </>
        }
      >
        <input
          autoFocus
          className={inputClass}
          value={prompt?.value ?? ''}
          onChange={(e) => prompt && setPrompt({ ...prompt, value: e.target.value })}
          onKeyDown={(e) => {
            if (e.key === 'Enter' && prompt?.value.trim()) {
              e.preventDefault();
              (e.currentTarget.closest('[role=dialog]')?.querySelector('button.bg-accent') as HTMLButtonElement | null)?.click();
            }
          }}
          aria-label="名称"
        />
      </Sheet>
    </div>
  );
}
