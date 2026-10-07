import { useEffect, useRef, useState } from 'react';
import { MonitorX } from 'lucide-react';
import { openPreview, previewOrigin, type PreviewFetcher, type PreviewSource, type PreviewStatus } from '../../lib/preview';
import { EmptyState, Spinner } from '../ui/primitives';

/** How long the bridge may take to register its worker before we fall back. */
const READY_TIMEOUT_MS = 6000;

type Phase = 'starting' | 'ready' | 'loaded' | 'fallback' | 'failed';

/**
 * A web page or HTML file from the host, rendered on the preview origin (own origin, service
 * worker answering every request through `fetcher`). When the preview origin is unavailable
 * the page is shown from `fallbackHtml()` in a sandboxed `srcdoc` frame without its assets.
 */
export function WebPreview({
  source,
  path,
  fetcher,
  fallbackHtml,
  reloadKey = 0,
  title,
  className,
}: {
  source: PreviewSource;
  /** Initial path including the query, e.g. `/` or `/index.html`. */
  path: string;
  fetcher: PreviewFetcher;
  /** Page markup for the fallback frame; undefined or a rejection shows an error. */
  fallbackHtml?: () => Promise<string>;
  /** Change to reload the page. */
  reloadKey?: number;
  title?: string;
  className?: string;
}) {
  const [phase, setPhase] = useState<Phase>('starting');
  const [src, setSrc] = useState<string>();
  const [srcdoc, setSrcdoc] = useState<string>();
  const [error, setError] = useState<string>();
  // The latest fetcher/fallback without restarting the preview on every render.
  const fetcherRef = useRef(fetcher);
  const fallbackRef = useRef(fallbackHtml);
  fetcherRef.current = fetcher;
  fallbackRef.current = fallbackHtml;
  const sourceKey = source.kind === 'http' ? `http ${source.origin}` : `fs ${source.sep} ${source.root}`;

  useEffect(() => {
    let alive = true;
    let timer: ReturnType<typeof setTimeout> | undefined;
    setPhase('starting');
    setSrc(undefined);
    setSrcdoc(undefined);
    setError(undefined);

    const fallBack = (reason: string) => {
      if (!alive) return;
      clearTimeout(timer);
      handle?.dispose();
      handle = undefined;
      setSrc(undefined);
      const make = fallbackRef.current;
      if (!make) {
        setError(reason);
        setPhase('failed');
        return;
      }
      setPhase('fallback');
      make().then(
        (html) => alive && setSrcdoc(html),
        (err: unknown) => {
          if (!alive) return;
          setError(err instanceof Error ? err.message : String(err));
          setPhase('failed');
        },
      );
    };

    let handle: ReturnType<typeof openPreview> | undefined;
    if (!previewOrigin() || !window.isSecureContext) {
      fallBack('preview origin unavailable');
    } else {
      const onStatus = (s: PreviewStatus) => {
        if (!alive) return;
        if (s.state === 'error') return fallBack(s.message);
        clearTimeout(timer);
        setPhase((p) => (p === 'starting' || p === 'ready' ? s.state : p));
      };
      handle = openPreview({ source, path, fetcher: (req) => fetcherRef.current(req), onStatus });
      setSrc(handle.src);
      timer = setTimeout(() => fallBack('preview did not start'), READY_TIMEOUT_MS);
    }
    return () => {
      alive = false;
      clearTimeout(timer);
      handle?.dispose();
    };
    // `source` is keyed by sourceKey so callers may pass a fresh object every render.
  }, [sourceKey, path, reloadKey]);

  const busy = phase === 'starting' || phase === 'ready' || (phase === 'fallback' && srcdoc === undefined);
  return (
    <div className={className ?? 'relative h-full w-full overflow-hidden bg-white'}>
      {src && (
        <iframe
          key={src}
          src={src}
          title={title ?? 'Preview'}
          allow="fullscreen; clipboard-write"
          referrerPolicy="no-referrer"
          className="absolute inset-0 h-full w-full border-0 bg-white"
        />
      )}
      {phase === 'fallback' && srcdoc !== undefined && (
        <iframe
          title={title ?? 'Preview'}
          srcDoc={srcdoc}
          sandbox="allow-scripts"
          referrerPolicy="no-referrer"
          className="absolute inset-0 h-full w-full border-0 bg-white"
        />
      )}
      {busy && (
        <div className="pointer-events-none absolute inset-x-0 top-0 flex justify-center pt-6" aria-live="polite">
          <div className="flex items-center gap-2 rounded-md border border-line bg-bg px-3 py-1.5 text-[13px] text-muted shadow-pop">
            <Spinner />
            加载中
          </div>
        </div>
      )}
      {phase === 'failed' && (
        <div className="absolute inset-0 bg-bg">
          <EmptyState icon={<MonitorX size={28} />} title="无法显示此页面">
            {error && <div className="max-w-80 text-[13px] break-words">{error}</div>}
          </EmptyState>
        </div>
      )}
    </div>
  );
}
