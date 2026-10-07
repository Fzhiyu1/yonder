import type { SessionInfo } from '../../proto/generated/SessionInfo';
import type { SessionSpec } from '../../proto/generated/SessionSpec';
import { getConn } from '../../net/provider';
import { call } from '../../net/types';
import { useHosts } from '../../store/hosts';
import { toastError, useUi } from '../../store/ui';
import { navigate } from '../router';

function conn(host: string) {
  const c = getConn(host);
  if (!c) throw new Error('主机未连接');
  return c;
}

export async function renameSession(host: string, s: SessionInfo, title: string) {
  try {
    const res = await call(conn(host), { op: 'rename', session: s.id, title }, 'session');
    useHosts.getState().upsertSession(host, res.session);
  } catch (err) {
    toastError('重命名失败', err);
  }
}

export async function interruptSession(host: string, s: SessionInfo) {
  try {
    await conn(host).request({ op: 'chat_interrupt', session: s.id });
  } catch (err) {
    toastError('中断失败', err);
  }
}

export async function killSession(host: string, s: SessionInfo) {
  const ok = await useUi.getState().ask({
    title: '结束会话',
    message: `将终止「${s.title}」的进程。会话记录会保留。`,
    confirmLabel: '结束会话',
    destructive: true,
  });
  if (!ok) return;
  try {
    await conn(host).request({ op: 'kill', session: s.id });
  } catch (err) {
    toastError('结束会话失败', err);
  }
}

export async function removeSession(host: string, s: SessionInfo) {
  const ok = await useUi.getState().ask({
    title: '删除会话',
    message: `将删除「${s.title}」及主机上的日志，无法恢复。`,
    confirmLabel: '删除',
    destructive: true,
  });
  if (!ok) return;
  try {
    await conn(host).request({ op: 'remove', session: s.id });
    useHosts.getState().removeSession(host, s.id);
    navigate({ name: 'home' }, true);
  } catch (err) {
    toastError('删除失败', err);
  }
}

export async function continueAsChat(host: string, s: SessionInfo) {
  const ok = await useUi.getState().ask({
    title: '以对话继续',
    message: '将结束当前终端会话，并用该代理自身的会话记录创建一个对话会话。',
    confirmLabel: '继续',
  });
  if (!ok) return;
  try {
    const res = await call(conn(host), { op: 'continue_as_chat', session: s.id }, 'session');
    useHosts.getState().upsertSession(host, res.session);
    navigate({ name: 'session', host, session: res.session.id });
  } catch (err) {
    toastError('转为对话失败', err);
  }
}

export async function resumeSession(host: string, s: SessionInfo) {
  const spec: SessionSpec = {
    kind: 'chat',
    agent: s.agent,
    cwd: s.cwd,
    model: s.model,
    resume: s.agent_session,
    title: s.title,
  };
  try {
    const res = await call(conn(host), { op: 'create_session', spec }, 'session');
    useHosts.getState().upsertSession(host, res.session);
    navigate({ name: 'session', host, session: res.session.id });
  } catch (err) {
    toastError('恢复会话失败', err);
  }
}

export function canContinueAsChat(s: SessionInfo): boolean {
  return s.kind === 'terminal' && (s.agent === 'codex' || s.agent === 'claude' || s.agent === 'pi') && s.state !== 'exited' && !!s.agent_session;
}
