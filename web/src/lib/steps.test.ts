import { describe, expect, it } from 'vitest';
import type { ChatItem } from '../proto/generated/ChatItem';
import { groupSteps, stepsSummary } from './steps';

const mk = (id: string, kind: ChatItem['kind'], extra: Partial<ChatItem> = {}): ChatItem => ({ id, kind, status: 'completed', paths: [], ts: 1, ...extra });

describe('groupSteps', () => {
  it('folds runs of steps and keeps single steps plain', () => {
    const items = [
      mk('u', 'user'),
      mk('r1', 'reasoning'),
      mk('c1', 'command'),
      mk('c2', 'command', { exit_code: 1 }),
      mk('a', 'agent'),
      mk('r2', 'reasoning'),
      mk('img', 'tool', { paths: ['/x/shot.png'] }),
      mk('t', 'tool', { title: 'mcp' }),
      mk('f', 'file_change'),
    ];
    const b = groupSteps(items);
    expect(b.map((x) => (x.type === 'item' ? x.item.id : x.items.map((i) => i.id).join('+')))).toEqual(['u', 'r1+c1+c2', 'a', 'r2', 'img', 't', 'f']);
    const g = b[1];
    expect(g.type === 'steps' && stepsSummary(g.items)).toBe('运行 2 个命令，思考 1 次（1 个失败）');
  });
});
