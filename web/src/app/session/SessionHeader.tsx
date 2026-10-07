import { useEffect, useRef, useState } from 'react';
import { ArrowLeft, Copy, Hand, MessageSquare, MoreHorizontal, Pencil, Power, Trash2 } from 'lucide-react';
import type { ChatStatus } from '../../proto/generated/ChatStatus';
import type { ChatItem } from '../../proto/generated/ChatItem';
import type { SessionInfo } from '../../proto/generated/SessionInfo';
import { middleTruncate } from '../../lib/format';
import { useUi } from '../../store/ui';
import { copyText } from '../../lib/clipboard';
import { navigate } from '../router';
import { ArtifactsButton } from '../artifacts/ArtifactList';
import { AGENT_LABEL, AgentIcon } from '../ui/icons';
import { cx, IconButton, Menu, Spinner, useIsDesktop } from '../ui/primitives';
import { canContinueAsChat, continueAsChat, interruptSession, killSession, removeSession, renameSession } from './actions';
import { MODE_INFO } from './approval';

const STATUS: Record<ChatStatus, { label: string; cls: string }> = {
  starting: { label: '启动中', cls: 'text-muted border-line-strong' },
  idle: { label: '空闲', cls: 'text-muted border-line-strong' },
  working: { label: '运行中', cls: 'text-accent border-accent/40 bg-accent-soft' },
  awaiting_approval: { label: '待审批', cls: 'text-warn border-warn/40 bg-warn-soft' },
  error: { label: '出错', cls: 'text-danger border-danger/40 bg-danger-soft' },
  exited: { label: '已结束', cls: 'text-faint border-line' },
};

export function StatusPill({ s, chatStatus }: { s: SessionInfo; chatStatus?: ChatStatus }) {
  let st: { label: string; cls: string };
  if (s.state === 'exited' || s.state === 'failed') st = { label: s.state === 'failed' ? '失败' : '已结束', cls: 'text-faint border-line' };
  else if (s.kind === 'chat') st = STATUS[chatStatus ?? s.chat_status ?? 'idle'];
  else st = { label: '运行中', cls: 'text-ok border-ok/40' };
  return (
    <span className={cx('inline-flex h-6 shrink-0 items-center gap-1 rounded-full border px-2 text-[12px] font-medium whitespace-nowrap', st.cls)}>
      {(chatStatus ?? s.chat_status) === 'working' && s.state !== 'exited' && <Spinner size={11} />}
      {st.label}
    </span>
  );
}

/** Title; on desktop a click renames it, on phones renaming is in the menu (taps are scrolls too). */
function TitleEditor({ host, s, editing, setEditing, clickToEdit }: { host: string; s: SessionInfo; editing: boolean; setEditing: (v: boolean) => void; clickToEdit: boolean }) {
  const [value, setValue] = useState(s.title);
  const input = useRef<HTMLInputElement>(null);
  useEffect(() => {
    if (editing) {
      setValue(s.title);
      input.current?.select();
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [editing]);
  if (editing) {
    const commit = () => {
      setEditing(false);
      const t = value.trim();
      if (t && t !== s.title) void renameSession(host, s, t);
    };
    return (
      <input
        ref={input}
        autoFocus
        value={value}
        onChange={(e) => setValue(e.target.value)}
        onBlur={commit}
        onKeyDown={(e) => {
          if (e.key === 'Enter') commit();
          if (e.key === 'Escape') setEditing(false);
        }}
        enterKeyHint="done"
        aria-label="会话标题"
        className="h-7 w-full min-w-0 rounded border border-accent bg-bg px-1.5 text-[14px] font-semibold focus:outline-none max-md:h-8"
      />
    );
  }
  if (!clickToEdit) {
    return <h1 className="block max-w-full min-w-0 truncate text-[15px] leading-5 font-semibold">{s.title || s.command.join(' ')}</h1>;
  }
  return (
    <button
      type="button"
      title="点击重命名"
      onClick={() => setEditing(true)}
      className="block max-w-full min-w-0 truncate rounded px-1 -mx-1 text-left text-[14px] font-semibold hover:bg-hover"
    >
      {s.title || s.command.join(' ')}
    </button>
  );
}

export function SessionHeader({ host, s, chatStatus, items }: { host: string; s: SessionInfo; chatStatus?: ChatStatus; items?: ChatItem[] }) {
  const desktop = useIsDesktop();
  const [editing, setEditing] = useState(false);
  const exited = s.state === 'exited' || s.state === 'failed';
  const working = (chatStatus ?? s.chat_status) === 'working' || (chatStatus ?? s.chat_status) === 'awaiting_approval';
  const meta = [AGENT_LABEL[s.agent], s.model].filter(Boolean).join(' · ');
  const mode = s.approval && !exited ? MODE_INFO[s.approval] : undefined;
  return (
    <header className="safe-top shrink-0 border-b border-line bg-bg">
      <div className="flex h-12 items-center gap-1.5 px-1 md:gap-2 md:px-4 max-md:h-14">
        {!desktop && (
          <IconButton label="返回" onClick={() => navigate({ name: 'home' })}>
            <ArrowLeft size={18} />
          </IconButton>
        )}
        <span className="hidden shrink-0 text-muted md:inline-flex">
          <AgentIcon agent={s.agent} kind={s.kind} size={16} />
        </span>
        <div className="flex min-w-0 flex-1 flex-col justify-center">
          <TitleEditor key={s.id} host={host} s={s} editing={editing} setEditing={setEditing} clickToEdit={desktop} />
          <div className="flex min-w-0 items-center gap-1.5 text-[12px] leading-4 text-muted">
            <span className="min-w-0 shrink truncate">{meta}</span>
            {mode && s.approval !== 'ask' && (
              <span className={cx('inline-flex shrink-0 items-center gap-0.5', MODE_INFO[s.approval!].tone)} title={`审批模式：${mode.label}`}>
                <mode.icon size={12} />
                {mode.short}
              </span>
            )}
            <span className="shrink-0 text-faint">·</span>
            <span className="min-w-0 flex-1 truncate font-mono" title={s.cwd}>
              {middleTruncate(s.cwd, desktop ? 64 : 24)}
            </span>
          </div>
        </div>
        <StatusPill s={s} chatStatus={chatStatus} />
        {items && <ArtifactsButton items={items} />}
        <Menu
          width={232}
          trigger={(p) => (
            <IconButton label="更多操作" {...p}>
              <MoreHorizontal size={18} />
            </IconButton>
          )}
          items={[
            { label: '中断', icon: <Hand size={15} />, onSelect: () => void interruptSession(host, s), hidden: s.kind !== 'chat' || exited, disabled: !working },
            { label: '重命名', icon: <Pencil size={15} />, onSelect: () => setEditing(true) },
            { label: '以对话继续', icon: <MessageSquare size={15} />, onSelect: () => void continueAsChat(host, s), hidden: !canContinueAsChat(s) },
            {
              label: '复制会话 ID',
              icon: <Copy size={15} />,
              onSelect: async () => {
                if (await copyText(s.agent_session ?? s.id)) useUi.getState().toast('success', '已复制会话 ID');
              },
            },
            { label: '结束会话', icon: <Power size={15} />, onSelect: () => void killSession(host, s), hidden: exited, danger: true },
            { label: '删除', icon: <Trash2 size={15} />, onSelect: () => void removeSession(host, s), hidden: !exited, danger: true },
          ]}
        />
      </div>
    </header>
  );
}
