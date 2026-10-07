import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { FileX, MoveHorizontal, ZoomIn, ZoomOut } from 'lucide-react';
import type { PDFDocumentLoadingTask, PDFDocumentProxy, RenderTask } from 'pdfjs-dist';
import { IconButton, Spinner } from '../ui/primitives';

type PdfJs = typeof import('pdfjs-dist');

let pdfjsPromise: Promise<PdfJs> | null = null;

/** pdf.js and its worker are only fetched the first time a PDF is opened. */
function loadPdfJs(): Promise<PdfJs> {
  pdfjsPromise ??= Promise.all([import('pdfjs-dist'), import('pdfjs-dist/build/pdf.worker.min.mjs?url')])
    .then(([lib, worker]) => {
      lib.GlobalWorkerOptions.workerSrc = worker.default;
      return lib;
    })
    .catch((err: unknown) => {
      pdfjsPromise = null;
      throw err;
    });
  return pdfjsPromise;
}

// Static pdf.js data (CMaps for CJK text, standard fonts, wasm decoders), copied by vite.config.ts.
function assetUrl(dir: string): string {
  return new URL(`${import.meta.env.BASE_URL}pdfjs/${dir}/`, window.location.href).href;
}

const PAD = 12;
const GAP = 12;
const MIN_ZOOM = 0.5;
const MAX_ZOOM = 4;
const ZOOM_STEPS = [0.5, 0.75, 1, 1.25, 1.5, 2, 3, 4];
// iOS refuses to draw canvases above ~16.7M pixels; stay below that.
const MAX_CANVAS_PIXELS = 16_000_000;
const RERENDER_DELAY_MS = 140;

type Size = { w: number; h: number };
type Layout = { tops: number[]; lefts: number[]; widths: number[]; heights: number[]; width: number; height: number };
type Anchor = { page: number; fx: number; fy: number; vx: number; vy: number };
type LoadState = { status: 'loading' } | { status: 'ready'; doc: PDFDocumentProxy } | { status: 'error'; message: string };

const clampZoom = (z: number) => Math.min(MAX_ZOOM, Math.max(MIN_ZOOM, z));

/** Every page fits the container width at zoom 1; pages stack vertically and center horizontally. */
function computeLayout(sizes: Size[], viewWidth: number, zoom: number): Layout {
  const avail = Math.max(0, viewWidth - PAD * 2) * zoom;
  const width = Math.max(viewWidth, avail + PAD * 2);
  const tops: number[] = [];
  const lefts: number[] = [];
  const widths: number[] = [];
  const heights: number[] = [];
  let y = PAD;
  for (const s of sizes) {
    const h = (avail * s.h) / s.w;
    tops.push(y);
    lefts.push((width - avail) / 2);
    widths.push(avail);
    heights.push(h);
    y += h + GAP;
  }
  return { tops, lefts, widths, heights, width, height: sizes.length ? y - GAP + PAD : 0 };
}

/** Index of the page containing content offset `y` (or the last page starting above it). */
function pageAt(lay: Layout, y: number): number {
  let lo = 0;
  let hi = lay.tops.length - 1;
  while (lo < hi) {
    const mid = (lo + hi + 1) >> 1;
    if (lay.tops[mid] <= y) lo = mid;
    else hi = mid - 1;
  }
  return Math.max(0, lo);
}

export function PdfView({ blob, name }: { blob: Blob; name: string }) {
  const [state, setState] = useState<LoadState>({ status: 'loading' });
  const [sizes, setSizes] = useState<Size[]>([]);
  const [scroller, setScroller] = useState<HTMLDivElement | null>(null);
  const [view, setView] = useState<Size>({ w: 0, h: 0 });
  const [zoom, setZoom] = useState(1);
  const [current, setCurrent] = useState(1);

  useEffect(() => {
    let cancelled = false;
    let task: PDFDocumentLoadingTask | null = null;
    setState({ status: 'loading' });
    setSizes([]);
    setZoom(1);
    setCurrent(1);
    (async () => {
      const lib = await loadPdfJs();
      const data = new Uint8Array(await blob.arrayBuffer());
      if (cancelled) return;
      task = lib.getDocument({
        data,
        cMapUrl: assetUrl('cmaps'),
        cMapPacked: true,
        standardFontDataUrl: assetUrl('standard_fonts'),
        wasmUrl: assetUrl('wasm'),
        iccUrl: assetUrl('iccs'),
      });
      const doc = await task.promise;
      if (cancelled) return;
      const first = (await doc.getPage(1)).getViewport({ scale: 1 });
      if (cancelled) return;
      // Assume every page matches page 1 so the layout exists immediately; fix outliers below.
      const all: Size[] = Array.from({ length: doc.numPages }, () => ({ w: first.width, h: first.height }));
      setSizes(all);
      setState({ status: 'ready', doc });
      let dirty = false;
      for (let i = 2; i <= doc.numPages && !cancelled; i++) {
        const vp = (await doc.getPage(i)).getViewport({ scale: 1 });
        if (vp.width !== all[i - 1].w || vp.height !== all[i - 1].h) {
          all[i - 1] = { w: vp.width, h: vp.height };
          dirty = true;
        }
        if (dirty && (i % 25 === 0 || i === doc.numPages) && !cancelled) {
          setSizes([...all]);
          dirty = false;
        }
      }
    })().catch((err: unknown) => {
      if (cancelled) return;
      const errName = (err as { name?: string } | null)?.name;
      setState({
        status: 'error',
        message: errName === 'PasswordException' ? '文件有密码保护' : errName === 'InvalidPDFException' ? '文件已损坏或不是 PDF' : String((err as Error)?.message ?? err),
      });
    });
    return () => {
      cancelled = true;
      void task?.destroy();
    };
  }, [blob]);

  const lay = useMemo(() => computeLayout(sizes, view.w, zoom), [sizes, view.w, zoom]);

  // Refs mirror state for the native gesture listeners registered once per scroller.
  const zoomRef = useRef(zoom);
  const layRef = useRef(lay);
  zoomRef.current = zoom;
  layRef.current = lay;
  const anchorRef = useRef<Anchor | null>(null);

  const anchorAt = useCallback(
    (vx: number, vy: number): Anchor | null => {
      const l = layRef.current;
      if (!scroller || !l.tops.length) return null;
      const x = scroller.scrollLeft + vx;
      const y = scroller.scrollTop + vy;
      const page = pageAt(l, y);
      return {
        page,
        fx: l.widths[page] ? (x - l.lefts[page]) / l.widths[page] : 0,
        fy: l.heights[page] ? (y - l.tops[page]) / l.heights[page] : 0,
        vx,
        vy,
      };
    },
    [scroller],
  );

  const applyAnchor = useCallback(
    (a: Anchor, l: Layout) => {
      if (!scroller || a.page >= l.tops.length) return;
      scroller.scrollLeft = l.lefts[a.page] + a.fx * l.widths[a.page] - a.vx;
      scroller.scrollTop = l.tops[a.page] + a.fy * l.heights[a.page] - a.vy;
    },
    [scroller],
  );

  useLayoutEffect(() => {
    const a = anchorRef.current;
    if (!a) return;
    anchorRef.current = null;
    applyAnchor(a, lay);
  }, [lay, applyAnchor]);

  /** Zoom keeping the content under viewport point (vx, vy) in place. */
  const zoomAround = useCallback(
    (target: number, vx: number, vy: number, anchor?: Anchor | null) => {
      const z = clampZoom(target);
      const a = anchor ? { ...anchor, vx, vy } : anchorAt(vx, vy);
      if (Math.abs(z - zoomRef.current) < 1e-3) {
        if (a) applyAnchor(a, layRef.current);
        return;
      }
      anchorRef.current = a;
      setZoom(z);
    },
    [anchorAt, applyAnchor],
  );

  const zoomCenter = (z: number) => zoomAround(z, view.w / 2, view.h / 2);
  const stepZoom = (dir: 1 | -1) => {
    const z = zoomRef.current;
    const next = dir > 0 ? ZOOM_STEPS.find((s) => s > z + 0.01) : [...ZOOM_STEPS].reverse().find((s) => s < z - 0.01);
    if (next !== undefined) zoomCenter(next);
  };

  // Track the container size; keep the top-left content point fixed across resizes.
  useEffect(() => {
    if (!scroller) return;
    const ro = new ResizeObserver(() => {
      const w = scroller.clientWidth;
      const h = scroller.clientHeight;
      setView((prev) => {
        if (prev.w === w && prev.h === h) return prev;
        if (prev.w && prev.w !== w) anchorRef.current = anchorAt(0, 0);
        return { w, h };
      });
    });
    ro.observe(scroller);
    return () => ro.disconnect();
  }, [scroller, anchorAt]);

  // Pinch (touch), ctrl/cmd + wheel and trackpad pinch (desktop). Page canvases stretch with the
  // layout during the gesture and re-render at the new scale once it settles.
  useEffect(() => {
    const el = scroller;
    if (!el) return;
    let pinch: { d0: number; z0: number; anchor: Anchor | null } | null = null;
    let gesture: { z0: number } | null = null;
    const local = (x: number, y: number) => {
      const r = el.getBoundingClientRect();
      return { x: x - r.left, y: y - r.top };
    };
    const two = (e: TouchEvent) => {
      const [a, b] = [e.touches[0], e.touches[1]];
      const m = local((a.clientX + b.clientX) / 2, (a.clientY + b.clientY) / 2);
      return { d: Math.hypot(a.clientX - b.clientX, a.clientY - b.clientY) || 1, ...m };
    };
    const onTouchStart = (e: TouchEvent) => {
      if (e.touches.length !== 2) return;
      const t = two(e);
      pinch = { d0: t.d, z0: zoomRef.current, anchor: anchorAt(t.x, t.y) };
      if (e.cancelable) e.preventDefault();
    };
    const onTouchMove = (e: TouchEvent) => {
      if (!pinch || e.touches.length < 2) return;
      if (e.cancelable) e.preventDefault();
      const t = two(e);
      zoomAround(pinch.z0 * (t.d / pinch.d0), t.x, t.y, pinch.anchor);
    };
    const onTouchEnd = (e: TouchEvent) => {
      if (e.touches.length < 2) pinch = null;
    };
    const onWheel = (e: WheelEvent) => {
      if (!e.ctrlKey && !e.metaKey) return;
      e.preventDefault();
      const p = local(e.clientX, e.clientY);
      zoomAround(zoomRef.current * Math.exp(-e.deltaY * 0.01), p.x, p.y);
    };
    // Safari's own pinch events; on iOS the touch handlers above take precedence.
    const onGesture = (ev: Event) => {
      ev.preventDefault();
      const e = ev as Event & { scale: number; clientX: number; clientY: number };
      if (pinch) return;
      if (ev.type === 'gesturestart') gesture = { z0: zoomRef.current };
      else if (ev.type === 'gestureend') gesture = null;
      else if (gesture) {
        const p = local(e.clientX, e.clientY);
        zoomAround(gesture.z0 * e.scale, p.x, p.y);
      }
    };
    el.addEventListener('touchstart', onTouchStart, { passive: false });
    el.addEventListener('touchmove', onTouchMove, { passive: false });
    el.addEventListener('touchend', onTouchEnd);
    el.addEventListener('touchcancel', onTouchEnd);
    el.addEventListener('wheel', onWheel, { passive: false });
    for (const t of ['gesturestart', 'gesturechange', 'gestureend']) el.addEventListener(t, onGesture);
    return () => {
      el.removeEventListener('touchstart', onTouchStart);
      el.removeEventListener('touchmove', onTouchMove);
      el.removeEventListener('touchend', onTouchEnd);
      el.removeEventListener('touchcancel', onTouchEnd);
      el.removeEventListener('wheel', onWheel);
      for (const t of ['gesturestart', 'gesturechange', 'gestureend']) el.removeEventListener(t, onGesture);
    };
  }, [scroller, anchorAt, zoomAround]);

  const frame = useRef(0);
  const onScroll = () => {
    if (frame.current) return;
    frame.current = requestAnimationFrame(() => {
      frame.current = 0;
      const l = layRef.current;
      if (!scroller || !l.tops.length) return;
      setCurrent(pageAt(l, scroller.scrollTop + scroller.clientHeight / 2) + 1);
    });
  };
  useEffect(() => () => cancelAnimationFrame(frame.current), []);

  const doc = state.status === 'ready' ? state.doc : null;
  const pages = doc ? sizes.length : 0;

  return (
    <div className="relative h-full min-h-0 w-full bg-panel" aria-label={name} role="document">
      <div
        ref={setScroller}
        onScroll={onScroll}
        className="scroll-thin h-full w-full overflow-auto overscroll-contain"
        style={{ touchAction: 'pan-x pan-y' }}
      >
        {doc && view.w > 0 && (
          <div className="relative" style={{ width: lay.width, height: lay.height }}>
            {sizes.map((_, i) => (
              <PdfPage
                key={i}
                doc={doc}
                index={i}
                root={scroller}
                left={lay.lefts[i]}
                top={lay.tops[i]}
                width={lay.widths[i]}
                height={lay.heights[i]}
              />
            ))}
          </div>
        )}
      </div>

      {state.status === 'loading' && (
        <div className="absolute inset-0 flex flex-col items-center justify-center gap-2 text-sm text-muted">
          <Spinner size={20} />
          <span>正在打开 PDF…</span>
        </div>
      )}

      {state.status === 'error' && (
        <div className="absolute inset-0 flex flex-col items-center justify-center gap-2 px-6 text-center">
          <FileX size={28} className="text-faint" aria-hidden />
          <span className="text-sm font-medium text-fg">无法打开 PDF</span>
          <span className="max-w-full text-xs break-all text-muted">{state.message}</span>
        </div>
      )}

      {doc && (
        <div
          className="absolute left-1/2 flex -translate-x-1/2 items-center gap-0.5 rounded-lg border border-line bg-bg/90 p-0.5 shadow-[var(--shadow)] backdrop-blur"
          style={{ bottom: 'calc(12px + env(safe-area-inset-bottom))' }}
        >
          <IconButton label="缩小" size="sm" onClick={() => stepZoom(-1)} disabled={zoom <= MIN_ZOOM + 0.01}>
            <ZoomOut size={16} />
          </IconButton>
          <span className="min-w-[4.5rem] px-1 text-center text-xs text-muted tabular-nums" aria-live="polite">
            {current} / {pages}
          </span>
          <IconButton label="放大" size="sm" onClick={() => stepZoom(1)} disabled={zoom >= MAX_ZOOM - 0.01}>
            <ZoomIn size={16} />
          </IconButton>
          <IconButton label="适合宽度" size="sm" onClick={() => zoomCenter(1)} disabled={Math.abs(zoom - 1) < 0.01}>
            <MoveHorizontal size={16} />
          </IconButton>
        </div>
      )}
    </div>
  );
}

function releaseCanvas(c: Element) {
  if (c instanceof HTMLCanvasElement) {
    // Shrinking first frees the backing store immediately on iOS.
    c.width = 0;
    c.height = 0;
  }
  c.remove();
}

function PdfPage({
  doc,
  index,
  root,
  left,
  top,
  width,
  height,
}: {
  doc: PDFDocumentProxy;
  index: number;
  root: HTMLElement | null;
  left: number;
  top: number;
  width: number;
  height: number;
}) {
  const pageRef = useRef<HTMLDivElement>(null);
  const hostRef = useRef<HTMLDivElement>(null);
  const [near, setNear] = useState(false);
  const [drawn, setDrawn] = useState(false);

  useEffect(() => {
    const el = pageRef.current;
    if (!el || !root) return;
    const io = new IntersectionObserver((entries) => setNear(entries[entries.length - 1].isIntersecting), {
      root,
      rootMargin: '150% 50%',
    });
    io.observe(el);
    return () => io.disconnect();
  }, [root]);

  const cssW = Math.round(width);
  useEffect(() => {
    const host = hostRef.current;
    if (!host) return;
    if (!near || cssW <= 0) {
      for (const c of [...host.children]) releaseCanvas(c);
      setDrawn(false);
      return;
    }
    let cancelled = false;
    let task: RenderTask | null = null;
    const timer = setTimeout(
      async () => {
        try {
          const page = await doc.getPage(index + 1);
          if (cancelled) return;
          const base = page.getViewport({ scale: 1 });
          const cssScale = cssW / base.width;
          const cssH = base.height * cssScale;
          let out = Math.min(window.devicePixelRatio || 1, 3);
          if (cssW * cssH * out * out > MAX_CANVAS_PIXELS) out = Math.sqrt(MAX_CANVAS_PIXELS / (cssW * cssH));
          const viewport = page.getViewport({ scale: cssScale * out });
          const canvas = document.createElement('canvas');
          canvas.width = Math.max(1, Math.floor(viewport.width));
          canvas.height = Math.max(1, Math.floor(viewport.height));
          canvas.className = 'absolute inset-0 h-full w-full';
          task = page.render({ canvas, viewport });
          await task.promise;
          if (cancelled) {
            releaseCanvas(canvas);
            return;
          }
          // Swap only after drawing so the stretched previous canvas stays visible meanwhile.
          const old = [...host.children];
          host.appendChild(canvas);
          for (const c of old) releaseCanvas(c);
          setDrawn(true);
        } catch (err) {
          if ((err as { name?: string } | null)?.name !== 'RenderingCancelledException' && !cancelled) {
            console.warn('pdf page render failed', index + 1, err);
          }
        }
      },
      host.childElementCount ? RERENDER_DELAY_MS : 0,
    );
    return () => {
      cancelled = true;
      clearTimeout(timer);
      task?.cancel();
    };
  }, [near, cssW, doc, index]);

  useEffect(() => {
    const host = hostRef.current;
    return () => {
      if (host) for (const c of [...host.children]) releaseCanvas(c);
    };
  }, []);

  return (
    <div
      ref={pageRef}
      data-page={index + 1}
      data-rendered={drawn ? '' : undefined}
      className="absolute overflow-hidden rounded-[2px] bg-white shadow-[0_0_0_1px_var(--border),0_1px_3px_rgb(0_0_0/0.08)]"
      style={{ left, top, width, height }}
    >
      {!drawn && <span className="absolute inset-0 flex items-center justify-center text-xs text-faint tabular-nums">{index + 1}</span>}
      <div ref={hostRef} className="absolute inset-0" />
    </div>
  );
}
