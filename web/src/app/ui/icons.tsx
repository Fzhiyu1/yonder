import { Apple, Bot, Monitor, MessageSquare, Server, Sparkles, SquareTerminal, TerminalSquare, Wand2, type LucideProps } from 'lucide-react';
import type { AgentKind } from '../../proto/generated/AgentKind';
import type { SessionInfo } from '../../proto/generated/SessionInfo';

export const AGENT_LABEL: Record<AgentKind, string> = {
  codex: 'Codex',
  claude: 'Claude Code',
  pi: 'pi',
  shell: 'Shell',
  custom: '自定义命令',
};

export function AgentIcon({ agent, kind, ...props }: { agent: AgentKind; kind?: SessionInfo['kind'] } & LucideProps) {
  const size = props.size ?? 16;
  switch (agent) {
    case 'codex':
      return <Sparkles {...props} size={size} />;
    case 'claude':
      return <Bot {...props} size={size} />;
    case 'pi':
      return <Wand2 {...props} size={size} />;
    case 'shell':
      return <SquareTerminal {...props} size={size} />;
    default:
      return kind === 'chat' ? <MessageSquare {...props} size={size} /> : <TerminalSquare {...props} size={size} />;
  }
}

export function OsIcon({ os, ...props }: { os?: string } & LucideProps) {
  const size = props.size ?? 14;
  if (os === 'macos') return <Apple {...props} size={size} />;
  if (os === 'windows') return <Monitor {...props} size={size} />;
  return <Server {...props} size={size} />;
}

export const OS_LABEL: Record<string, string> = { macos: 'macOS', linux: 'Linux', windows: 'Windows' };
