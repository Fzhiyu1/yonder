import { useCallback, useEffect, useRef, useState } from 'react';
import { Terminal, type ITheme } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import { WebLinksAddon } from '@xterm/addon-web-links';
import { Unicode11Addon } from '@xterm/addon-unicode11';
import { MessageSquare } from 'lucide-react';
import { b64ToBytes, utf8Encode } from '../../lib/base64';
import { applyPtyOutput } from '../../lib/termOffset';
import { getConn } from '../../net/provider';
import { call } from '../../net/types';
import type { SessionInfo } from '../../proto/generated/SessionInfo';
import type { TerminalSnapshot } from '../../proto/generated/TerminalSnapshot';
import { useHosts } from '../../store/hosts';
import { toastError, useUi } from '../../store/ui';
import { canContinueAsChat, continueAsChat, resumeSession } from '../session/actions';
import { SessionHeader } from '../session/SessionHeader';
import { useResolvedTheme } from '../theme';
import { Button, Spinner, useIsMobile } from '../ui/primitives';
import { applyModifiers, KeyBar, type Modifiers } from './KeyBar';

const LIGHT: ITheme = {
  background: '#ffffff',
  foreground: '#1f1f23',
  cursor: '#1f1f23',
  cursorAccent: '#ffffff',
  selectionBackground: '#bfd3fb',
  black: '#1f1f23',
  red: '#c92a2a',
  green: '#2b8a3e',
  yellow: '#a36300',
  blue: '#1c5fd4',
  magenta: '#9c36b5',
  cyan: '#0b7285',
  white: '#6b6b74',
  brightBlack: '#71717a',
  brightRed: '#e03131',
  brightGreen: '#2f9e44',
  brightYellow: '#b87400',
  brightBlue: '#2563eb',
  brightMagenta: '#ae3ec9',
  brightCyan: '#1098ad',
  brightWhite: '#3f3f46',
};

const DARK: ITheme = {
  background: '#0f0f10',
  foreground: '#e4e4e7',
  cursor: '#e4e4e7',
  cursorAccent: '#0f0f10',
  selectionBackground: '#2c3e63',
  black: '#27272a',
  red: '#f87171',
  green: '#4ade80',
  yellow: '#facc15',
  blue: '#60a5fa',
  magenta: '#e879f9',
  cyan: '#22d3ee',
  white: '#d4d4d8',
  brightBlack: '#71717a',
  brightRed: '#fca5a5',
  brightGreen: '#86efac',
  brightYellow: '#fde047',
  brightBlue: '#93c5fd',
  brightMagenta: '#f0abfc',
  brightCyan: '#67e8f9',
  brightWhite: '#fafafa',
};

export function TerminalView({ host, s }: { host: string; s: SessionInfo }) {
  const container = useRef<HTMLDivElement>(null);
  const termRef = useRef<Terminal | null>(null);
  const fitRef = useRef<FitAddon | null>(null);
  const mobile = useIsMobile();
  const theme = useResolvedTheme();
  const fontSize = useUi((st) => st.settings.termFontSize);
  const [loading, setLoading] = useState(true);
  const [mods, setModsState] = useState<Modifiers>({ ctrl: false, alt: false });
  const [typing, setTyping] = useState(false);
  const modsRef = useRef(mods);
  const exited = s.state === 'exited' || s.state === 'failed';
  const exitedRef = useRef(exited);
  exitedRef.current = exited;
  // PTY size last reported by the host; we resize before input if ours differs.
  const ptySize = useRef({ cols: s.cols, rows: s.rows });
  const sendRef = useRef<(data: string) => void>(() => undefined);

  const setMods = (m: Modifiers) => {
    modsRef.current = m;
    setModsState(m);
  };

  useEffect(() => {
    const el = container.current;
    const conn = getConn(host);
    if (!el || !conn) return;
    const session = s.id;
    const term = new Terminal({
      fontFamily: 'ui-monospace, "SFMono-Regular", "SF Mono", Menlo, Consolas, "Liberation Mono", monospace',
      fontSize: useUi.getState().settings.termFontSize,
      lineHeight: 1.15,
      cursorBlink: true,
      scrollback: 5000,
      allowProposedApi: true,
      macOptionIsMeta: true,
      theme: document.documentElement.classList.contains('dark') ? DARK : LIGHT,
      disableStdin: exitedRef.current,
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.loadAddon(new WebLinksAddon((_e, uri) => window.open(uri, '_blank', 'noopener')));
    term.loadAddon(new Unicode11Addon());
    term.unicode.activeVersion = '11';
    term.open(el);
    termRef.current = term;
    fitRef.current = fit;
    // The hidden textarea xterm types into: its focus is the soft keyboard's state.
    const ta = term.textarea;
    const onFocus = () => setTyping(true);
    const onBlur = () => setTyping(false);
    ta?.addEventListener('focus', onFocus);
    ta?.addEventListener('blur', onBlur);

    let known = 0;
    let attached = false;
    let attaching = false;
    let disposed = false;
    let resizeTimer: ReturnType<typeof setTimeout> | undefined;

    const applySnapshot = (snap: TerminalSnapshot) => {
      if (snap.reset) term.reset();
      term.write(b64ToBytes(snap.data));
      known = snap.offset;
      ptySize.current = { cols: snap.cols, rows: snap.rows };
    };

    const sendResize = () => {
      if (disposed || exitedRef.current || !attached) return;
      const { cols, rows } = term;
      if (cols < 2 || rows < 2) return;
      ptySize.current = { cols, rows };
      conn.sendResize(session, cols, rows);
    };

    const doFit = () => {
      if (disposed || !el.offsetWidth || !el.offsetHeight) return;
      try {
        fit.fit();
      } catch {
        return;
      }
      clearTimeout(resizeTimer);
      resizeTimer = setTimeout(sendResize, 120);
    };

    const attach = async () => {
      if (attaching || disposed) return;
      attaching = true;
      try {
        const res = await call(conn, attached ? { op: 'attach', session, since: known } : { op: 'attach', session }, 'attached');
        if (disposed) return;
        useHosts.getState().upsertSession(host, res.session);
        if (res.terminal) applySnapshot(res.terminal);
        attached = true;
        setLoading(false);
        doFit();
        sendResize();
      } catch (err) {
        if (!disposed) {
          setLoading(false);
          toastError('连接终端失败', err);
        }
      } finally {
        attaching = false;
      }
    };

    const offEvent = conn.onEvent((e) => {
      if (disposed || !('session' in e) || e.session !== session) return;
      if (e.ev === 'pty_output') {
        if (attaching || !attached) return;
        const r = applyPtyOutput(known, e.offset, b64ToBytes(e.data));
        if (r.kind === 'write') {
          term.write(r.bytes);
          known = r.next;
        } else if (r.kind === 'gap') {
          void attach();
        }
      } else if (e.ev === 'pty_snapshot') {
        applySnapshot(e.snapshot);
      } else if (e.ev === 'pty_resized') {
        ptySize.current = { cols: e.cols, rows: e.rows };
      }
    });
    const offRe = conn.onReconnected(() => void attach());

    const send = (data: string) => {
      if (exitedRef.current || !attached) return;
      if (ptySize.current.cols !== term.cols || ptySize.current.rows !== term.rows) sendResize();
      conn.sendInput(session, utf8Encode(data));
    };
    sendRef.current = send;
    const onData = term.onData((d) => {
      const m = modsRef.current;
      if (m.ctrl || m.alt) {
        send(applyModifiers(d, m));
        setMods({ ctrl: false, alt: false });
      } else {
        send(d);
      }
    });

    const ro = new ResizeObserver(() => doFit());
    ro.observe(el);
    doFit();
    void attach();

    return () => {
      disposed = true;
      clearTimeout(resizeTimer);
      ro.disconnect();
      offEvent();
      offRe();
      onData.dispose();
      ta?.removeEventListener('focus', onFocus);
      ta?.removeEventListener('blur', onBlur);
      if (conn.status === 'online') void conn.request({ op: 'detach', session }).catch(() => undefined);
      term.dispose();
      termRef.current = null;
      fitRef.current = null;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [host, s.id]);

  useEffect(() => {
    const t = termRef.current;
    if (t) t.options.theme = theme === 'dark' ? DARK : LIGHT;
  }, [theme]);

  useEffect(() => {
    const t = termRef.current;
    if (!t) return;
    t.options.fontSize = fontSize;
    try {
      fitRef.current?.fit();
    } catch {
      /* not visible */
    }
  }, [fontSize]);

  useEffect(() => {
    const t = termRef.current;
    if (t) t.options.disableStdin = exited;
  }, [exited]);

  const paste = useCallback(async () => {
    try {
      const text = await navigator.clipboard.readText();
      if (text) termRef.current?.paste(text);
    } catch {
      useUi.getState().toast('warning', '无法读取剪贴板，请长按终端粘贴');
    }
    termRef.current?.focus();
  }, []);

  const bg = theme === 'dark' ? DARK.background : LIGHT.background;

  return (
    <div className="flex h-full min-h-0 flex-col">
      <SessionHeader host={host} s={s} />
      {exited && (
        <div className="flex shrink-0 flex-wrap items-center gap-2 border-b border-line bg-panel px-4 py-2 text-[13px] text-muted">
          <span className="min-w-0 flex-1">
            进程已退出{s.exit_code !== undefined ? `，退出码 ${s.exit_code}` : ''}，以下为只读回放
          </span>
          {s.agent_session && s.agent !== 'shell' && s.agent !== 'custom' && (
            <Button size="sm" icon={<MessageSquare size={14} />} onClick={() => void resumeSession(host, s)}>
              以对话恢复
            </Button>
          )}
        </div>
      )}
      {!exited && canContinueAsChat(s) && (
        <div className="flex shrink-0 items-center gap-2 border-b border-line bg-panel px-4 py-1.5 text-[13px] text-muted">
          <span className="min-w-0 flex-1 truncate">此代理会话可以切换到对话视图</span>
          <Button size="sm" variant="ghost" icon={<MessageSquare size={14} />} onClick={() => void continueAsChat(host, s)}>
            以对话继续
          </Button>
        </div>
      )}
      <div className="relative min-h-0 flex-1" style={{ background: bg }}>
        <div ref={container} className="term-host absolute top-1.5 right-0 bottom-0 left-2" onClick={() => termRef.current?.focus()} />
        {loading && (
          <div className="absolute inset-0 flex items-center justify-center gap-2 text-sm text-muted">
            <Spinner /> 正在连接终端
          </div>
        )}
      </div>
      {mobile && !exited && (
        <div className="safe-bottom shrink-0 bg-panel">
          <KeyBar
            mods={mods}
            setMods={setMods}
            send={(d) => sendRef.current(d)}
            paste={paste}
            keyboard={typing}
            toggleKeyboard={() => (typing ? termRef.current?.blur() : termRef.current?.focus())}
          />
        </div>
      )}
      {(!mobile || exited) && <div className="safe-bottom shrink-0" style={{ background: bg }} />}
    </div>
  );
}
