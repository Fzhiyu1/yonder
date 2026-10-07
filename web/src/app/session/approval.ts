import { ShieldAlert, ShieldCheck, ShieldHalf, type LucideIcon } from 'lucide-react';
import type { AgentAvailability } from '../../proto/generated/AgentAvailability';
import type { AgentKind } from '../../proto/generated/AgentKind';
import type { ApprovalMode } from '../../proto/generated/ApprovalMode';
import type { SessionInfo } from '../../proto/generated/SessionInfo';
import { getConn } from '../../net/provider';
import { call } from '../../net/types';
import { useHosts } from '../../store/hosts';
import { toastError, useUi } from '../../store/ui';
import { navigate } from '../router';

export const MODES: ApprovalMode[] = ['ask', 'auto', 'yolo'];

export const MODE_INFO: Record<ApprovalMode, { label: string; short: string; hint: string; icon: LucideIcon; tone: string }> = {
  ask: { label: '询问', short: '询问', hint: '执行命令或修改文件前先问你', icon: ShieldCheck, tone: 'text-muted' },
  auto: { label: '自动', short: '自动', hint: '工作区内自动执行，越界时才问', icon: ShieldHalf, tone: 'text-accent' },
  yolo: { label: '完全放行', short: '放行', hint: '不再询问，拥有完整权限', icon: ShieldAlert, tone: 'text-warn' },
};

/** Agents that gate commands behind approvals (pi runs tools without asking). */
export function hasApprovals(agent: AgentKind): boolean {
  return agent === 'codex' || agent === 'claude';
}

/** The mode a new chat starts in: the user's last pick for this agent, else the host's own configuration. */
export function initialMode(agent: AgentKind, info: AgentAvailability | undefined, remembered: Partial<Record<AgentKind, ApprovalMode>>): ApprovalMode {
  return remembered[agent] ?? info?.default_approval ?? 'ask';
}

/**
 * Switch a running chat's approval mode. In place when the host supports it; otherwise the
 * host restarts the chat (resuming the agent's own session) and we move to the new session.
 */
export async function setApprovalMode(host: string, s: SessionInfo, mode: ApprovalMode): Promise<boolean> {
  const conn = getConn(host);
  if (!conn) {
    useUi.getState().toast('error', '主机未连接');
    return false;
  }
  if (!s.approval_live) {
    const ok = await useUi.getState().ask({
      title: `切换到「${MODE_INFO[mode].label}」`,
      message: '这个会话由旧版主机进程启动，需要重启代理并恢复同一会话才能切换。',
      confirmLabel: '重启并切换',
    });
    if (!ok) return false;
  }
  try {
    const res = await call(conn, { op: 'set_approval_mode', session: s.id, mode }, 'session');
    useHosts.getState().upsertSession(host, res.session);
    if (res.session.id !== s.id) navigate({ name: 'session', host, session: res.session.id }, true);
    return true;
  } catch (err) {
    toastError('切换审批模式失败', err);
    return false;
  }
}
