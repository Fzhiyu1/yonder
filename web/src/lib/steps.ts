import type { ChatItem } from '../proto/generated/ChatItem';

/** A chat row: one item, or a run of work steps shown as one collapsible block. */
export type ChatBlock = { type: 'item'; item: ChatItem } | { type: 'steps'; id: string; items: ChatItem[] };

const IMAGE_TOOL = /\.(png|jpe?g|gif|webp|bmp|avif|svg)$/i;

/** Work steps that fold into a group: thinking, commands, tool calls, searches. */
export function isStep(it: ChatItem): boolean {
  if (it.kind === 'reasoning' || it.kind === 'command' || it.kind === 'web_search') return true;
  // Tool calls that show an image stay visible on their own.
  if (it.kind === 'tool') return !it.paths.some((p) => IMAGE_TOOL.test(p));
  return false;
}

/** Fold consecutive steps; a run of one stays a plain item. */
export function groupSteps(items: ChatItem[]): ChatBlock[] {
  const out: ChatBlock[] = [];
  let run: ChatItem[] = [];
  const flush = () => {
    if (run.length === 1) out.push({ type: 'item', item: run[0] });
    else if (run.length > 1) out.push({ type: 'steps', id: `g-${run[0].id}`, items: run });
    run = [];
  };
  for (const it of items) {
    if (isStep(it)) run.push(it);
    else {
      flush();
      out.push({ type: 'item', item: it });
    }
  }
  flush();
  return out;
}

/** "已运行 3 个命令，思考 2 次" */
export function stepsSummary(items: ChatItem[]): string {
  let cmd = 0;
  let tool = 0;
  let search = 0;
  let think = 0;
  let failed = 0;
  for (const it of items) {
    if (it.kind === 'command') cmd++;
    else if (it.kind === 'tool') tool++;
    else if (it.kind === 'web_search') search++;
    else if (it.kind === 'reasoning') think++;
    if (it.status === 'failed' || (it.exit_code !== undefined && it.exit_code !== 0)) failed++;
  }
  const parts = [cmd && `运行 ${cmd} 个命令`, tool && `调用 ${tool} 次工具`, search && `搜索 ${search} 次`, think && `思考 ${think} 次`].filter(Boolean) as string[];
  const s = parts.join('，') || `${items.length} 个步骤`;
  return failed ? `${s}（${failed} 个失败）` : s;
}
