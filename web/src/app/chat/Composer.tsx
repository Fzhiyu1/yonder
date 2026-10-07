import { useEffect, useLayoutEffect, useRef, useState, type ReactNode } from 'react';
import { ArrowUp, Paperclip, Square, X } from 'lucide-react';
import { bytesToB64 } from '../../lib/base64';
import { getConn } from '../../net/provider';
import { call } from '../../net/types';
import { sendChat } from '../../store/chat';
import { toastError, useUi } from '../../store/ui';
import { cx, IconButton, Spinner, useIsMobile } from '../ui/primitives';

interface Attachment {
  id: string;
  name: string;
  path?: string;
  uploading: boolean;
}

const MAX_UPLOAD = 20 * 1024 * 1024;
const drafts = new Map<string, string>();

export function Composer({
  host,
  session,
  working,
  disabled,
  tools,
}: {
  host: string;
  session: string;
  working: boolean;
  disabled?: boolean;
  /** Extra controls at the start of the toolbar (approval mode). */
  tools?: ReactNode;
}) {
  const key = `${host}/${session}`;
  const [text, setText] = useState(() => drafts.get(key) ?? '');
  const [atts, setAtts] = useState<Attachment[]>([]);
  const [stopping, setStopping] = useState(false);
  const ta = useRef<HTMLTextAreaElement>(null);
  const file = useRef<HTMLInputElement>(null);
  const mobile = useIsMobile();

  useEffect(() => {
    setText(drafts.get(key) ?? '');
    setAtts([]);
  }, [key]);

  useLayoutEffect(() => {
    const el = ta.current;
    if (!el) return;
    el.style.height = 'auto';
    el.style.height = `${Math.min(el.scrollHeight, 220)}px`;
  }, [text]);

  useEffect(() => {
    if (!working) setStopping(false);
  }, [working]);

  const uploading = atts.some((a) => a.uploading);
  const canSend = !disabled && !uploading && (text.trim().length > 0 || atts.length > 0);
  const typed = text.trim().length > 0 || atts.length > 0;

  const send = () => {
    if (!canSend) return;
    const paths = atts.map((a) => a.path!).filter(Boolean);
    void sendChat(host, session, text.trim(), paths);
    setText('');
    drafts.delete(key);
    setAtts([]);
    // Phones: drop the keyboard so the reply is visible; desktop keeps typing.
    if (mobile) ta.current?.blur();
    else ta.current?.focus();
  };

  const stop = async () => {
    const conn = getConn(host);
    if (!conn) return;
    setStopping(true);
    try {
      await conn.request({ op: 'chat_interrupt', session });
    } catch (err) {
      toastError('中断失败', err);
      setStopping(false);
    }
  };

  const addFiles = async (files: FileList | File[]) => {
    const conn = getConn(host);
    if (!conn) return;
    for (const f of Array.from(files)) {
      if (f.size > MAX_UPLOAD) {
        useUi.getState().toast('error', `${f.name} 超过 20 MB，无法作为附件`);
        continue;
      }
      const id = `${Date.now()}-${Math.random()}`;
      setAtts((a) => [...a, { id, name: f.name, uploading: true }]);
      try {
        const data = bytesToB64(new Uint8Array(await f.arrayBuffer()));
        const res = await call(conn, { op: 'upload_temp', name: f.name, data }, 'path');
        setAtts((a) => a.map((x) => (x.id === id ? { ...x, path: res.path, uploading: false } : x)));
      } catch (err) {
        setAtts((a) => a.filter((x) => x.id !== id));
        toastError(`上传 ${f.name} 失败`, err);
      }
    }
  };

  return (
    <div className="mx-auto w-full max-w-3xl px-3 pb-3 md:px-6">
      <div
        className={cx('rounded-lg border border-line-strong bg-bg shadow-[0_1px_2px_rgb(0_0_0/0.04)] focus-within:border-accent', disabled && 'opacity-60')}
        onDragOver={(e) => e.preventDefault()}
        onDrop={(e) => {
          e.preventDefault();
          if (e.dataTransfer.files.length) void addFiles(e.dataTransfer.files);
        }}
      >
        {atts.length > 0 && (
          <div className="flex flex-wrap gap-1.5 px-2.5 pt-2.5">
            {atts.map((a) => (
              <span key={a.id} className="inline-flex h-7 max-w-[240px] items-center gap-1.5 rounded-md border border-line bg-panel pr-1 pl-2 text-[12.5px]">
                {a.uploading ? <Spinner size={12} /> : <Paperclip size={12} className="shrink-0 text-muted" />}
                <span className="truncate">{a.name}</span>
                <button type="button" title="移除" aria-label="移除附件" onClick={() => setAtts((x) => x.filter((y) => y.id !== a.id))} className="rounded p-0.5 text-faint hover:text-fg">
                  <X size={12} />
                </button>
              </span>
            ))}
          </div>
        )}
        <textarea
          ref={ta}
          rows={1}
          value={text}
          disabled={disabled}
          onChange={(e) => {
            setText(e.target.value);
            drafts.set(key, e.target.value);
          }}
          onKeyDown={(e) => {
            if (e.key === 'Enter' && !e.shiftKey && !mobile && !e.nativeEvent.isComposing) {
              e.preventDefault();
              send();
            }
          }}
          onPaste={(e) => {
            const files = Array.from(e.clipboardData.files);
            if (files.length) {
              e.preventDefault();
              void addFiles(files);
            }
          }}
          placeholder={working ? '继续补充或引导…' : '输入消息'}
          aria-label="消息"
          enterKeyHint={mobile ? 'enter' : 'send'}
          className="block max-h-[220px] min-h-[44px] w-full resize-none bg-transparent px-3 pt-2.5 pb-1 text-[14.5px] leading-6 placeholder:text-faint focus:outline-none"
        />
        <div className="flex min-w-0 items-center gap-1 px-1.5 pb-1.5">
          <input
            ref={file}
            type="file"
            multiple
            hidden
            onChange={(e) => {
              if (e.target.files) void addFiles(e.target.files);
              e.target.value = '';
            }}
          />
          <IconButton label="添加附件" size="sm" disabled={disabled} onClick={() => file.current?.click()}>
            <Paperclip size={16} />
          </IconButton>
          {tools}
          <div className="flex-1" />
          {working && (
            <IconButton label="停止" size="sm" onClick={stop} disabled={stopping} className={cx('border border-line-strong text-fg', !typed && 'max-md:w-auto max-md:gap-1.5 max-md:px-3')}>
              {stopping ? <Spinner size={14} /> : <Square size={13} fill="currentColor" />}
              {!typed && <span className="text-[13px] font-medium md:hidden">停止</span>}
            </IconButton>
          )}
          {(!working || typed || !mobile) && (
            <button
              type="button"
              title="发送"
              aria-label="发送"
              disabled={!canSend}
              // Keep the textarea focused on desktop. Not on pointerdown: WebKit then drops the
              // click of a touch tap.
              onMouseDown={(e) => e.preventDefault()}
              onClick={send}
              className="inline-flex h-8 w-8 shrink-0 items-center justify-center rounded-md bg-accent text-accent-fg transition-opacity disabled:opacity-30 max-md:h-11 max-md:w-11"
            >
              <ArrowUp size={17} />
            </button>
          )}
        </div>
      </div>
    </div>
  );
}
