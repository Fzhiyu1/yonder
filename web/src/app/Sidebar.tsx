import { useEffect, useState } from 'react';
import { ChevronDown, ChevronRight, FolderOpen, History, Info, Plus, RefreshCw, Settings } from 'lucide-react';
import { relativeTime } from '../lib/format';
import type { SessionInfo } from '../proto/generated/SessionInfo';
import { hostLabel, sortedSessions, useHosts, type HostRuntime } from '../store/hosts';
import { useUi } from '../store/ui';
import type { StoredHost } from '../storage';
import { describeError } from '../net/types';
import { PairEntry } from './pairing/Pairing';
import { navigate, useRoute, type Route } from './router';
import { AgentIcon, OsIcon } from './ui/icons';
import { cx, IconButton, Spinner, StatusDot } from './ui/primitives';

function useNow(ms = 30_000) {
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), ms);
    return () => clearInterval(t);
  }, [ms]);
  return now;
}

function SessionRow({ host, s, active, now }: { host: string; s: SessionInfo; active: boolean; now: number }) {
  const exited = s.state === 'exited' || s.state === 'failed';
  const working = s.chat_status === 'working' || s.chat_status === 'starting';
  const waiting = s.pending_approvals > 0 && !exited;
  return (
    <button
      type="button"
      onClick={() => navigate({ name: 'session', host, session: s.id })}
      className={cx(
        'group flex w-full min-w-0 items-start gap-2.5 rounded-md px-2 py-1.5 text-left transition-colors max-md:py-2.5',
        active ? 'bg-active' : 'hover:bg-hover',
        exited && 'opacity-60',
      )}
    >
      <span className="mt-0.5 flex h-4 w-4 shrink-0 items-center justify-center text-muted">
        {working ? <Spinner size={14} className="text-accent" /> : <AgentIcon agent={s.agent} kind={s.kind} size={15} />}
      </span>
      <span className="min-w-0 flex-1">
        <span className="flex min-w-0 items-center gap-2">
          <span className="min-w-0 flex-1 truncate text-[13.5px] font-medium">{s.title || s.command.join(' ')}</span>
          {waiting ? (
            <span title={`${s.pending_approvals} 个待审批`} className="shrink-0 rounded-full bg-warn px-2 text-[12px] leading-5 font-semibold text-white dark:text-black">
              {s.pending_approvals} 待审批
            </span>
          ) : (
            <span className="shrink-0 text-[12px] text-faint tabular-nums">{relativeTime(s.updated_at, now)}</span>
          )}
        </span>
        {s.preview && <span className="block truncate text-[12.5px] text-muted">{s.preview}</span>}
      </span>
    </button>
  );
}

function HostGroup({ h, rt, route, now }: { h: StoredHost; rt?: HostRuntime; route: Route; now: number }) {
  const [open, setOpen] = useState(true);
  const [showAll, setShowAll] = useState(false);
  const status = rt?.status ?? 'connecting';
  const sessions = sortedSessions(rt);
  const shown = showAll ? sessions : sessions.slice(0, 12);
  const name = hostLabel(h, rt);
  const os = rt?.info?.os ?? rt?.hello?.os ?? h.os;
  const filesActive = route.name === 'files' && route.host === h.host;
  const infoActive = route.name === 'host' && route.host === h.host;
  const historyActive = route.name === 'history' && route.host === h.host;
  return (
    <section className="mb-2">
      <div className="group flex h-8 items-center gap-1.5 rounded-md pr-1 pl-1 max-md:h-11">
        <button
          type="button"
          onClick={() => setOpen((o) => !o)}
          className="flex h-full min-w-0 flex-1 items-center gap-1.5 rounded-md py-1 text-left"
          aria-expanded={open}
          title={open ? '折叠' : '展开'}
        >
          <span className="text-faint">{open ? <ChevronDown size={14} /> : <ChevronRight size={14} />}</span>
          <StatusDot status={status} />
          <span className="min-w-0 truncate text-[13px] font-semibold">{name}</span>
          <OsIcon os={os} size={12} className="shrink-0 text-faint" />
        </button>
        <IconButton label={`在 ${name} 上新建会话`} size="sm" className="opacity-0 group-hover:opacity-100 max-md:opacity-100" onClick={() => useUi.getState().openNewSession({ host: h.host })}>
          <Plus size={15} />
        </IconButton>
      </div>
      {open && (
        <div className="flex flex-col gap-px pl-1">
          {status === 'error' && <div className="px-2 py-1 text-[12.5px] text-danger">{describeError(rt?.error)}</div>}
          {status === 'offline' && !sessions.length && <div className="px-2 py-1 text-[12.5px] text-faint">主机离线</div>}
          {status === 'connecting' && !rt?.sessionsLoaded && (
            <div className="flex items-center gap-2 px-2 py-1 text-[12.5px] text-faint">
              <Spinner size={12} /> 连接中
            </div>
          )}
          {status === 'online' && rt?.sessionsLoaded && !sessions.length && <div className="px-2 py-1 text-[12.5px] text-faint">暂无会话</div>}
          {shown.map((s) => (
            <SessionRow key={s.id} host={h.host} s={s} now={now} active={route.name === 'session' && route.host === h.host && route.session === s.id} />
          ))}
          {sessions.length > shown.length && (
            <button type="button" onClick={() => setShowAll(true)} className="rounded-md px-2 py-1 text-left text-[12.5px] text-muted hover:bg-hover max-md:py-2.5">
              显示全部 {sessions.length} 个
            </button>
          )}
          <div className="mt-0.5 flex gap-px">
            <button
              type="button"
              onClick={() => navigate({ name: 'history', host: h.host })}
              className={cx('flex h-8 min-w-0 flex-1 items-center gap-2 rounded-md px-2 text-[13px] text-muted hover:bg-hover hover:text-fg max-md:h-11', historyActive && 'bg-active text-fg')}
            >
              <History size={15} className="shrink-0" /> <span className="truncate">历史</span>
            </button>
            <button
              type="button"
              disabled={status !== 'online'}
              onClick={() => navigate({ name: 'files', host: h.host })}
              className={cx('flex h-8 min-w-0 flex-1 items-center gap-2 rounded-md px-2 text-[13px] text-muted hover:bg-hover hover:text-fg disabled:opacity-40 max-md:h-11', filesActive && 'bg-active text-fg')}
            >
              <FolderOpen size={15} className="shrink-0" /> <span className="truncate">文件</span>
            </button>
            <button
              type="button"
              onClick={() => navigate({ name: 'host', host: h.host })}
              className={cx('flex h-8 min-w-0 flex-1 items-center gap-2 rounded-md px-2 text-[13px] text-muted hover:bg-hover hover:text-fg max-md:h-11', infoActive && 'bg-active text-fg')}
            >
              <Info size={15} className="shrink-0" /> <span className="truncate">主机信息</span>
            </button>
          </div>
        </div>
      )}
    </section>
  );
}

export function Sidebar() {
  const hosts = useHosts((s) => s.hosts);
  const runtime = useHosts((s) => s.runtime);
  const loaded = useHosts((s) => s.loaded);
  const refresh = useHosts((s) => s.refresh);
  const route = useRoute();
  const now = useNow();
  const [spinning, setSpinning] = useState(false);

  return (
    <aside className="safe-top flex h-full min-h-0 w-full flex-col bg-panel">
      <div className="flex h-12 shrink-0 items-center gap-1 pr-2 pl-4">
        <span className="flex-1 text-[15px] font-semibold">yonder</span>
        <IconButton label="新建会话" disabled={!hosts.length} onClick={() => useUi.getState().openNewSession({})}>
          <Plus size={18} />
        </IconButton>
        <IconButton
          label="刷新"
          disabled={!hosts.length}
          onClick={async () => {
            setSpinning(true);
            await refresh().finally(() => setTimeout(() => setSpinning(false), 300));
          }}
        >
          <RefreshCw size={16} className={spinning ? 'animate-spin' : ''} />
        </IconButton>
      </div>
      <nav className="scroll-thin min-h-0 flex-1 overflow-y-auto px-2 pb-2">
        {loaded && !hosts.length && (
          <div className="flex flex-col gap-3 px-2 pt-4">
            <div className="text-[13px] font-semibold">配对主机</div>
            <PairEntry compact onPayload={(t) => useUi.getState().openPair(t)} />
          </div>
        )}
        {hosts.map((h) => (
          <HostGroup key={h.host} h={h} rt={runtime[h.host]} route={route} now={now} />
        ))}
      </nav>
      <div className="safe-bottom shrink-0 border-t border-line px-2 py-1.5">
        <button
          type="button"
          onClick={() => navigate({ name: 'settings' })}
          className={cx('flex h-9 w-full items-center gap-2.5 rounded-md px-2 text-sm text-muted hover:bg-hover hover:text-fg max-md:h-11', route.name === 'settings' && 'bg-active text-fg')}
        >
          <Settings size={16} /> 设置
        </button>
      </div>
    </aside>
  );
}
