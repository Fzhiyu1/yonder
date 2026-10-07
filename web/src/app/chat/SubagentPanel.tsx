import { useEffect, useLayoutEffect, useMemo, useRef } from 'react';
import { ArrowUp, ChevronDown, X } from 'lucide-react';
import { groupSteps } from '../../lib/steps';
import type { ChatItem } from '../../proto/generated/ChatItem';
import { useChats } from '../../store/chat';
import { loadOlderThread, threadKey, useSubagents, watchThread } from '../../store/subagents';
import { cx, IconButton, Spinner, useIsDesktop } from '../ui/primitives';
import { ApprovalCard } from './ApprovalCard';
import { ChatBlockView, SubagentStatusPill, subagentName } from './ChatItems';

/** The card of sub-agent `thread`: in the chat, or in another sub-agent's thread (nested). */
function useCard(chat: string, thread: string): ChatItem | undefined {
  const own = useChats((st) => {
    const c = st.chats[chat];
    if (!c) return undefined;
    for (const id of c.order) if (c.byId[id].subagent?.id === thread) return c.byId[id];
    return undefined;
  });
  const nested = useSubagents((st) => {
    if (own) return undefined;
    for (const [k, v] of Object.entries(st.views)) {
      if (!k.startsWith(`${chat}#`) || !v.byId) continue;
      for (const it of Object.values(v.byId)) if (it.subagent?.id === thread) return it;
    }
    return undefined;
  });
  return own ?? nested;
}

/**
 * Read-only view of one sub-agent's thread: its task, messages and tool calls, live while it
 * runs. A full-height window over the chat on phones, a panel next to it on wide screens.
 * Approvals the sub-agent waits for can be answered here as in the chat.
 */
export function SubagentPanel({ host, session, chat, exited }: { host: string; session: string; chat: string; exited: boolean }) {
  const thread = useSubagents((st) => st.open[chat]);
  const desktop = useIsDesktop();
  const hide = () => useSubagents.getState().hide(chat);

  useEffect(() => {
    if (!thread) return;
    return watchThread(host, session, chat, thread);
  }, [host, session, chat, thread]);

  useEffect(() => {
    if (!thread || desktop) return;
    const onKey = (e: KeyboardEvent) => e.key === 'Escape' && useSubagents.getState().hide(chat);
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [thread, desktop, chat]);

  if (!thread) return null;
  return (
    <section
      aria-label="子智能体"
      className={cx(
        'flex min-h-0 flex-col bg-bg',
        // Below the artifact viewer (z-40): artifacts opened from the thread show on top of it.
        desktop ? 'h-full w-[min(44%,640px)] min-w-[360px] shrink-0 border-l border-line' : 'viewer-sheet app-band safe-top fixed inset-x-0 z-30',
      )}
    >
      <ThreadBody key={thread} host={host} session={session} chat={chat} thread={thread} exited={exited} desktop={desktop} onClose={hide} />
    </section>
  );
}

function ThreadBody({ host, session, chat, thread, exited, desktop, onClose }: { host: string; session: string; chat: string; thread: string; exited: boolean; desktop: boolean; onClose: () => void }) {
  const card = useCard(chat, thread);
  const view = useSubagents((st) => st.views[threadKey(chat, thread)]);
  const allApprovals = useChats((st) => st.chats[chat]?.approvals);
  const approvals = useMemo(() => (allApprovals ?? []).filter((a) => a.thread === thread), [allApprovals, thread]);
  const items = useMemo(() => (view?.order ?? []).map((id) => view!.byId![id]), [view?.order, view?.byId]); // eslint-disable-line react-hooks/exhaustive-deps
  const blocks = useMemo(() => groupSteps(items), [items]);
  const scroller = useRef<HTMLDivElement>(null);
  const stick = useRef(true);
  const sub = card?.subagent;
  const running = sub?.status === 'running';
  const last = items[items.length - 1];
  const sig = `${items.length}:${last?.text?.length ?? 0}:${last?.output?.length ?? 0}:${last?.status}:${approvals.length}`;

  useLayoutEffect(() => {
    const el = scroller.current;
    if (el && stick.current) el.scrollTop = el.scrollHeight;
  }, [sig]);

  const onScroll = () => {
    const el = scroller.current;
    if (!el) return;
    stick.current = el.scrollHeight - el.scrollTop - el.clientHeight < 48;
    if (el.scrollTop < 300 && view?.more && !view.loadingOlder) void loadOlderThread(host, session, chat, thread);
  };

  const meta = [sub?.name && sub.role ? sub.role : '', sub?.model ?? ''].filter(Boolean).join(' · ');
  return (
    <>
      <div className="flex h-12 shrink-0 items-center gap-1 border-b border-line pr-2 pl-1">
        <IconButton label={desktop ? '关闭子智能体' : '返回对话'} onClick={onClose}>
          {desktop ? <X size={17} /> : <ChevronDown size={20} />}
        </IconButton>
        <div className="flex min-w-0 flex-1 flex-col justify-center">
          <div className="truncate text-[14px] leading-5 font-semibold">{card ? subagentName(card) : '子智能体'}</div>
          {meta && <div className="truncate text-[12px] leading-4 text-faint">{meta}</div>}
        </div>
        {sub && <SubagentStatusPill status={sub.status} />}
      </div>
      <div ref={scroller} onScroll={onScroll} className="scroll-thin min-h-0 flex-1 overflow-y-auto overscroll-contain">
        <div className="mx-auto flex w-full max-w-3xl flex-col gap-2.5 px-4 pt-4 pb-6">
          {view?.more && (
            <button
              type="button"
              onClick={() => void loadOlderThread(host, session, chat, thread)}
              disabled={view.loadingOlder}
              className="mx-auto inline-flex h-8 items-center gap-1.5 rounded-full px-3 text-[12px] text-faint hover:bg-hover max-md:h-11"
            >
              {view.loadingOlder ? <Spinner size={12} /> : <ArrowUp size={13} />} {view.loadingOlder ? '正在加载' : '更早的内容'}
            </button>
          )}
          {view?.loading && (
            <div className="flex items-center justify-center gap-2 py-10 text-sm text-muted">
              <Spinner /> 正在加载
            </div>
          )}
          {view?.error && <div className="py-10 text-center text-sm text-danger">{view.error}</div>}
          {!view?.loading && !view?.error && !items.length && (
            <div className="py-10 text-center text-sm text-muted">{running ? '子智能体正在启动' : '没有可显示的内容'}</div>
          )}
          {blocks.map((b, i) => (
            <ChatBlockView key={b.type === 'item' ? b.item.id : b.id} block={b} live={i === blocks.length - 1 && running} />
          ))}
          {running && last?.status !== 'in_progress' && !approvals.length && (
            <div className="flex items-center gap-2 px-1.5 text-[13px] text-muted">
              <Spinner size={13} /> 处理中
            </div>
          )}
        </div>
      </div>
      {!!approvals.length && !exited && (
        <div className="safe-bottom shrink-0 border-t border-line pt-2">
          <div className="scroll-thin mx-auto flex max-h-[calc(var(--vv-height,100dvh)*0.5)] w-full max-w-3xl flex-col gap-2 overflow-y-auto overscroll-contain px-3 pb-2">
            {approvals.map((a) => (
              <ApprovalCard key={a.id} host={host} session={session} approval={a} />
            ))}
          </div>
        </div>
      )}
    </>
  );
}
