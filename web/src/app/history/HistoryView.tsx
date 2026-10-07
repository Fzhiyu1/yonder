import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { Activity, ArrowLeft, Eye, EyeOff, Folder, History, MessageSquare, Play, RefreshCw, Search, X } from 'lucide-react';
import { relativeTime } from '../../lib/format';
import { baseName } from '../../lib/paths';
import { getConn } from '../../net/provider';
import { call } from '../../net/types';
import type { AgentSessionSummary } from '../../proto/generated/AgentSessionSummary';
import type { ChatItem } from '../../proto/generated/ChatItem';
import type { HistoryFolder } from '../../proto/generated/HistoryFolder';
import type { SessionInfo } from '../../proto/generated/SessionInfo';
import type { SessionSpec } from '../../proto/generated/SessionSpec';
import { hostLabel, useHosts } from '../../store/hosts';
import { toastError, useUi } from '../../store/ui';
import { Markdown } from '../chat/Markdown';
import { navigate } from '../router';
import { hasApprovals, initialMode } from '../session/approval';
import { AGENT_LABEL, AgentIcon } from '../ui/icons';
import { Button, cx, EmptyState, IconButton, Segmented, Sheet, Spinner, useIsDesktop } from '../ui/primitives';

type AgentFilter = 'all' | 'codex' | 'claude' | 'pi';

const SOURCE_LABEL: Record<string, string> = { desktop: '桌面', ide: 'IDE', cli: '终端', yonder: 'yonder', sdk: 'SDK', exec: 'exec' };

const DAY = 86_400_000;

/** 今天 / 昨天 / 本周 / 近 30 天 / 更早, by the local calendar. */
export function timeBucket(ts: number | undefined, now = Date.now()): string {
  if (!ts) return '更早';
  const today = new Date(now);
  today.setHours(0, 0, 0, 0);
  const t0 = today.getTime();
  if (ts >= t0) return '今天';
  if (ts >= t0 - DAY) return '昨天';
  if (ts >= t0 - 6 * DAY) return '本周';
  if (ts >= t0 - 29 * DAY) return '近 30 天';
  return '更早';
}

interface Page {
  sessions: AgentSessionSummary[];
  next?: string;
  errors: string[];
}

function useDebounced<T>(value: T, ms: number): T {
  const [v, setV] = useState(value);
  useEffect(() => {
    const t = setTimeout(() => setV(value), ms);
    return () => clearTimeout(t);
  }, [value, ms]);
  return v;
}

const isLive = (s: SessionInfo) => s.state === 'running' || s.state === 'starting';

/** The yonder session that is this agent session (a running one first). */
export function liveFor(sessions: Record<string, SessionInfo> | undefined, h: AgentSessionSummary): SessionInfo | undefined {
  if (!sessions) return undefined;
  return Object.values(sessions)
    .filter((s) => s.agent === h.agent && s.agent_session === h.id)
    .sort((a, b) => Number(isLive(b)) - Number(isLive(a)) || b.updated_at - a.updated_at)[0];
}

export function HistoryView({ host }: { host: string }) {
  const h = useHosts((s) => s.hosts.find((x) => x.host === host));
  const rt = useHosts((s) => s.runtime[host]);
  const desktop = useIsDesktop();
  const online = rt?.status === 'online';
  const [agent, setAgent] = useState<AgentFilter>('all');
  const [query, setQuery] = useState('');
  const [cwd, setCwd] = useState<string>();
  const [all, setAll] = useState(false);
  const [page, setPage] = useState<Page>();
  const [folders, setFolders] = useState<HistoryFolder[]>([]);
  const [loading, setLoading] = useState(false);
  /** A new first page is on its way (filter changed); the shown one is stale. */
  const [refreshing, setRefreshing] = useState(false);
  const [error, setError] = useState<string>();
  const [open, setOpen] = useState<AgentSessionSummary>();
  const [reload, setReload] = useState(0);
  const q = useDebounced(query.trim(), 250);
  const gen = useRef(0);
  const sentinel = useRef<HTMLDivElement>(null);
  const [now] = useState(() => Date.now());

  const agents = rt?.info?.agents.filter((a) => a.available && a.chat && (a.agent === 'codex' || a.agent === 'claude' || a.agent === 'pi')) ?? [];

  const load = useCallback(
    async (cursor?: string) => {
      const conn = getConn(host);
      if (!conn) return;
      const my = cursor ? gen.current : ++gen.current;
      setLoading(true);
      setRefreshing(!cursor);
      if (!cursor) setError(undefined);
      try {
        const r = await call(
          conn,
          { op: 'agent_history', agent: agent === 'all' ? undefined : agent, cwd, query: q || undefined, cursor, limit: 40, all },
          'agent_history',
          { timeoutMs: 45_000 },
        );
        if (my !== gen.current) return;
        // Folder chips come from the unfiltered first page, so picking one keeps the others.
        if (!cursor && !cwd && !q) setFolders(r.folders ?? []);
        setPage((p) => ({ sessions: cursor && p ? [...p.sessions, ...r.sessions] : r.sessions, next: r.next_cursor, errors: r.errors ?? [] }));
      } catch (err) {
        if (my === gen.current) setError((err as Error).message);
      } finally {
        if (my === gen.current) {
          setLoading(false);
          setRefreshing(false);
        }
      }
    },
    [host, agent, cwd, q, all],
  );

  // Results of the previous filter stay (dimmed) until the new ones arrive.
  useEffect(() => {
    if (!online) return;
    void load();
  }, [online, load, reload]);


  // Folders differ per agent filter.
  useEffect(() => {
    setCwd(undefined);
    setFolders([]);
  }, [agent, all]);

  // Infinite scroll.
  useEffect(() => {
    const el = sentinel.current;
    if (!el || !page?.next) return;
    const io = new IntersectionObserver((e) => {
      if (e[0].isIntersecting && !loading) void load(page.next);
    });
    io.observe(el);
    return () => io.disconnect();
  }, [page?.next, loading, load]);

  const groups = useMemo(() => {
    const out: Array<{ label: string; items: AgentSessionSummary[] }> = [];
    for (const s of page?.sessions ?? []) {
      const label = timeBucket(s.updated_at ?? undefined, now);
      const g = out[out.length - 1];
      if (g && g.label === label) g.items.push(s);
      else out.push({ label, items: [s] });
    }
    return out;
  }, [page?.sessions, now]);

  if (!h) return null;
  const name = hostLabel(h, rt);
  const filtering = !!(q || cwd);

  return (
    <div className="flex h-full min-h-0 flex-col">
      <header className="safe-top shrink-0 border-b border-line">
        <div className="flex h-12 items-center gap-1 px-2 md:px-4">
          {!desktop && (
            <IconButton label="返回" onClick={() => navigate({ name: 'home' })}>
              <ArrowLeft size={18} />
            </IconButton>
          )}
          <h1 className="min-w-0 flex-1 truncate pl-1 text-[15px] font-semibold">历史会话 · {name}</h1>
          <IconButton label={all ? '隐藏临时与测试会话' : '显示临时与测试会话'} active={all} onClick={() => setAll((v) => !v)}>
            {all ? <Eye size={17} /> : <EyeOff size={17} />}
          </IconButton>
          <IconButton label="刷新" disabled={!online || loading} onClick={() => setReload((n) => n + 1)}>
            <RefreshCw size={16} className={refreshing ? 'animate-spin' : ''} />
          </IconButton>
        </div>
        <div className="flex flex-col gap-2 px-3 pb-2.5 md:px-4">
          <label className="relative block">
            <Search size={15} className="pointer-events-none absolute top-1/2 left-2.5 -translate-y-1/2 text-faint" />
            <input
              type="search"
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              placeholder="搜索标题、首条消息、文件夹"
              aria-label="搜索历史会话"
              enterKeyHint="search"
              onKeyDown={(e) => e.key === 'Enter' && (e.target as HTMLInputElement).blur()}
              className="h-9 w-full min-w-0 rounded-md border border-line-strong bg-bg pr-10 pl-8 text-sm text-fg placeholder:text-faint focus:border-accent focus:outline-none max-md:h-11 [&::-webkit-search-cancel-button]:hidden"
            />
            {query && (
              <button
                type="button"
                aria-label="清除搜索"
                title="清除搜索"
                onClick={() => setQuery('')}
                className="absolute top-1/2 right-0.5 flex h-8 w-8 -translate-y-1/2 items-center justify-center rounded text-faint hover:text-fg max-md:h-10 max-md:w-10"
              >
                <X size={15} />
              </button>
            )}
          </label>
          {agents.length > 1 && (
            <Segmented<AgentFilter>
              className="w-full md:w-auto md:self-start"
              value={agent}
              onChange={setAgent}
              options={[{ value: 'all', label: '全部' }, ...agents.map((a) => ({ value: a.agent as AgentFilter, label: AGENT_LABEL[a.agent] }))]}
            />
          )}
          {folders.length > 1 && (
            <div className="no-scrollbar -mx-3 flex gap-1.5 overflow-x-auto px-3 md:-mx-4 md:px-4" role="radiogroup" aria-label="文件夹">
              <FolderChip label="全部文件夹" active={!cwd} onClick={() => setCwd(undefined)} />
              {folders.map((f) => (
                <FolderChip key={f.path} label={baseName(f.path) || f.path} title={f.path} count={f.count} active={cwd === f.path} onClick={() => setCwd(cwd === f.path ? undefined : f.path)} />
              ))}
            </div>
          )}
        </div>
      </header>
      <div className="scroll-thin safe-bottom min-h-0 flex-1 overflow-y-auto overscroll-contain" data-testid="history-list">
        <div className={cx('mx-auto w-full max-w-3xl px-2 pb-4 transition-opacity md:px-4', refreshing && page && 'pointer-events-none opacity-50')} aria-busy={refreshing}>
          {!online && <EmptyState title="主机未连接" />}
          {online && error && !page && (
            <EmptyState title="读取历史失败">
              <span className="text-[13px] break-words text-danger">{error}</span>
              <Button className="mt-2" size="sm" icon={<RefreshCw size={14} />} onClick={() => setReload((n) => n + 1)}>
                重试
              </Button>
            </EmptyState>
          )}
          {online && !page && !error && (
            <div className="flex items-center justify-center gap-2 py-16 text-sm text-muted">
              <Spinner /> 正在读取 {name} 上的会话
            </div>
          )}
          {page && !page.sessions.length && (
            <EmptyState icon={<History size={22} />} title={filtering ? '没有匹配的会话' : '没有历史会话'}>
              {filtering && (
                <Button
                  size="sm"
                  onClick={() => {
                    setQuery('');
                    setCwd(undefined);
                  }}
                >
                  清除筛选
                </Button>
              )}
            </EmptyState>
          )}
          {page?.errors?.map((e) => (
            <div key={e} className="mx-2 mt-2 rounded-md border border-warn/30 bg-warn-soft px-3 py-2 text-[12.5px] break-words text-warn">
              {e}
            </div>
          ))}
          {groups.map((g) => (
            <section key={g.label} className="pt-2">
              <h2 className="sticky top-0 z-10 bg-bg/95 px-2 py-1.5 text-[12px] font-semibold text-muted backdrop-blur">{g.label}</h2>
              <div className="flex flex-col gap-px">
                {g.items.map((s) => (
                  <HistoryRow key={`${s.agent}/${s.id}`} s={s} now={now} live={liveFor(rt?.sessions, s)} showFolder={!cwd} onOpen={() => setOpen(s)} />
                ))}
              </div>
            </section>
          ))}
          {page?.next && (
            <div ref={sentinel} className="flex items-center justify-center gap-2 py-4 text-[13px] text-muted">
              {loading && <Spinner size={13} />}
              <button type="button" className="rounded px-3 py-1 hover:bg-hover max-md:py-3" onClick={() => void load(page.next)} disabled={loading}>
                加载更多
              </button>
            </div>
          )}
        </div>
      </div>
      <PreviewSheet host={host} s={open} live={open ? liveFor(rt?.sessions, open) : undefined} onClose={() => setOpen(undefined)} />
    </div>
  );
}

function FolderChip({ label, title, count, active, onClick }: { label: string; title?: string; count?: number; active: boolean; onClick: () => void }) {
  return (
    <button
      type="button"
      role="radio"
      aria-checked={active}
      title={title}
      onClick={onClick}
      className={cx(
        'inline-flex h-8 shrink-0 items-center gap-1.5 rounded-full border px-2.5 text-[12.5px] whitespace-nowrap transition-colors max-md:h-11 max-md:px-3.5',
        active ? 'border-accent bg-accent-soft text-accent' : 'border-line text-muted hover:bg-hover hover:text-fg',
      )}
    >
      {count !== undefined && <Folder size={12} className="shrink-0" />}
      <span className="max-w-[12rem] truncate">{label}</span>
      {count !== undefined && <span className="text-faint tabular-nums">{count}</span>}
    </button>
  );
}

function HistoryRow({ s, now, live, showFolder, onOpen }: { s: AgentSessionSummary; now: number; live?: SessionInfo; showFolder: boolean; onOpen: () => void }) {
  const running = live && isLive(live);
  const meta = [showFolder && s.cwd ? baseName(s.cwd) : '', s.source ? (SOURCE_LABEL[s.source] ?? s.source) : ''].filter(Boolean);
  return (
    <button type="button" onClick={onOpen} title={s.cwd ?? undefined} className="flex w-full min-w-0 items-start gap-2.5 rounded-md px-2 py-2 text-left hover:bg-hover max-md:py-2.5">
      <span className="mt-0.5 flex h-4 w-4 shrink-0 items-center justify-center text-muted">
        <AgentIcon agent={s.agent} kind="chat" size={15} />
      </span>
      <span className="min-w-0 flex-1">
        <span className="flex min-w-0 items-center gap-2">
          <span className="min-w-0 flex-1 truncate text-[14px] font-medium">{s.title}</span>
          {running ? (
            <span className="shrink-0 rounded-full border border-accent/40 bg-accent-soft px-2 text-[11.5px] leading-5 text-accent" title="已在 yonder 中打开">已打开</span>
          ) : s.active ? (
            <span className="shrink-0 rounded-full border border-warn/40 px-2 text-[11.5px] leading-5 text-warn" title="最近一分钟有更新，可能正在别处运行">
              运行中
            </span>
          ) : (
            s.updated_at && <span className="shrink-0 text-[12px] text-faint tabular-nums">{relativeTime(s.updated_at, now)}</span>
          )}
        </span>
        {s.preview && <span className="block truncate text-[12.5px] text-muted">{s.preview}</span>}
        {meta.length > 0 && <span className="block truncate text-[12px] text-faint">{meta.join(' · ')}</span>}
      </span>
    </button>
  );
}

function PreviewSheet({ host, s, live, onClose }: { host: string; s?: AgentSessionSummary; live?: SessionInfo; onClose: () => void }) {
  const [items, setItems] = useState<ChatItem[]>();
  const [truncated, setTruncated] = useState(false);
  const [error, setError] = useState<string>();
  const [busy, setBusy] = useState(false);
  const rt = useHosts((st) => st.runtime[host]);
  const remembered = useUi((st) => st.settings.approvalByAgent);
  const end = useRef<HTMLDivElement>(null);

  useEffect(() => {
    setItems(undefined);
    setError(undefined);
    if (!s) return;
    const conn = getConn(host);
    if (!conn) return;
    let cancelled = false;
    call(conn, { op: 'agent_preview', agent: s.agent, id: s.id }, 'agent_preview')
      .then((r) => {
        if (cancelled) return;
        setItems(r.items);
        setTruncated(r.truncated);
      })
      .catch((err) => !cancelled && setError((err as Error).message));
    return () => {
      cancelled = true;
    };
  }, [host, s]);

  // The newest messages matter most: open at the end.
  useEffect(() => {
    if (items?.length) end.current?.scrollIntoView({ block: 'end' });
  }, [items]);

  const running = !!live && isLive(live);

  const resume = async () => {
    if (!s) return;
    if (running && live) {
      onClose();
      navigate({ name: 'session', host, session: live.id });
      return;
    }
    const conn = getConn(host);
    if (!conn) return;
    setBusy(true);
    const info = rt?.info?.agents.find((a) => a.agent === s.agent);
    const spec: SessionSpec = { kind: 'chat', agent: s.agent, cwd: s.cwd ?? undefined, resume: s.id, title: s.title };
    // Same rule as a new chat: this device's last pick for the agent, else the host's config.
    if (hasApprovals(s.agent) && (remembered?.[s.agent] ?? info?.default_approval)) spec.approval = initialMode(s.agent, info, remembered ?? {});
    try {
      const res = await call(conn, { op: 'create_session', spec }, 'session');
      useHosts.getState().upsertSession(host, res.session);
      onClose();
      navigate({ name: 'session', host, session: res.session.id });
    } catch (err) {
      toastError('继续会话失败', err);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Sheet
      open={!!s}
      onClose={onClose}
      width={680}
      title={
        s && (
          <span className="flex min-w-0 items-center gap-2">
            <AgentIcon agent={s.agent} kind="chat" size={15} className="shrink-0 text-muted" />
            <span className="truncate">{s.title}</span>
          </span>
        )
      }
      footer={
        s && (
          <>
            <span className="mr-auto min-w-0 truncate text-[12px] text-faint max-md:basis-full" title={s.cwd ?? undefined}>
              {[AGENT_LABEL[s.agent], s.model, s.cwd].filter(Boolean).join(' · ')}
            </span>
            <Button variant="primary" busy={busy} icon={running ? <MessageSquare size={14} /> : <Play size={14} />} onClick={resume} className="max-md:flex-1">
              {running ? '打开会话' : s.active ? '在副本中继续' : '继续对话'}
            </Button>
          </>
        )
      }
    >
      <div className="flex flex-col gap-3" data-testid="history-preview">
        {!items && !error && (
          <div className="flex items-center justify-center gap-2 py-10 text-sm text-muted">
            <Spinner /> 正在读取
          </div>
        )}
        {!running && s?.active && (
          <div className="flex items-start gap-2 rounded-md border border-warn/40 px-3 py-2 text-[13px] text-warn">
            <Activity size={15} className="mt-0.5 shrink-0" />
            <span>这个会话可能正在桌面端或终端运行。在这里继续会建一个副本，副本里的消息不会出现在原会话中。</span>
          </div>
        )}
        {error && <div className="py-6 text-center text-[13px] break-words text-danger">{error}</div>}
        {items && truncated && <div className="text-center text-[12px] text-faint">只显示最近的消息</div>}
        {items && !items.length && <div className="py-6 text-center text-[13px] text-faint">没有可显示的消息</div>}
        {items?.map((it) =>
          it.kind === 'user' ? (
            <div key={it.id} className="flex justify-end">
              <div className="max-w-[85%] rounded-lg bg-active px-3.5 py-2 text-[14px] break-words whitespace-pre-wrap">{it.text}</div>
            </div>
          ) : (
            <Markdown key={it.id} text={it.text ?? ''} className="text-[14px]" />
          ),
        )}
        <div ref={end} />
      </div>
    </Sheet>
  );
}
