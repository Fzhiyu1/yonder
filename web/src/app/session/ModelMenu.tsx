import { useState } from 'react';
import { ChevronDown, Cpu } from 'lucide-react';
import { getConn } from '../../net/provider';
import { call } from '../../net/types';
import type { SessionInfo } from '../../proto/generated/SessionInfo';
import { useHosts } from '../../store/hosts';
import { toastError, useUi } from '../../store/ui';
import { ModelPicker } from '../ModelPicker';
import { Sheet, Spinner } from '../ui/primitives';

/** Short label for a model id: drops a `provider/` prefix. */
const short = (m: string) => m.slice(m.lastIndexOf('/') + 1);

/** Model of a running chat, switchable in place (applies from the next turn). */
export function ModelMenu({ host, s, disabled }: { host: string; s: SessionInfo; disabled?: boolean }) {
  const agent = useHosts((st) => st.runtime[host]?.info?.agents.find((a) => a.agent === s.agent));
  const infoLoaded = useHosts((st) => !!st.runtime[host]?.info);
  const [open, setOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const current = s.model ?? agent?.default_model ?? '';

  const pick = async (model: string) => {
    setOpen(false);
    if (!model || model === current) return;
    const conn = getConn(host);
    if (!conn) return;
    setBusy(true);
    try {
      const res = await call(conn, { op: 'set_chat_model', session: s.id, model }, 'session');
      useHosts.getState().upsertSession(host, res.session);
      useUi.getState().toast('success', `下一轮起使用 ${short(model)}`);
    } catch (err) {
      toastError('切换模型失败', err);
    } finally {
      setBusy(false);
    }
  };

  return (
    <>
      <button
        type="button"
        disabled={disabled || busy}
        onClick={() => setOpen(true)}
        title={`模型：${current || '默认'}`}
        aria-label={`模型：${current || '默认'}`}
        className="inline-flex h-8 max-w-[44%] min-w-0 shrink items-center gap-1 rounded-md px-2 text-[13px] font-medium text-muted transition-colors hover:bg-hover hover:text-fg disabled:opacity-50 max-md:h-11 max-md:px-2.5"
      >
        {busy ? <Spinner size={14} /> : <Cpu size={14} className="shrink-0" />}
        <span className="min-w-0 truncate">{current ? short(current) : '默认模型'}</span>
        <ChevronDown size={13} className="shrink-0 opacity-60" />
      </button>
      <Sheet open={open} onClose={() => setOpen(false)} title="切换模型" width={440} bodyClassName="p-3">
        <ModelPicker models={agent?.models ?? []} value={current} defaultModel={agent?.default_model} loading={!infoLoaded} onPick={(m) => void pick(m)} onClose={() => setOpen(false)} />
      </Sheet>
    </>
  );
}
