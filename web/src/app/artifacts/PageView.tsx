import { useCallback, useMemo } from 'react';
import type { Artifact } from '../../lib/artifacts';
import { makeHostFetcher, resolveFsPath, type PreviewFetcher, type PreviewSource } from '../../lib/preview';
import { getConn } from '../../net/provider';
import { RequestError } from '../../net/types';
import { inlinePage } from './inline-page';
import { WebPreview } from './WebPreview';

function dirOf(path: string): { dir: string; sep: '/' | '\\'; file: string } {
  const sep = /^[A-Za-z]:\\/.test(path) || (path.includes('\\') && !path.includes('/')) ? '\\' : '/';
  const i = path.lastIndexOf(sep);
  return { dir: path.slice(0, i) || sep, sep, file: path.slice(i + 1) };
}

/** A dev-server page or an HTML file of the host, with its assets, on the preview origin. */
export function PageView({ host, a, reload }: { host: string; a: Artifact; reload: number }) {
  const { source, path } = useMemo((): { source: PreviewSource; path: string } => {
    if (a.kind === 'web') {
      const u = new URL(a.ref);
      return { source: { kind: 'http', origin: u.origin }, path: u.pathname + u.search };
    }
    const { dir, sep, file } = dirOf(a.ref);
    return { source: { kind: 'fs', root: dir, sep }, path: `/${encodeURIComponent(file)}` };
  }, [a]);

  const fetcher = useCallback<PreviewFetcher>(
    (req) => {
      const conn = getConn(host);
      if (!conn) return Promise.reject(new RequestError('offline', '主机未连接'));
      return makeHostFetcher(conn)(req);
    },
    [host],
  );

  // Without the preview origin: the page with its same-server styles, scripts and images inlined.
  const fallbackHtml = useCallback(async () => {
    const base = source.kind === 'http' ? source.origin : 'http://preview.invalid';
    const pageUrl = new URL(path, base).href;
    const get = async (url: string) => {
      const p = new URL(url).pathname + new URL(url).search;
      if (source.kind === 'http') return fetcher({ kind: 'http', url });
      const file = resolveFsPath(source.root, source.sep, p);
      return file ? fetcher({ kind: 'fs', path: file }) : undefined;
    };
    const page = await get(pageUrl);
    if (!page || page.status >= 400) throw new Error(`页面返回 ${page?.status ?? '错误'}`);
    return inlinePage(new TextDecoder().decode(page.body), pageUrl, get);
  }, [source, path, fetcher]);

  return <WebPreview source={source} path={path} fetcher={fetcher} fallbackHtml={fallbackHtml} reloadKey={reload} title={a.name} />;
}
