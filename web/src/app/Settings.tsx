import { useEffect, useState, type ReactNode } from 'react';
import { ArrowLeft, Bell, BellOff, Check, Minus, Pencil, Plus, Send, ShieldX, Trash2 } from 'lucide-react';
import { formatDateTime, relativeTime } from '../lib/format';
import { getProvider } from '../net/provider';
import { call, describeError, type HostConnection } from '../net/types';
import { loadWasm } from '../net/wasm';
import type { DeviceInfo } from '../proto/generated/DeviceInfo';
import { disablePush, enablePush, isIos, isStandalone, pushState, pushSupport } from '../push';
import { getDeviceName, getDevicePublic, setDeviceName } from '../storage';
import { hostLabel, useHosts } from '../store/hosts';
import { toastError, useUi } from '../store/ui';
import type { StoredHost } from '../storage';
import { PairEntry } from './pairing/Pairing';
import { navigate } from './router';
import { OS_LABEL, OsIcon } from './ui/icons';
import { Button, cx, IconButton, inputClass, Segmented, Spinner, StatusDot, useIsDesktop } from './ui/primitives';


export function Section({ title, children, action }: { title: string; children: ReactNode; action?: ReactNode }) {
  return (
    <section className="border-b border-line py-5 last:border-b-0">
      <div className="mb-3 flex items-center gap-2">
        <h2 className="flex-1 text-[13px] font-semibold text-muted">{title}</h2>
        {action}
      </div>
      {children}
    </section>
  );
}

export function Row({ label, children, sub }: { label: ReactNode; children?: ReactNode; sub?: ReactNode }) {
  return (
    <div className="flex min-h-11 items-center gap-3 py-1.5">
      <div className="min-w-0 flex-1">
        <div className="truncate text-[14px]">{label}</div>
        {sub && <div className="text-[12.5px] break-all text-muted">{sub}</div>}
      </div>
      {children && <div className="flex shrink-0 items-center gap-2">{children}</div>}
    </div>
  );
}

function EditableName({ value, onSave, placeholder }: { value: string; onSave: (v: string) => Promise<void> | void; placeholder?: string }) {
  const [editing, setEditing] = useState(false);
  const [v, setV] = useState(value);
  useEffect(() => setV(value), [value]);
  if (!editing)
    return (
      <span className="flex min-w-0 items-center gap-1">
        <span className="truncate">{value || placeholder}</span>
        <IconButton label="重命名" size="sm" onClick={() => setEditing(true)}>
          <Pencil size={14} />
        </IconButton>
      </span>
    );
  return (
    <form
      className="flex min-w-0 items-center gap-1"
      onSubmit={async (e) => {
        e.preventDefault();
        await onSave(v.trim());
        setEditing(false);
      }}
    >
      <input autoFocus className={cx(inputClass, 'h-8')} value={v} onChange={(e) => setV(e.target.value)} placeholder={placeholder} />
      <IconButton label="保存" size="sm" type="submit">
        <Check size={15} />
      </IconButton>
    </form>
  );
}

function DeviceSection() {
  const [name, setName] = useState('');
  const [fp, setFp] = useState('');
  useEffect(() => {
    void getDeviceName().then(setName);
    void (async () => {
      const w = await loadWasm();
      setFp(w.fingerprint(await getDevicePublic()));
    })().catch(() => setFp(''));
  }, []);
  return (
    <Section title="本设备">
      <Row label={<EditableName value={name} placeholder="设备名称" onSave={async (v) => { await setDeviceName(v); setName(v || (await getDeviceName())); useUi.getState().toast('success', '设备名称会在下次连接时发送给主机'); }} />} sub="配对时显示在主机上的名称" />
      <Row label="指纹" sub={<span className="font-mono">{fp || '…'}</span>} />
    </Section>
  );
}

function DevicesOf({ conn, name }: { conn: HostConnection; name: string }) {
  const [devices, setDevices] = useState<DeviceInfo[]>();
  const [error, setError] = useState<string>();
  const load = () =>
    call(conn, { op: 'list_devices' }, 'devices')
      .then((r) => setDevices(r.devices))
      .catch((e) => setError((e as Error).message));
  useEffect(() => {
    void load();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [conn]);
  const revoke = async (d: DeviceInfo) => {
    const ok = await useUi.getState().ask({
      title: '撤销设备',
      message: `「${d.name}」将无法再连接 ${name}，需要重新配对才能使用。`,
      confirmLabel: '撤销',
      destructive: true,
    });
    if (!ok) return;
    try {
      await conn.request({ op: 'revoke_device', device: d.public });
      await load();
    } catch (err) {
      toastError('撤销失败', err);
    }
  };
  return (
    <div className="mb-3">
      <div className="mb-1 text-[13px] font-medium">{name}</div>
      {error && <div className="text-[13px] text-danger">{error}</div>}
      {!devices && !error && <Spinner size={13} className="text-muted" />}
      <div className="divide-y divide-line rounded-lg border border-line">
        {devices?.map((d) => (
          <div key={d.public} className="flex min-h-12 items-center gap-3 px-3 py-2">
            <div className="min-w-0 flex-1">
              <div className="flex min-w-0 items-center gap-2 text-[14px]">
                <span className="truncate">{d.name}</span>
                {d.current && <span className="shrink-0 rounded border border-accent/40 bg-accent-soft px-1.5 text-[11px] leading-[18px] text-accent">本设备</span>}
              </div>
              <div className="truncate text-[12px] text-muted">
                {d.client} · {d.permissions.join(', ') || '无权限'} · 配对于 {formatDateTime(d.paired_at)}
                {d.last_seen ? ` · ${relativeTime(d.last_seen) === '刚刚' ? '刚刚活跃' : `${relativeTime(d.last_seen)}前活跃`}` : ''}
              </div>
            </div>
            {!d.current && (
              <IconButton label="撤销" onClick={() => revoke(d)} className="hover:text-danger">
                <ShieldX size={16} />
              </IconButton>
            )}
          </div>
        ))}
      </div>
    </div>
  );
}

export function HostRow({ h }: { h: StoredHost }) {
  const rt = useHosts((s) => s.runtime[h.host]);
  const setLabel = useHosts((s) => s.setLabel);
  const forget = useHosts((s) => s.forget);
  const [fp, setFp] = useState(rt?.info?.fingerprint ?? '');
  useEffect(() => {
    if (rt?.info?.fingerprint) setFp(rt.info.fingerprint);
    else
      void loadWasm()
        .then((w) => setFp(w.fingerprint(h.host)))
        .catch(() => setFp(`${h.host.slice(0, 16)}…`));
  }, [h.host, rt?.info?.fingerprint]);
  const status = rt?.status ?? 'connecting';
  const stateText = status === 'online' ? '在线' : status === 'connecting' ? '连接中' : status === 'error' ? describeError(rt?.error) : '离线';
  return (
    <div className="flex flex-col gap-1 rounded-lg border border-line px-3 py-2.5">
      <div className="flex min-w-0 items-center gap-2">
        <StatusDot status={status} />
        <OsIcon os={rt?.info?.os ?? h.os} className="shrink-0 text-muted" />
        <div className="min-w-0 flex-1 text-[14px] font-medium">
          <EditableName value={hostLabel(h, rt)} onSave={(v) => setLabel(h.host, v)} />
        </div>
        <Button
          size="sm"
          variant="danger-soft"
          icon={<Trash2 size={14} />}
          onClick={async () => {
            const ok = await useUi.getState().ask({ title: '忘记此主机', message: `将从本设备移除 ${hostLabel(h, rt)}。主机上的授权需在主机上撤销。`, confirmLabel: '忘记', destructive: true });
            if (ok) await forget(h.host);
          }}
        >
          忘记
        </Button>
      </div>
      <dl className="grid grid-cols-[48px_minmax(0,1fr)] gap-x-2 gap-y-0.5 text-[12.5px]">
        <dt className="text-muted">状态</dt>
        <dd className={cx('break-words', status === 'error' && 'text-danger')}>{stateText}</dd>
        <dt className="text-muted">指纹</dt>
        <dd className="font-mono [overflow-wrap:anywhere]">{fp}</dd>
        <dt className="text-muted">中继</dt>
        <dd className="font-mono break-all">{h.relay}</dd>
      </dl>
    </div>
  );
}

function NotifySection() {
  const hosts = useHosts((s) => s.hosts);
  const runtime = useHosts((s) => s.runtime);
  const [state, setState] = useState<'on' | 'off' | 'denied' | 'loading'>('loading');
  const [busy, setBusy] = useState(false);
  const support = typeof window !== 'undefined' ? pushSupport() : 'unsupported';
  const conns = () => hosts.map((h) => getProvider().get(h.host)).filter((c): c is HostConnection => !!c);
  const refresh = () => void pushState().then(setState).catch(() => setState('off'));
  useEffect(refresh, []);

  const enable = async () => {
    setBusy(true);
    try {
      const r = await enablePush(conns());
      if (r.failed.length) useUi.getState().toast('warning', `已开启，但以下主机未订阅：${r.failed.join('、')}`);
      else useUi.getState().toast('success', `已为 ${r.ok} 台主机开启通知`);
    } catch (err) {
      toastError('开启通知失败', err);
    } finally {
      setBusy(false);
      refresh();
    }
  };
  const disable = async () => {
    setBusy(true);
    await disablePush(conns()).catch((err) => toastError('关闭通知失败', err));
    setBusy(false);
    refresh();
  };
  const online = hosts.filter((h) => runtime[h.host]?.status === 'online');

  return (
    <Section title="通知">
      {support === 'ios_needs_install' && (
        <p className="mb-2 rounded-md border border-line bg-panel px-3 py-2 text-[13px] text-muted">
          在 iOS 上需先用 Safari 的“共享 → 添加到主屏幕”，再从主屏幕打开 yonder 才能接收通知。
        </p>
      )}
      {support === 'insecure' && <p className="mb-2 text-[13px] text-muted">需要通过 HTTPS 访问才能开启通知。</p>}
      {support === 'unsupported' && <p className="mb-2 text-[13px] text-muted">此浏览器不支持 Web Push。</p>}
      <Row
        label="推送通知"
        sub={state === 'on' ? '审批请求、回合完成与会话结束时通知' : state === 'denied' ? '通知权限已被拒绝，请在系统设置中开启' : '未开启'}
      >
        {state === 'loading' ? (
          <Spinner size={14} />
        ) : state === 'on' ? (
          <Button size="sm" busy={busy} icon={<BellOff size={14} />} onClick={disable}>
            关闭
          </Button>
        ) : (
          <Button size="sm" variant="primary" busy={busy} disabled={support !== 'supported' || state === 'denied' || !hosts.length} icon={<Bell size={14} />} onClick={enable}>
            开启
          </Button>
        )}
      </Row>
      <Row label="发送测试通知" sub={online.length ? `发送到 ${online.length} 台在线主机配置的所有通道` : '没有在线主机'}>
        <Button
          size="sm"
          icon={<Send size={14} />}
          disabled={!online.length}
          onClick={async () => {
            let sent = 0;
            for (const h of online) {
              const c = getProvider().get(h.host);
              try {
                await c?.request({ op: 'notify_test' });
                sent++;
              } catch (err) {
                toastError(`${hostLabel(h, runtime[h.host])} 测试失败`, err);
              }
            }
            if (sent) useUi.getState().toast('success', `已从 ${sent} 台主机发出`);
          }}
        >
          发送
        </Button>
      </Row>
    </Section>
  );
}

function AppearanceSection() {
  const settings = useUi((s) => s.settings);
  const set = useUi((s) => s.setSettings);
  return (
    <Section title="外观">
      <Row label="主题">
        <Segmented
          size="sm"
          value={settings.theme}
          onChange={(v) => set({ theme: v })}
          options={[
            { value: 'system', label: '跟随系统' },
            { value: 'light', label: '浅色' },
            { value: 'dark', label: '深色' },
          ]}
        />
      </Row>
      <Row label="终端字号">
        <div className="flex items-center rounded-md border border-line">
          <IconButton label="减小" size="sm" disabled={settings.termFontSize <= 9} onClick={() => set({ termFontSize: settings.termFontSize - 1 })}>
            <Minus size={14} />
          </IconButton>
          <span className="w-10 text-center text-[13px] tabular-nums">{settings.termFontSize}</span>
          <IconButton label="增大" size="sm" disabled={settings.termFontSize >= 24} onClick={() => set({ termFontSize: settings.termFontSize + 1 })}>
            <Plus size={14} />
          </IconButton>
        </div>
      </Row>
    </Section>
  );
}

export function SettingsView() {
  const hosts = useHosts((s) => s.hosts);
  const runtime = useHosts((s) => s.runtime);
  const desktop = useIsDesktop();
  const onlineConns = hosts
    .filter((h) => runtime[h.host]?.status === 'online')
    .map((h) => ({ h, conn: getProvider().get(h.host) }))
    .filter((x): x is { h: StoredHost; conn: HostConnection } => !!x.conn);

  return (
    <div className="flex h-full min-h-0 flex-col">
      <header className="safe-top shrink-0 border-b border-line">
        <div className="flex h-12 items-center gap-1 px-2 md:px-4">
          {!desktop && (
            <IconButton label="返回" onClick={() => navigate({ name: 'home' })}>
              <ArrowLeft size={18} />
            </IconButton>
          )}
          <h1 className="pl-1 text-[15px] font-semibold">设置</h1>
        </div>
      </header>
      <div className="scroll-thin safe-bottom min-h-0 flex-1 overflow-y-auto">
        <div className="mx-auto w-full max-w-2xl px-4 md:px-6">
          <DeviceSection />
          <Section title="主机">
            <div className="flex flex-col gap-2">
              {hosts.map((h) => (
                <HostRow key={h.host} h={h} />
              ))}
              {!hosts.length && <div className="text-[13px] text-faint">尚未配对主机</div>}
            </div>
          </Section>
          <Section title="已配对设备">
            {onlineConns.map(({ h, conn }) => (
              <DevicesOf key={h.host} conn={conn} name={hostLabel(h, runtime[h.host])} />
            ))}
            {!onlineConns.length && <div className="text-[13px] text-faint">没有在线主机</div>}
          </Section>
          <NotifySection />
          <AppearanceSection />
          <Section title="配对新主机">
            <PairEntry onPayload={(t) => useUi.getState().openPair(t)} />
          </Section>
          <Section title="关于">
            <Row label="版本" sub={`yonder web ${__APP_VERSION__}`} />
            {isIos() && !isStandalone() && <Row label="安装到主屏幕" sub="Safari → 共享 → 添加到主屏幕" />}
          </Section>
        </div>
      </div>
    </div>
  );
}

export function HostInfoView({ host }: { host: string }) {
  const h = useHosts((s) => s.hosts.find((x) => x.host === host));
  const rt = useHosts((s) => s.runtime[host]);
  const desktop = useIsDesktop();
  const conn = getProvider().get(host);
  const info = rt?.info;
  if (!h) return null;
  return (
    <div className="flex h-full min-h-0 flex-col">
      <header className="safe-top shrink-0 border-b border-line">
        <div className="flex h-12 items-center gap-1 px-2 md:px-4">
          {!desktop && (
            <IconButton label="返回" onClick={() => navigate({ name: 'home' })}>
              <ArrowLeft size={18} />
            </IconButton>
          )}
          <h1 className="min-w-0 truncate pl-1 text-[15px] font-semibold">{hostLabel(h, rt)}</h1>
        </div>
      </header>
      <div className="scroll-thin safe-bottom min-h-0 flex-1 overflow-y-auto">
        <div className="mx-auto w-full max-w-2xl px-4 md:px-6">
          <Section title="连接">
            <HostRow h={h} />
          </Section>
          {info && (
            <Section title="系统">
              <dl className="grid grid-cols-[84px_minmax(0,1fr)] gap-x-3 gap-y-1.5 text-[13.5px]">
                <dt className="text-muted">主机名</dt>
                <dd className="break-all">{info.hostname}</dd>
                <dt className="text-muted">系统</dt>
                <dd>
                  {OS_LABEL[info.os] ?? info.os} · {info.arch}
                </dd>
                <dt className="text-muted">yonder</dt>
                <dd>{info.version}</dd>
                <dt className="text-muted">Shell</dt>
                <dd className="font-mono break-all">{info.shell}</dd>
                <dt className="text-muted">主目录</dt>
                <dd className="font-mono break-all">{info.home}</dd>
                <dt className="text-muted">文件根目录</dt>
                <dd className="font-mono break-all">{info.fs_roots.join('  ')}</dd>
                <dt className="text-muted">权限</dt>
                <dd>{info.permissions.map((p) => (p === 'sessions' ? '会话' : p === 'files' ? '文件' : p)).join('、') || '无'}</dd>
              </dl>
            </Section>
          )}
          {info && (
            <Section title="代理">
              <div className="divide-y divide-line rounded-lg border border-line">
                {info.agents.map((a) => (
                  <div key={a.agent} className="flex min-h-11 items-center gap-3 px-3 py-2 text-[13.5px]">
                    <span className="w-24 shrink-0 font-medium">{a.agent}</span>
                    <span className="min-w-0 flex-1 truncate text-muted">{a.available ? [a.version, a.chat ? '支持对话' : '仅终端', a.models.length ? `${a.models.length} 个模型` : ''].filter(Boolean).join(' · ') : '未安装'}</span>
                    <StatusDot status={a.available ? 'online' : 'offline'} />
                  </div>
                ))}
              </div>
            </Section>
          )}
          {conn && rt?.status === 'online' && (
            <Section title="已配对设备">
              <DevicesOf conn={conn} name={hostLabel(h, rt)} />
            </Section>
          )}
        </div>
      </div>
    </div>
  );
}
