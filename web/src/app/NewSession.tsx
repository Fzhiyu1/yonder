import { useEffect, useLayoutEffect, useMemo, useState, type FocusEvent } from 'react';
import { ChevronDown, ChevronRight, FolderSearch, History, Play } from 'lucide-react';
import { relativeTime } from '../lib/format';
import { baseName } from '../lib/paths';
import { getConn } from '../net/provider';
import { call } from '../net/types';
import type { AgentKind } from '../proto/generated/AgentKind';
import type { AgentSessionSummary } from '../proto/generated/AgentSessionSummary';
import type { ApprovalMode } from '../proto/generated/ApprovalMode';
import type { SessionSpec } from '../proto/generated/SessionSpec';
import { hostLabel, useHosts } from '../store/hosts';
import { useUi } from '../store/ui';
import { FolderPicker } from './files/FolderPicker';
import { ModelButton, ModelPicker } from './ModelPicker';
import { navigate } from './router';
import { hasApprovals, initialMode, MODE_INFO, MODES } from './session/approval';
import { AGENT_LABEL, AgentIcon } from './ui/icons';
import { Button, cx, Field, inputClass, Segmented, Sheet, Spinner, useIsMobile } from './ui/primitives';

type Kind = 'chat' | 'terminal';

/** Split a command line into argv, honoring simple quotes. */
function splitArgs(cmd: string): string[] {
  const out: string[] = [];
  const re = /"([^"]*)"|'([^']*)'|(\S+)/g;
  let m: RegExpExecArray | null;
  while ((m = re.exec(cmd))) out.push(m[1] ?? m[2] ?? m[3]);
  return out;
}

/** iOS scrolls a focused field into the visible band only roughly; finish once the keyboard is up. */
function revealOnFocus(e: FocusEvent<HTMLElement>) {
  const el = e.currentTarget;
  setTimeout(() => el.scrollIntoView({ block: 'nearest', behavior: 'smooth' }), 320);
}

export function NewSessionDialog() {
  const preset = useUi((s) => s.newSession);
  const close = useUi((s) => s.closeNewSession);
  const remembered = useUi((s) => s.settings.approvalByAgent);
  const setSettings = useUi((s) => s.setSettings);
  const hosts = useHosts((s) => s.hosts);
  const runtime = useHosts((s) => s.runtime);
  const mobile = useIsMobile();

  const [host, setHost] = useState('');
  const [kind, setKind] = useState<Kind>('chat');
  const [agent, setAgent] = useState<AgentKind>('codex');
  const [command, setCommand] = useState('');
  const [cwd, setCwd] = useState('');
  const [model, setModel] = useState('');
  /** Picked in this dialog; otherwise the remembered pick or the host's configuration applies. */
  const [approvalPick, setApprovalPick] = useState<ApprovalMode>();
  const [resume, setResume] = useState('');
  const [showHistory, setShowHistory] = useState(false);
  const [prompt, setPrompt] = useState('');
  const [title, setTitle] = useState('');
  const [browse, setBrowse] = useState(false);
  const [pickModel, setPickModel] = useState(false);
  const [history, setHistory] = useState<AgentSessionSummary[]>();
  const [historyLoading, setHistoryLoading] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();

  const open = preset !== undefined;
  const rt = runtime[host];
  const info = rt?.info;
  const online = rt?.status === 'online';
  const agents = info?.agents ?? [];
  const agentInfo = agents.find((a) => a.agent === agent);
  const isAgent = agent === 'codex' || agent === 'claude' || agent === 'pi';
  const gated = kind === 'chat' && hasApprovals(agent);
  const hostDefault = agentInfo?.default_approval;
  const approval = approvalPick ?? initialMode(agent, agentInfo, remembered ?? {});

  // Reset on open (before paint, so the previous host/kind never flashes).
  useLayoutEffect(() => {
    if (!preset) return;
    const firstOnline = hosts.find((h) => runtime[h.host]?.status === 'online')?.host ?? hosts[0]?.host ?? '';
    setHost(preset.host ?? firstOnline);
    setKind(preset.kind ?? 'chat');
    setCwd(preset.cwd ?? '');
    setPrompt('');
    setTitle('');
    setResume('');
    setCommand('');
    setApprovalPick(undefined);
    setShowHistory(false);
    setBrowse(false);
    setPickModel(false);
    setError(undefined);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [preset]);

  // Pick a valid agent when host/kind change.
  useEffect(() => {
    if (!open) return;
    const ok = (a: AgentKind) => {
      if (a === 'shell' || a === 'custom') return kind === 'terminal';
      const ai = agents.find((x) => x.agent === a);
      return !!ai?.available && (kind === 'terminal' || ai.chat);
    };
    if (!ok(agent)) {
      const first = agents.find((a) => a.available && (kind === 'terminal' || a.chat))?.agent;
      setAgent(first ?? (kind === 'terminal' ? 'shell' : 'codex'));
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, host, kind, info]);

  // Each opening and each host/agent change starts from that agent's default model.
  useEffect(() => {
    if (!open) return;
    setModel(agentInfo?.default_model ?? '');
    setPickModel(false);
    setResume('');
    setApprovalPick(undefined);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, agent, host]);

  // Host info can arrive after the dialog opened: fill in the default, never over a pick.
  useEffect(() => {
    const d = agentInfo?.default_model;
    if (d) setModel((m) => m || d);
  }, [agentInfo?.default_model]);

  // Resume candidates for agent + cwd.
  useEffect(() => {
    setHistory(undefined);
    if (!open || !online || !isAgent) return;
    const conn = getConn(host);
    if (!conn) return;
    let cancelled = false;
    setHistoryLoading(true);
    const t = setTimeout(() => {
      call(conn, { op: 'agent_history', agent, cwd: cwd.trim() || undefined, all: false }, 'agent_history')
        .then((r) => !cancelled && setHistory(r.sessions))
        .catch(() => !cancelled && setHistory([]))
        .finally(() => !cancelled && setHistoryLoading(false));
    }, 300);
    return () => {
      cancelled = true;
      clearTimeout(t);
    };
  }, [open, online, host, agent, cwd, isAgent]);

  const agentOptions = useMemo(() => {
    // The host lists its shell too; terminal-only kinds are added here, once, and hidden for chat.
    const list: Array<{ agent: AgentKind; disabled: boolean; note?: string }> = agents
      .filter((a) => a.agent !== 'shell' && a.agent !== 'custom')
      .map((a) => ({
        agent: a.agent,
        disabled: !a.available || (kind === 'chat' && !a.chat),
        note: !a.available ? '未安装' : kind === 'chat' && !a.chat ? '不支持对话' : a.version,
      }));
    if (kind === 'terminal') list.push({ agent: 'shell', disabled: false, note: info?.shell ? baseName(info.shell) : undefined }, { agent: 'custom', disabled: false });
    return list;
  }, [agents, kind, info?.shell]);

  const canCreate = online && !busy && (agent !== 'custom' || command.trim().length > 0) && !agentOptions.find((o) => o.agent === agent)?.disabled;

  const create = async () => {
    const conn = getConn(host);
    if (!conn || !canCreate) return;
    setBusy(true);
    setError(undefined);
    const spec: SessionSpec = {
      kind,
      agent,
      cwd: cwd.trim() || undefined,
      title: title.trim() || undefined,
    };
    if (agent === 'custom') spec.command = splitArgs(command.trim());
    if (isAgent) {
      if (model.trim()) spec.model = model.trim();
      if (resume) spec.resume = resume;
    }
    if (kind === 'chat') {
      // Unknown host default and nothing picked: let the host apply its own configuration.
      if (gated && (approvalPick ?? remembered?.[agent] ?? hostDefault)) spec.approval = approval;
      if (prompt.trim()) spec.prompt = prompt.trim();
    }
    try {
      const res = await call(conn, { op: 'create_session', spec }, 'session');
      useHosts.getState().upsertSession(host, res.session);
      close();
      navigate({ name: 'session', host, session: res.session.id });
    } catch (err) {
      setError((err as Error).message);
    } finally {
      setBusy(false);
    }
  };

  const models = agentInfo?.models ?? [];
  const pickApproval = (m: ApprovalMode) => {
    setApprovalPick(m);
    // An explicit pick is what the next chats of this agent start with, on every host.
    setSettings({ approvalByAgent: { ...(remembered ?? {}), [agent]: m } });
  };
  const resumeTitle = resume ? (history?.find((h) => h.id === resume)?.title ?? resume) : undefined;

  return (
    <Sheet
      open={open}
      onClose={close}
      title="新建会话"
      width={600}
      footer={
        <>
          {error && (
            <span role="alert" className="mr-auto min-w-0 text-[13px] break-words text-danger max-md:basis-full">
              {error}
            </span>
          )}
          {!mobile && <Button onClick={close}>取消</Button>}
          <Button variant="primary" disabled={!canCreate} busy={busy} icon={<Play size={14} />} onClick={create} className="max-md:flex-1">
            {resume ? '恢复会话' : '创建'}
          </Button>
        </>
      }
    >
      <form
        className="flex flex-col gap-4"
        onSubmit={(e) => {
          e.preventDefault();
          void create();
        }}
      >
        <div className="grid grid-cols-[minmax(0,1fr)_auto] gap-3 sm:gap-4">
          <Field label="主机">
            <select className={inputClass} value={host} onChange={(e) => setHost(e.target.value)}>
              {hosts.map((h) => {
                const r = runtime[h.host];
                return (
                  <option key={h.host} value={h.host} disabled={r?.status !== 'online'}>
                    {hostLabel(h, r)}
                    {r?.status !== 'online' ? '（离线）' : ''}
                  </option>
                );
              })}
            </select>
          </Field>
          <Field label="类型" group>
            <Segmented<Kind>
              value={kind}
              onChange={setKind}
              className="w-auto"
              options={[
                { value: 'chat', label: '对话' },
                { value: 'terminal', label: '终端' },
              ]}
            />
          </Field>
        </div>

        <div className="flex flex-col gap-1.5">
          <span className="text-[13px] font-medium text-muted">代理</span>
          {!info && online && (
            <div className="flex items-center gap-2 text-[13px] text-muted">
              <Spinner size={13} /> 正在读取主机信息
            </div>
          )}
          {!online && <div className="text-[13px] text-faint">主机不在线</div>}
          <div className="grid grid-cols-3 gap-1.5">
            {agentOptions.map((o) => (
              <button
                key={o.agent}
                type="button"
                disabled={o.disabled}
                aria-pressed={agent === o.agent}
                onClick={() => setAgent(o.agent)}
                className={cx(
                  'flex h-12 min-w-0 items-center gap-2 rounded-md border px-2.5 text-left transition-colors disabled:opacity-40 max-md:gap-1.5 max-md:px-2',
                  agent === o.agent ? 'border-accent bg-accent-soft' : 'border-line-strong hover:bg-hover',
                )}
              >
                <AgentIcon agent={o.agent} kind={kind} size={16} className={cx('shrink-0', agent === o.agent ? 'text-accent' : 'text-muted')} />
                <span className="min-w-0 flex-1">
                  <span className="block truncate text-[13.5px] font-medium">{o.agent === 'claude' && mobile ? 'Claude' : AGENT_LABEL[o.agent]}</span>
                  {o.note && <span className="block truncate text-[12px] leading-4 text-faint">{o.note}</span>}
                </span>
              </button>
            ))}
          </div>
        </div>

        {agent === 'custom' && (
          <Field label="命令">
            <input className={cx(inputClass, 'font-mono')} value={command} onChange={(e) => setCommand(e.target.value)} placeholder="htop" autoCapitalize="off" autoCorrect="off" spellCheck={false} />
          </Field>
        )}

        <div className="flex flex-col gap-1.5">
          <span className="text-[13px] font-medium text-muted">工作目录</span>
          <div className="flex gap-2">
            <input
              className={cx(inputClass, 'font-mono')}
              value={cwd}
              onChange={(e) => setCwd(e.target.value)}
              onFocus={revealOnFocus}
              placeholder={info?.home ?? '主目录'}
              autoCapitalize="off"
              autoCorrect="off"
              spellCheck={false}
              aria-label="工作目录"
            />
            <Button icon={<FolderSearch size={15} />} disabled={!online} onClick={() => setBrowse((b) => !b)} title="浏览文件夹" aria-label="浏览文件夹" />
          </div>
          {browse && (
            <FolderPicker
              host={host}
              start={cwd.trim() || undefined}
              sep={info?.path_sep ?? '/'}
              onClose={() => setBrowse(false)}
              onPick={(p) => {
                setCwd(p);
                setBrowse(false);
              }}
            />
          )}
          {!!info?.recent_dirs.length && (
            <div className="flex flex-wrap gap-1.5">
              {info.recent_dirs.slice(0, 6).map((d) => (
                <button
                  key={d}
                  type="button"
                  title={d}
                  onClick={() => setCwd(d)}
                  className={cx(
                    'h-7 max-w-[200px] truncate rounded-md border px-2 font-mono text-[12px] max-md:h-10 max-md:px-2.5 max-md:text-[13px]',
                    cwd === d ? 'border-accent bg-accent-soft text-accent' : 'border-line text-muted hover:bg-hover hover:text-fg',
                  )}
                >
                  {baseName(d, info.path_sep)}
                </button>
              ))}
            </div>
          )}
        </div>

        {gated && (
          <Field
            label="审批"
            group
            hint={
              <>
                {MODE_INFO[approval].hint}
                {hostDefault === approval && '（主机默认）'}
              </>
            }
          >
            <Segmented<ApprovalMode>
              value={approval}
              onChange={pickApproval}
              className="w-full"
              options={MODES.map((m) => {
                const Icon = MODE_INFO[m].icon;
                return { value: m, title: MODE_INFO[m].hint, label: <><Icon size={14} className={approval === m ? MODE_INFO[m].tone : undefined} />{MODE_INFO[m].label}</> };
              })}
            />
          </Field>
        )}

        {kind === 'chat' && (
          <Field label="首条消息">
            <textarea
              className={cx(inputClass, 'h-auto min-h-20 py-2 leading-6')}
              rows={3}
              value={prompt}
              onChange={(e) => setPrompt(e.target.value)}
              onFocus={revealOnFocus}
              placeholder="可选"
            />
          </Field>
        )}

        {isAgent && (
          <div className="flex flex-col gap-2">
            <Field label="模型" group>
              <ModelButton value={model} defaultModel={agentInfo?.default_model} open={pickModel} disabled={!online} onClick={() => setPickModel((p) => !p)} />
            </Field>
            {pickModel && (
              <ModelPicker
                models={models}
                value={model}
                defaultModel={agentInfo?.default_model}
                loading={!info}
                onClose={() => setPickModel(false)}
                onPick={(m) => {
                  setModel(m);
                  setPickModel(false);
                }}
              />
            )}
          </div>
        )}

        {isAgent && (
          <div className="flex flex-col gap-1.5">
            <button
              type="button"
              aria-expanded={showHistory}
              onClick={() => setShowHistory((v) => !v)}
              className="-mx-1 flex h-9 min-w-0 items-center gap-1.5 rounded-md px-1 text-left text-[13px] font-medium text-muted hover:text-fg max-md:h-11"
            >
              {showHistory ? <ChevronDown size={14} /> : <ChevronRight size={14} />}
              <span className="shrink-0">恢复历史会话</span>
              {historyLoading && <Spinner size={12} />}
              {!historyLoading && history && <span className="shrink-0 font-normal text-faint">{history.length}</span>}
              {resumeTitle && !showHistory && <span className="min-w-0 truncate font-normal text-accent">· {resumeTitle}</span>}
            </button>
            {showHistory && (
            <div className="scroll-thin flex max-h-52 flex-col gap-px overflow-y-auto overscroll-contain rounded-md border border-line p-1">
              <button
                type="button"
                onClick={() => setResume('')}
                className={cx('flex h-8 items-center gap-2 rounded px-2 text-left text-[13px] max-md:h-11', !resume ? 'bg-active' : 'hover:bg-hover')}
              >
                <Play size={13} className="shrink-0 text-muted" /> 新会话
              </button>
              {history?.map((h) => (
                <button
                  key={h.id}
                  type="button"
                  onClick={() => setResume(h.id)}
                  className={cx('flex h-8 min-w-0 items-center gap-2 rounded px-2 text-left text-[13px] max-md:h-11', resume === h.id ? 'bg-active' : 'hover:bg-hover')}
                >
                  <History size={13} className="shrink-0 text-muted" />
                  <span className="min-w-0 flex-1 truncate">{h.title || h.id}</span>
                  {h.updated_at && <span className="shrink-0 text-[12px] text-faint">{relativeTime(h.updated_at)}</span>}
                </button>
              ))}
              {history && !history.length && <div className="px-2 py-1 text-[12.5px] text-faint">没有可恢复的会话</div>}
            </div>
            )}
          </div>
        )}

        <Field label="标题">
          <input className={inputClass} value={title} onChange={(e) => setTitle(e.target.value)} onFocus={revealOnFocus} placeholder="可选" />
        </Field>
      </form>
    </Sheet>
  );
}
