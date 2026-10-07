import { useCallback, useEffect, useMemo, useState } from 'react';
import { Link2, QrCode, ShieldCheck } from 'lucide-react';
import { isPairExpired, PairParseError, parsePairPayload } from '../../lib/pair';
import { loadWasm } from '../../net/wasm';
import type { PairPayload } from '../../proto/generated/PairPayload';
import { isIos, isStandalone } from '../../push';
import { useHosts } from '../../store/hosts';
import { useUi } from '../../store/ui';
import { navigate } from '../router';
import { Button, cx, inputClass, Sheet } from '../ui/primitives';
import { QrScanner } from './QrScanner';

/** Paste-link input + camera scan. Calls `onPayload` with the raw link text. */
export function PairEntry({ onPayload, compact }: { onPayload: (text: string) => void; compact?: boolean }) {
  const [text, setText] = useState('');
  const [scan, setScan] = useState(false);
  const onScan = useCallback(
    (t: string) => {
      setScan(false);
      onPayload(t);
    },
    [onPayload],
  );
  return (
    <div className={cx('flex w-full flex-col gap-3', !compact && 'max-w-md')}>
      <form
        className="flex gap-2"
        onSubmit={(e) => {
          e.preventDefault();
          if (text.trim()) onPayload(text.trim());
        }}
      >
        <input
          className={inputClass}
          value={text}
          onChange={(e) => setText(e.target.value)}
          placeholder="粘贴配对链接"
          autoCapitalize="off"
          autoCorrect="off"
          spellCheck={false}
          aria-label="配对链接"
        />
        <Button type="submit" variant="primary" className="shrink-0" disabled={!text.trim()} icon={<Link2 size={15} />}>
          配对
        </Button>
      </form>
      {scan ? (
        <div className="flex flex-col gap-2">
          <QrScanner onResult={onScan} />
          <Button variant="ghost" onClick={() => setScan(false)}>
            取消扫描
          </Button>
        </div>
      ) : (
        <Button icon={<QrCode size={15} />} onClick={() => setScan(true)}>
          扫描二维码
        </Button>
      )}
    </div>
  );
}

/** Confirm sheet driven by `useUi.pairInput` (from `#pair=` or the entry form). */
export function PairSheet() {
  const input = useUi((s) => s.pairInput);
  const close = useUi((s) => s.closePair);
  const pair = useHosts((s) => s.pair);
  const known = useHosts((s) => s.hosts);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();
  const [fp, setFp] = useState<string>();

  const parsed = useMemo((): { payload?: PairPayload; error?: string } => {
    if (!input) return {};
    try {
      return { payload: parsePairPayload(input) };
    } catch (e) {
      return { error: e instanceof PairParseError ? e.message : '配对链接无效' };
    }
  }, [input]);

  useEffect(() => {
    setError(undefined);
    setBusy(false);
    setFp(undefined);
    if (!parsed.payload) return;
    const host = parsed.payload.host;
    loadWasm()
      .then((w) => setFp(w.fingerprint(host)))
      .catch(() => setFp(undefined));
  }, [parsed.payload]);

  const p = parsed.payload;
  const expired = p ? isPairExpired(p) : false;
  const already = p ? known.some((h) => h.host === p.host) : false;

  const clearHash = () => {
    if (location.hash.startsWith('#pair=')) history.replaceState(null, '', location.pathname + location.search + '#/');
  };
  const dismiss = () => {
    clearHash();
    close();
  };

  const go = async () => {
    if (!p) return;
    setBusy(true);
    setError(undefined);
    try {
      const hello = await pair(p);
      clearHash();
      close();
      useUi.getState().toast('success', `已与 ${hello.host_name || p.host_name} 配对`);
      navigate({ name: 'home' }, true);
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Sheet
      open={input !== undefined}
      onClose={dismiss}
      title="配对新主机"
      width={460}
      footer={
        p && !expired ? (
          <>
            <Button onClick={dismiss}>取消</Button>
            <Button variant="primary" busy={busy} onClick={go} icon={<ShieldCheck size={15} />}>
              配对
            </Button>
          </>
        ) : (
          <Button onClick={dismiss}>关闭</Button>
        )
      }
    >
      {parsed.error && <p className="text-sm text-danger">{parsed.error}</p>}
      {p && (
        <div className="flex flex-col gap-4">
          <dl className="grid grid-cols-[72px_minmax(0,1fr)] gap-x-3 gap-y-2.5 text-sm">
            <dt className="text-muted">主机</dt>
            <dd className="truncate font-medium">{p.host_name || '未命名主机'}</dd>
            <dt className="text-muted">指纹</dt>
            <dd className="font-mono text-[13px] break-all">{fp ?? p.host.slice(0, 16) + '…'}</dd>
            <dt className="text-muted">中继</dt>
            <dd className="font-mono text-[13px] break-all">{p.relay}</dd>
          </dl>
          {expired && (
            <p className="rounded-md border border-danger/30 bg-danger-soft px-3 py-2 text-sm text-danger">
              配对码已过期，请在主机上运行 <code className="font-mono">yonder pair</code> 生成新的二维码。
            </p>
          )}
          {already && !expired && <p className="text-sm text-muted">此主机已配对，继续将重新授权。</p>}
          {/* iOS keeps Home Screen web app storage apart from Safari, and pairing codes are single-use. */}
          {!expired && isIos() && !isStandalone() && (
            <p className="rounded-md border border-line bg-panel px-3 py-2 text-[13px] text-muted">
              主屏幕 App 与 Safari 的数据分开保存，配对码只能用一次。需要通知的话，请先添加到主屏幕，再在 App 里扫码配对。
            </p>
          )}
          {!expired && <p className="text-[13px] text-muted">核对指纹与主机上显示的一致后再配对。</p>}
          {error && <p className="rounded-md border border-danger/30 bg-danger-soft px-3 py-2 text-sm break-words text-danger">{error}</p>}
        </div>
      )}
    </Sheet>
  );
}
