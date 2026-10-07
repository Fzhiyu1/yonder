import { lazy, Suspense, useEffect } from 'react';
import { Plus, WifiOff } from 'lucide-react';
import { getProvider } from '../net/provider';
import { describeError } from '../net/types';
import { syncPushTo } from '../push';
import { hostLabel, onHostOnline, useHosts } from '../store/hosts';
import { useUi } from '../store/ui';
import { loadSettings } from '../storage';
import { FileManager } from './files/FileManager';
import { NewSessionDialog } from './NewSession';
import { ConfirmDialog, Toasts } from './Overlays';
import { PairEntry, PairSheet } from './pairing/Pairing';
import { navigate, navigateUrl, useRoute, type Route } from './router';
import { HostInfoView, SettingsView } from './Settings';
import { Sidebar } from './Sidebar';
import { useApplyTheme } from './theme';
import { Button, EmptyState, Spinner, useIsDesktop } from './ui/primitives';

const ChatView = lazy(() => import('./chat/ChatView').then((m) => ({ default: m.ChatView })));
const TerminalView = lazy(() => import('./terminal/TerminalView').then((m) => ({ default: m.TerminalView })));
const HistoryView = lazy(() => import('./history/HistoryView').then((m) => ({ default: m.HistoryView })));

const loadingView = (
  <div className="flex h-full items-center justify-center gap-2 text-sm text-muted">
    <Spinner /> 正在加载
  </div>
);

function ConnectionBanner({ host }: { host: string }) {
  const h = useHosts((s) => s.hosts.find((x) => x.host === host));
  const rt = useHosts((s) => s.runtime[host]);
  if (!h || !rt || rt.status === 'online') return null;
  const name = hostLabel(h, rt);
  const text =
    rt.status === 'connecting' ? `正在连接 ${name}…` : rt.status === 'error' ? `${name}：${describeError(rt.error)}` : `${name} 离线，恢复后自动重连`;
  return (
    <div
      role="status"
      className={`flex shrink-0 items-center gap-2 border-b px-4 py-1.5 text-[13px] ${rt.status === 'error' ? 'border-danger/30 bg-danger-soft text-danger' : 'border-warn/30 bg-warn-soft text-warn'}`}
    >
      {rt.status === 'connecting' ? <Spinner size={13} /> : <WifiOff size={14} />}
      <span className="min-w-0 flex-1 truncate">{text}</span>
      {rt.status !== 'connecting' && (
        <button type="button" className="shrink-0 rounded px-2 py-0.5 font-medium hover:bg-bg/50" onClick={() => getProvider().get(host)?.wake()}>
          重试
        </button>
      )}
    </div>
  );
}

function SessionRoute({ host, session }: { host: string; session: string }) {
  const rt = useHosts((s) => s.runtime[host]);
  const known = useHosts((s) => s.hosts.some((h) => h.host === host));
  const s = rt?.sessions[session];
  if (!known) return <EmptyState title="未知主机" />;
  if (!s) {
    if (!rt?.sessionsLoaded)
      return (
        <div className="flex h-full items-center justify-center gap-2 text-sm text-muted">
          <Spinner /> 正在连接
        </div>
      );
    return <EmptyState title="会话不存在或已删除" />;
  }
  return (
    <Suspense fallback={loadingView}>
      {s.kind === 'chat' ? <ChatView key={`${host}/${s.id}`} host={host} s={s} /> : <TerminalView key={`${host}/${s.id}`} host={host} s={s} />}
    </Suspense>
  );
}

function Home() {
  const hosts = useHosts((s) => s.hosts);
  const loaded = useHosts((s) => s.loaded);
  if (!loaded) return null;
  if (!hosts.length)
    return (
      <div className="flex h-full flex-col items-center justify-center gap-4 p-6">
        <h1 className="text-[15px] font-semibold">配对主机</h1>
        <PairEntry onPayload={(t) => useUi.getState().openPair(t)} />
      </div>
    );
  return (
    <EmptyState title="选择一个会话">
      <Button className="mt-2" variant="primary" icon={<Plus size={15} />} onClick={() => useUi.getState().openNewSession({})}>
        新建会话
      </Button>
    </EmptyState>
  );
}

function Main({ route }: { route: Route }) {
  switch (route.name) {
    case 'session':
      return <SessionRoute host={route.host} session={route.session} />;
    case 'files':
      return <FileManager key={route.host} host={route.host} path={route.path} />;
    case 'host':
      return <HostInfoView host={route.host} />;
    case 'history':
      return (
        <Suspense fallback={loadingView}>
          <HistoryView key={route.host} host={route.host} />
        </Suspense>
      );
    case 'settings':
      return <SettingsView />;
    default:
      return <Home />;
  }
}

/** Tell hosts which session is visible so they can skip notifications for it. */
function useFocusReporting(route: Route) {
  const hosts = useHosts((s) => s.hosts);
  const runtime = useHosts((s) => s.runtime);
  useEffect(() => {
    const report = (e?: { type: string }) => {
      const leaving = e?.type === 'pagehide' || e?.type === 'freeze';
      const visible = !leaving && document.visibilityState === 'visible' && document.hasFocus();
      for (const h of hosts) {
        const conn = getProvider().get(h.host);
        if (!conn || conn.status !== 'online') continue;
        const focused = visible && route.name === 'session' && route.host === h.host ? route.session : undefined;
        conn.sendFocus(focused);
      }
    };
    report();
    // Hosts forget a focus that is not repeated (a suspended iPhone cannot report leaving), so
    // keep repeating it while the session is on screen.
    const repeat = window.setInterval(() => document.visibilityState === 'visible' && report(), 10_000);
    document.addEventListener('visibilitychange', report);
    // iOS suspends a backgrounded page without always firing visibilitychange first; pagehide
    // and freeze are the last chances to tell hosts the session is no longer on screen, so
    // they notify again instead of assuming the user is still looking.
    window.addEventListener('pagehide', report);
    document.addEventListener('freeze', report);
    window.addEventListener('focus', report);
    window.addEventListener('blur', report);
    return () => {
      window.clearInterval(repeat);
      document.removeEventListener('visibilitychange', report);
      window.removeEventListener('pagehide', report);
      document.removeEventListener('freeze', report);
      window.removeEventListener('focus', report);
      window.removeEventListener('blur', report);
    };
    // Re-run when links come online so the host learns the current focus.
  }, [route, hosts, Object.values(runtime).map((r) => r.status).join()]);
}

export function App() {
  useApplyTheme();
  const route = useRoute();
  const desktop = useIsDesktop();
  const init = useHosts((s) => s.init);

  useEffect(() => {
    void loadSettings().then((s) => useUi.setState({ settings: s }));
    void init();
    const off = onHostOnline((_h, conn) => void syncPushTo(conn));
    const checkPair = () => {
      if (location.hash.startsWith('#pair=')) useUi.getState().openPair(location.hash);
    };
    checkPair();
    window.addEventListener('hashchange', checkPair);
    const onSw = (e: MessageEvent) => {
      if (e.data?.type === 'open' && typeof e.data.url === 'string') navigateUrl(e.data.url);
    };
    navigator.serviceWorker?.addEventListener('message', onSw);
    return () => {
      off();
      window.removeEventListener('hashchange', checkPair);
      navigator.serviceWorker?.removeEventListener('message', onSw);
    };
  }, [init]);

  useFocusReporting(route);

  const routeHost = route.name === 'session' || route.name === 'files' || route.name === 'host' || route.name === 'history' ? route.host : undefined;
  const inDetail = route.name !== 'home';

  return (
    <div className="safe-x flex h-full min-h-0 w-full overflow-hidden bg-bg">
      {(desktop || !inDetail) && (
        <div className={desktop ? 'h-full w-[272px] shrink-0 border-r border-line' : 'h-full w-full'}>
          <Sidebar />
        </div>
      )}
      {(desktop || inDetail) && (
        <main className="flex h-full min-w-0 flex-1 flex-col">
          {routeHost && <ConnectionBanner host={routeHost} />}
          <div className="min-h-0 flex-1">
            <Main route={route} />
          </div>
        </main>
      )}
      <NewSessionDialog />
      <PairSheet />
      <ConfirmDialog />
      <Toasts />
    </div>
  );
}

export { navigate };
