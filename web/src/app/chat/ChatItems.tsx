import { memo, useState, type ReactNode } from 'react';
import {
  AlertCircle,
  Ban,
  Check,
  ChevronDown,
  ChevronRight,
  FileDiff,
  Globe,
  Image as ImageIcon,
  Lightbulb,
  ListChecks,
  Paperclip,
  RotateCw,
  SquareTerminal,
  Trash2,
  Wrench,
  X,
} from 'lucide-react';
import { formatDuration } from '../../lib/format';
import { baseName } from '../../lib/paths';
import { stepsSummary, type ChatBlock } from '../../lib/steps';
import type { ChatItem } from '../../proto/generated/ChatItem';
import type { PendingMessage } from '../../store/chat';
import { useArtifactCtx } from '../artifacts/context';
import { ArtifactChips, ImageThumbs } from '../artifacts/Inline';
import { artifactFor } from '../../lib/artifacts';
import { cx, Spinner } from '../ui/primitives';
import { DiffView, diffStats } from './Diff';
import { CopyButton, Markdown } from './Markdown';

function StatusIcon({ status }: { status: ChatItem['status'] }) {
  if (status === 'in_progress') return <Spinner size={13} className="text-accent" />;
  if (status === 'completed') return <Check size={13} className="text-ok" />;
  if (status === 'declined') return <Ban size={13} className="text-warn" />;
  return <X size={13} className="text-danger" />;
}

/** A compact work row: icon, one line, chevron; opens to show details. */
function Collapsible({
  header,
  children,
  defaultOpen = false,
  open: forced,
}: {
  header: ReactNode;
  children: ReactNode;
  defaultOpen?: boolean;
  open?: boolean;
}) {
  const [open, setOpen] = useState(defaultOpen);
  const [touched, setTouched] = useState(false);
  const isOpen = touched ? open : (forced ?? open);
  return (
    <div className="min-w-0">
      <button
        type="button"
        onClick={() => {
          setTouched(true);
          setOpen(!isOpen);
        }}
        className="group/row flex min-h-7 w-full min-w-0 items-center gap-2 rounded-md px-1 py-0.5 text-left hover:bg-hover max-md:min-h-9"
        aria-expanded={isOpen}
      >
        {header}
        <span className={cx('ml-auto shrink-0 text-faint transition-opacity', !isOpen && 'opacity-0 group-hover/row:opacity-100 max-md:opacity-60')}>
          {isOpen ? <ChevronDown size={13} /> : <ChevronRight size={13} />}
        </span>
      </button>
      {isOpen && <div className="mt-0.5 mb-1.5 pl-6 max-md:pl-1">{children}</div>}
    </div>
  );
}

const rowIcon = 'flex w-4 shrink-0 justify-center';

function CommandItem({ item }: { item: ChatItem }) {
  const running = item.status === 'in_progress';
  const failed = item.exit_code !== undefined && item.exit_code !== 0;
  const meta = [failed ? `退出码 ${item.exit_code}` : '', formatDuration(item.duration_ms)].filter(Boolean).join(' · ');
  return (
    <Collapsible
      open={running && !!item.output}
      header={
        <>
          <span className={rowIcon}>{running || failed || item.status !== 'completed' ? <StatusIcon status={failed ? 'failed' : item.status} /> : <SquareTerminal size={13} className="text-faint" />}</span>
          <code className="min-w-0 flex-1 truncate font-mono text-[12.5px] text-muted">{item.title}</code>
          {meta && <span className={cx('shrink-0 text-[11.5px] tabular-nums', failed ? 'text-danger' : 'text-faint')}>{meta}</span>}
        </>
      }
    >
      <div className="relative">
        <pre className="scroll-thin max-h-72 overflow-auto rounded-md border border-line bg-code px-3 py-2 font-mono text-[12px] leading-[1.5] whitespace-pre-wrap break-all">
          {item.output || (running ? '等待输出…' : '（无输出）')}
        </pre>
        {item.output && <CopyButton text={item.output} label="复制输出" className="absolute top-1 right-1 inline-flex h-7 w-7 items-center justify-center rounded bg-code text-faint hover:text-fg max-md:h-10 max-md:w-10" />}
      </div>
    </Collapsible>
  );
}

function FileChangeItem({ item }: { item: ChatItem }) {
  const stats = item.diff ? diffStats(item.diff) : undefined;
  return (
    <div className="overflow-hidden rounded-md border border-line">
      <Collapsible
        header={
          <>
            <span className={cx(rowIcon, 'text-muted')}>{item.status === 'in_progress' ? <Spinner size={13} /> : <FileDiff size={14} />}</span>
            <span className="min-w-0 flex-1 truncate text-[13px]">
              <span className="font-medium">{item.title || '文件变更'}</span>
              {item.paths.length > 0 && <span className="ml-2 font-mono text-[12px] text-muted">{item.paths.map((p) => baseName(p)).join(', ')}</span>}
            </span>
            {stats && (
              <span className="shrink-0 font-mono text-[12px] tabular-nums">
                <span className="text-ok">+{stats.add}</span> <span className="text-danger">-{stats.del}</span>
              </span>
            )}
          </>
        }
      >
      {item.paths.length > 0 && (
        <ul className="mb-1.5 flex flex-col gap-0.5 font-mono text-[12px] text-muted">
          {item.paths.map((p) => (
            <li key={p} className="truncate" title={p}>
              {p}
            </li>
          ))}
        </ul>
      )}
      {item.diff ? <DiffView diff={item.diff} className="max-h-[420px]" /> : <div className="text-[13px] text-faint">无差异内容</div>}
      </Collapsible>
    </div>
  );
}

function ToolItem({ item }: { item: ChatItem }) {
  const image = item.paths.some((p) => artifactFor(p)?.kind === 'image');
  const icon = item.kind === 'web_search' ? <Globe size={13} /> : image ? <ImageIcon size={13} /> : <Wrench size={13} />;
  const body = item.output || item.text;
  const header = (
    <>
      <span className={cx(rowIcon, 'text-faint')}>{item.status === 'in_progress' ? <Spinner size={13} /> : icon}</span>
      <span className="min-w-0 flex-1 truncate text-[13px] text-muted">
        {item.kind === 'web_search' ? '搜索 ' : ''}
        <span className={item.kind === 'tool' && !image ? 'font-mono text-[12.5px]' : ''}>{item.title || (item.kind === 'web_search' ? '网络搜索' : '工具调用')}</span>
        {image && <span className="ml-1.5 font-mono text-[12px] text-faint">{item.paths.map((p) => baseName(p)).join(', ')}</span>}
      </span>
      {item.status === 'failed' && <X size={13} className="shrink-0 text-danger" />}
    </>
  );
  const row = !body ? (
    <div className="flex min-h-7 items-center gap-2 px-1 py-0.5 max-md:min-h-9">{header}</div>
  ) : (
    <Collapsible header={header}>
      <pre className="scroll-thin max-h-60 overflow-auto rounded-md border border-line bg-code px-3 py-2 font-mono text-[12px] whitespace-pre-wrap break-all">{body}</pre>
    </Collapsible>
  );
  if (!image) return row;
  return (
    <div>
      {row}
      <ImageThumbs item={item} />
    </div>
  );
}

function ReasoningItem({ item }: { item: ChatItem }) {
  const running = item.status === 'in_progress';
  const first = (item.text ?? '').replace(/[*_`#>]/g, '').split('\n').find((l) => l.trim())?.trim();
  return (
    <Collapsible
      header={
        <>
          <span className={cx(rowIcon, 'text-faint')}>{running ? <Spinner size={13} /> : <Lightbulb size={13} />}</span>
          <span className="shrink-0 text-[13px] text-muted">{running ? '思考中' : '思考'}</span>
          {first && <span className="min-w-0 flex-1 truncate text-[12.5px] text-faint">{first}</span>}
        </>
      }
    >
      <Markdown text={item.text ?? ''} className="border-l-2 border-line pl-3 text-[13px] text-muted" />
    </Collapsible>
  );
}

/** A run of work steps folded into one line, open while the newest step is still running. */
function StepGroup({ items, live }: { items: ChatItem[]; live: boolean }) {
  const running = items.some((i) => i.status === 'in_progress');
  const [open, setOpen] = useState<boolean>();
  const isOpen = open ?? (live && running);
  const failed = items.some((i) => i.status === 'failed' || (i.exit_code !== undefined && i.exit_code !== 0));
  return (
    <div className="min-w-0">
      <button
        type="button"
        aria-expanded={isOpen}
        onClick={() => setOpen(!isOpen)}
        className="flex min-h-7 w-full min-w-0 items-center gap-2 rounded-md px-1 py-0.5 text-left text-[13px] text-muted hover:bg-hover max-md:min-h-9"
      >
        <span className={rowIcon}>{running ? <Spinner size={13} className="text-accent" /> : failed ? <AlertCircle size={13} className="text-warn" /> : <Check size={13} className="text-faint" />}</span>
        <span className="min-w-0 truncate">{stepsSummary(items)}</span>
        {isOpen ? <ChevronDown size={13} className="shrink-0 text-faint" /> : <ChevronRight size={13} className="shrink-0 text-faint" />}
      </button>
      {isOpen && (
        <div className="mt-0.5 ml-[11px] flex flex-col gap-0.5 border-l border-line pl-2 max-md:ml-[9px] max-md:pl-1.5">
          {items.map((it) => (
            <ChatItemView key={it.id} item={it} />
          ))}
        </div>
      )}
    </div>
  );
}

export const ChatBlockView = memo(function ChatBlockView({ block, live }: { block: ChatBlock; live: boolean }) {
  if (block.type === 'steps') return <StepGroup items={block.items} live={live} />;
  return <ChatItemView item={block.item} />;
});

function PlanItem({ item }: { item: ChatItem }) {
  return (
    <div className="rounded-md border border-line bg-panel px-3 py-2.5">
      <div className="mb-1 flex items-center gap-2 text-[13px] font-medium text-muted">
        <ListChecks size={15} /> 计划
      </div>
      <Markdown text={item.text ?? ''} className="text-[13.5px]" />
    </div>
  );
}

function Attachments({ paths }: { paths: string[] }) {
  const ctx = useArtifactCtx();
  if (!paths.length) return null;
  return (
    <div className="mt-1.5 flex flex-wrap justify-end gap-1.5">
      {paths.map((p) => {
        const a = artifactFor(p);
        const can = !!a && !!ctx?.files;
        return (
          <button
            key={p}
            type="button"
            title={p}
            disabled={!can}
            onClick={() => a && ctx?.open(a)}
            className="inline-flex h-7 max-w-[220px] items-center gap-1 rounded-md border border-line bg-bg px-2 text-[12px] text-muted enabled:hover:bg-hover disabled:cursor-default max-md:h-9"
          >
            <Paperclip size={12} className="shrink-0" />
            <span className="truncate">{baseName(p)}</span>
          </button>
        );
      })}
    </div>
  );
}

export function UserBubble({ text, paths, pending, onRetry, onDiscard }: { text: string; paths: string[]; pending?: PendingMessage; onRetry?: () => void; onDiscard?: () => void }) {
  return (
    <div className="flex flex-col items-end">
      <div className={cx('max-w-[85%] rounded-2xl rounded-br-md bg-active px-3.5 py-2 text-[14.5px] leading-6 break-words whitespace-pre-wrap md:max-w-[75%]', pending && !pending.failed && 'opacity-70')}>
        {text}
      </div>
      <Attachments paths={paths} />
      {pending?.failed && (
        <div className="mt-1 flex items-center gap-1 text-[12px] text-danger">
          <AlertCircle size={13} /> 发送失败
          {onRetry && (
            <button type="button" onClick={onRetry} className="ml-1 inline-flex items-center gap-1 rounded px-1.5 py-0.5 text-muted hover:bg-hover hover:text-fg">
              <RotateCw size={12} /> 重试
            </button>
          )}
          {onDiscard && (
            <button type="button" onClick={onDiscard} title="删除" aria-label="删除" className="inline-flex items-center rounded px-1 py-0.5 text-muted hover:bg-hover hover:text-fg">
              <Trash2 size={12} />
            </button>
          )}
        </div>
      )}
    </div>
  );
}

export const ChatItemView = memo(function ChatItemView({ item }: { item: ChatItem }) {
  switch (item.kind) {
    case 'user':
      return <UserBubble text={item.text ?? ''} paths={item.paths} />;
    case 'agent':
      return (
        <div className="min-w-0">
          <Markdown text={item.text ?? ''} className={cx('text-[14.5px]', item.status === 'in_progress' && 'stream-caret')} />
          {item.status !== 'in_progress' && <ArtifactChips item={item} className="mt-2" />}
        </div>
      );
    case 'reasoning':
      return <ReasoningItem item={item} />;
    case 'plan':
      return <PlanItem item={item} />;
    case 'command':
      return <CommandItem item={item} />;
    case 'file_change':
      return (
        <div className="min-w-0">
          <FileChangeItem item={item} />
          <ArtifactChips item={item} className="mt-1.5" />
        </div>
      );
    case 'tool':
    case 'web_search':
      return <ToolItem item={item} />;
    case 'error':
      return (
        <div className="flex items-start gap-2 rounded-md border border-danger/30 bg-danger-soft px-3 py-2 text-[13.5px] text-danger">
          <AlertCircle size={15} className="mt-0.5 shrink-0" />
          <Markdown text={item.text || item.title || '错误'} className="min-w-0 flex-1" />
        </div>
      );
    case 'system':
      return (
        <div className="flex items-center gap-3 py-1 text-[12px] text-faint">
          <span className="h-px flex-1 bg-line" />
          <span className="max-w-[80%] text-center break-words">{item.text || item.title}</span>
          <span className="h-px flex-1 bg-line" />
        </div>
      );
  }
});
