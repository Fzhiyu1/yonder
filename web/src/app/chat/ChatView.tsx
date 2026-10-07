import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { ArrowDown, ArrowUp, History, RotateCcw } from 'lucide-react';
import type { Artifact } from '../../lib/artifacts';
import { groupSteps } from '../../lib/steps';
import type { SessionInfo } from '../../proto/generated/SessionInfo';
import { attachChat, chatKey, discardPending, loadOlder, sendChat, useChats } from '../../store/chat';
import { useHosts } from '../../store/hosts';
import { useViewer } from '../../store/viewer';
import { ArtifactContext, type ArtifactCtx } from '../artifacts/context';
import { Viewer } from '../artifacts/Viewer';
import { Button, Spinner, useIsDesktop } from '../ui/primitives';
import { resumeSession } from '../session/actions';
import { ApprovalModeMenu } from '../session/ApprovalModeMenu';
import { ModelMenu } from '../session/ModelMenu';
import { SessionHeader } from '../session/SessionHeader';
import { ApprovalCard } from './ApprovalCard';
import { ChatBlockView, UserBubble } from './ChatItems';
import { Composer } from './Composer';
import { SubagentPanel } from './SubagentPanel';
import { useSubagents } from '../../store/subagents';

export function ChatView({ host, s }: { host: string; s: SessionInfo }) {
  const key = chatKey(host, s.id);
  const chat = useChats((st) => st.chats[key]);
  const scroller = useRef<HTMLDivElement>(null);
  const stick = useRef(true);
  const [showJump, setShowJump] = useState(false);
  const [resuming, setResuming] = useState(false);
  const info = useHosts((st) => st.runtime[host]?.info);
  const desktop = useIsDesktop();
  const viewerOpen = useViewer((st) => !!st.byChat[key]?.open);
  const threadOpen = useSubagents((st) => !!st.open[key]);

  const open = useCallback((a: Artifact) => useViewer.getState().openArtifact(key, a), [key]);
  const ctx = useMemo<ArtifactCtx>(
    () => ({ host, chat: key, cwd: s.cwd, home: info?.home, files: !info || info.permissions.includes('files'), open }),
    [host, key, s.cwd, info, open],
  );

  useEffect(() => attachChat(host, s.id), [host, s.id]);

  useEffect(() => {
    stick.current = true;
    setShowJump(false);
  }, [key]);

  const items = chat ? chat.order.map((id) => chat.byId[id]) : [];
  const blocks = useMemo(() => groupSteps(items), [chat?.order, chat?.byId]); // eslint-disable-line react-hooks/exhaustive-deps
  const last = items[items.length - 1];
  const first = items[0]?.id;
  const contentSig = `${last?.id}:${last?.text?.length ?? 0}:${last?.output?.length ?? 0}:${last?.status}:${chat?.pending.length ?? 0}:${chat?.approvals.length ?? 0}`;
  // Older items inserted above: keep what the user is reading in place.
  const anchor = useRef<{ first?: string; height: number; top: number }>({ height: 0, top: 0 });

  useLayoutEffect(() => {
    const el = scroller.current;
    if (!el) return;
    const a = anchor.current;
    if (a.first !== undefined && first !== a.first && !stick.current) el.scrollTop = a.top + (el.scrollHeight - a.height);
    anchor.current = { first, height: el.scrollHeight, top: el.scrollTop };
  }, [first]);

  useLayoutEffect(() => {
    const el = scroller.current;
    if (!el) return;
    if (stick.current) el.scrollTop = el.scrollHeight;
    else setShowJump(true);
    anchor.current = { first, height: el.scrollHeight, top: el.scrollTop };
  }, [contentSig]);

  const older = () => void loadOlder(host, s.id);

  const onScroll = () => {
    const el = scroller.current;
    if (!el) return;
    const atBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 48;
    stick.current = atBottom;
    if (atBottom) setShowJump(false);
    anchor.current = { first, height: el.scrollHeight, top: el.scrollTop };
    if (el.scrollTop < 400 && chat?.truncated && !chat.loadingOlder) older();
  };

  // A short first page may not fill the screen, so there is nothing to scroll: load on.
  useEffect(() => {
    const el = scroller.current;
    if (el && chat?.attached && chat.truncated && !chat.loadingOlder && el.scrollHeight <= el.clientHeight + 400) older();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [chat?.attached, chat?.truncated, chat?.loadingOlder, items.length]);

  const jump = () => {
    const el = scroller.current;
    if (!el) return;
    stick.current = true;
    el.scrollTo({ top: el.scrollHeight, behavior: 'smooth' });
    setShowJump(false);
  };

  const exited = s.state === 'exited' || s.state === 'failed' || chat?.status === 'exited';
  const working = chat?.status === 'working' || chat?.status === 'awaiting_approval';

  return (
    <ArtifactContext.Provider value={ctx}>
    <div className="flex h-full min-h-0">
    <div className="flex h-full min-h-0 min-w-0 flex-1 flex-col" inert={!desktop && (viewerOpen || threadOpen)}>
      <SessionHeader host={host} s={s} chatStatus={chat?.status} items={items} />
      <div className="relative min-h-0 flex-1">
        <div ref={scroller} onScroll={onScroll} className="scroll-thin absolute inset-0 overflow-y-auto">
          <div className="mx-auto flex w-full max-w-3xl flex-col gap-2.5 px-4 pt-4 pb-6 md:px-6">
            {chat?.truncated && (
              <button
                type="button"
                onClick={older}
                disabled={chat.loadingOlder}
                className="mx-auto inline-flex h-8 items-center gap-1.5 rounded-full px-3 text-[12px] text-faint hover:bg-hover max-md:h-10"
              >
                {chat.loadingOlder ? <Spinner size={12} /> : <ArrowUp size={13} />} {chat.loadingOlder ? '正在加载更早的消息' : '更早的消息'}
              </button>
            )}
            {chat?.loading && !chat.attached && (
              <div className="flex items-center justify-center gap-2 py-10 text-sm text-muted">
                <Spinner /> 正在加载
              </div>
            )}
            {chat?.error && !chat.attached && <div className="py-10 text-center text-sm text-danger">{chat.error}</div>}
            {blocks.map((b, i) => (
              <ChatBlockView key={b.type === 'item' ? b.item.id : b.id} block={b} live={i === blocks.length - 1 && !!working} />
            ))}
            {chat?.pending.map((p) => (
              <UserBubble
                key={p.id}
                text={p.text}
                paths={p.attachments}
                pending={p}
                onRetry={() => {
                  discardPending(host, s.id, p.id);
                  void sendChat(host, s.id, p.text, p.attachments);
                }}
                onDiscard={() => discardPending(host, s.id, p.id)}
              />
            ))}
            {chat?.status === 'working' && last?.status !== 'in_progress' && (
              <div className="flex items-center gap-2 px-1.5 text-[13px] text-muted">
                <Spinner size={13} /> {chat.statusDetail || '处理中'}
              </div>
            )}
            {chat?.status === 'error' && chat.statusDetail && <div className="px-1.5 text-[13px] text-danger">{chat.statusDetail}</div>}
          </div>
        </div>
        {showJump && (
          <button
            type="button"
            onClick={jump}
            className="rise-in absolute bottom-3 left-1/2 inline-flex h-8 -translate-x-1/2 items-center gap-1.5 rounded-full border border-line-strong bg-bg px-3 text-[13px] shadow-pop max-md:h-11 max-md:px-4"
          >
            <ArrowDown size={14} /> 新消息
          </button>
        )}
      </div>
      <div className="safe-bottom shrink-0">
        {!!chat?.approvals.length && !exited && (
          <div className="scroll-thin mx-auto flex max-h-[calc(var(--vv-height,100dvh)*0.5)] w-full max-w-3xl flex-col gap-2 overflow-y-auto overscroll-contain px-3 pb-2 md:px-6">
            {chat.approvals.map((a) => (
              <div key={a.id} className="rise-in">
                <ApprovalCard host={host} session={s.id} approval={a} />
              </div>
            ))}
          </div>
        )}
        {exited ? (
          <div className="mx-auto flex w-full max-w-3xl items-center gap-3 px-3 pb-3 md:px-6">
            <div className="flex h-11 min-w-0 flex-1 items-center gap-2 rounded-lg border border-line bg-panel px-3 text-sm text-muted">
              <History size={15} className="shrink-0" />
              <span className="truncate">会话已结束</span>
            </div>
            {s.agent_session && (
              <Button
                variant="primary"
                busy={resuming}
                icon={<RotateCcw size={15} />}
                onClick={async () => {
                  setResuming(true);
                  await resumeSession(host, s);
                  setResuming(false);
                }}
              >
                恢复会话
              </Button>
            )}
          </div>
        ) : (
          <Composer
            host={host}
            session={s.id}
            working={working}
            disabled={!chat?.attached}
            tools={
              <>
                {s.approval && <ApprovalModeMenu host={host} s={s} disabled={!chat?.attached} />}
                {s.agent !== 'custom' && s.agent !== 'shell' && <ModelMenu host={host} s={s} disabled={!chat?.attached} />}
              </>
            }
          />
        )}
      </div>
    </div>
    <Viewer host={host} chat={key} />
    <SubagentPanel host={host} session={s.id} chat={key} exited={exited} />
    </div>
    </ArtifactContext.Provider>
  );
}
