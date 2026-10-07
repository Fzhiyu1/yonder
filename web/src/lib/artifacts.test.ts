import { describe, expect, it } from 'vitest';
import type { ChatItem } from '../proto/generated/ChatItem';
import { artifactFor, chatArtifacts, itemArtifacts, refsInText, sameOriginAssets } from './artifacts';

const it_ = (kind: ChatItem['kind'], extra: Partial<ChatItem> = {}): ChatItem => ({ id: Math.random().toString(), kind, status: 'completed', paths: [], ts: 1, ...extra });

describe('artifacts', () => {
  it('classifies paths and urls', () => {
    expect(artifactFor('/a/b/shot.PNG')?.kind).toBe('image');
    expect(artifactFor('/a/report.pdf')?.kind).toBe('pdf');
    expect(artifactFor('C:\\w\\index.html')?.name).toBe('index.html');
    expect(artifactFor('http://localhost:5173/')?.kind).toBe('web');
    expect(artifactFor('http://localhost:5173/')?.name).toBe('localhost:5173');
    expect(artifactFor('https://example.com/x.png')?.kind).toBe('image');
    expect(artifactFor('/a/b.rs')).toBeUndefined();
  });

  it('finds paths and loopback urls in prose', () => {
    const t = '截图在 `/Users/f/run/a/out.png`，报告：~/r/report.pdf。打开 http://localhost:5173/app 看看，或 http://127.0.0.1:8000。';
    expect(refsInText(t, '/Users/f')).toEqual(['http://localhost:5173/app', 'http://127.0.0.1:8000', '/Users/f/run/a/out.png', '/Users/f/r/report.pdf']);
  });

  it('resolves relative paths in inline code against the working directory', () => {
    const item = it_('agent', { text: '写成了 `docs/timeline.html`，代码在 `src/main.rs`' });
    expect(itemArtifacts(item, '/h', '/w/proj').map((a) => a.ref)).toEqual(['/w/proj/docs/timeline.html']);
  });

  it('collects per item and per chat, newest first, deduplicated', () => {
    const items = [
      it_('tool', { title: '查看图片', paths: ['/x/a.png'] }),
      it_('file_change', { paths: ['/x/index.html', '/x/main.rs'] }),
      it_('agent', { text: '看 /x/a.png 和 http://localhost:3000' }),
    ];
    expect(itemArtifacts(items[1]).map((a) => a.name)).toEqual(['index.html']);
    expect(chatArtifacts(items).map((a) => a.ref)).toEqual(['http://localhost:3000/', '/x/a.png', '/x/index.html']);
  });

  it('lists same-origin assets of a page', () => {
    const html = `<link rel="stylesheet" href="/s.css"><link rel="canonical" href="/c"><script type="module" src="./m.js"></script><img src="https://cdn.x/y.png"><img src="pic.png">`;
    expect(sameOriginAssets(html, 'http://localhost:5173/app/')).toEqual(['http://localhost:5173/s.css', 'http://localhost:5173/app/m.js', 'http://localhost:5173/app/pic.png']);
  });
});
