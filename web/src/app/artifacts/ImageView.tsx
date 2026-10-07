import { useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react';
import { ImageOff } from 'lucide-react';
import { Spinner } from '../ui/primitives';

const MIN_ZOOM = 1;
const MAX_ZOOM = 6;
const TAP_ZOOM = 2.5;
const DOUBLE_TAP_MS = 300;
const DOUBLE_TAP_PX = 30;

type Size = { w: number; h: number };
/** Content point (fractions of the zoomed image box) to pin under viewport point (vx, vy). */
type Anchor = { fx: number; fy: number; vx: number; vy: number };

const clampZoom = (z: number) => Math.min(MAX_ZOOM, Math.max(MIN_ZOOM, z));

/**
 * Image fitted into its container (object-contain). Zoom is layout-based: the image box grows and
 * the container's native overflow scroll pans it, so momentum scrolling works on iOS.
 */
export function ImageView({ url, name }: { url: string; name: string }) {
  const [scroller, setScroller] = useState<HTMLDivElement | null>(null);
  const [view, setView] = useState<Size>({ w: 0, h: 0 });
  const [natural, setNatural] = useState<Size | null>(null);
  const [status, setStatus] = useState<'loading' | 'ready' | 'error'>('loading');
  const [zoom, setZoom] = useState(1);

  useEffect(() => {
    setStatus('loading');
    setNatural(null);
    setZoom(1);
  }, [url]);

  useEffect(() => {
    if (!scroller) return;
    const ro = new ResizeObserver(() => {
      const w = scroller.clientWidth;
      const h = scroller.clientHeight;
      setView((p) => (p.w === w && p.h === h ? p : { w, h }));
    });
    ro.observe(scroller);
    return () => ro.disconnect();
  }, [scroller]);

  // Fit size at zoom 1: contained in the viewport, never upscaled beyond 2x for tiny images.
  const fit = natural && view.w && view.h ? Math.min(view.w / natural.w, view.h / natural.h, Math.max(1, 2 / (window.devicePixelRatio || 1))) : 0;
  const imgW = natural ? natural.w * fit * zoom : 0;
  const imgH = natural ? natural.h * fit * zoom : 0;
  const boxW = Math.max(view.w, imgW);
  const boxH = Math.max(view.h, imgH);

  const geom = useRef({ imgW, imgH, boxW, boxH, zoom });
  geom.current = { imgW, imgH, boxW, boxH, zoom };
  const anchorRef = useRef<Anchor | null>(null);

  const anchorAt = useCallback(
    (vx: number, vy: number): Anchor | null => {
      const g = geom.current;
      if (!scroller || !g.imgW || !g.imgH) return null;
      const x = scroller.scrollLeft + vx - (g.boxW - g.imgW) / 2;
      const y = scroller.scrollTop + vy - (g.boxH - g.imgH) / 2;
      return { fx: x / g.imgW, fy: y / g.imgH, vx, vy };
    },
    [scroller],
  );

  useLayoutEffect(() => {
    const a = anchorRef.current;
    if (!a || !scroller) return;
    anchorRef.current = null;
    scroller.scrollLeft = (boxW - imgW) / 2 + a.fx * imgW - a.vx;
    scroller.scrollTop = (boxH - imgH) / 2 + a.fy * imgH - a.vy;
  }, [scroller, boxW, boxH, imgW, imgH]);

  const zoomAround = useCallback(
    (target: number, vx: number, vy: number, anchor?: Anchor | null) => {
      const z = clampZoom(target);
      const a = anchor ? { ...anchor, vx, vy } : anchorAt(vx, vy);
      anchorRef.current = a;
      setZoom((prev) => {
        if (Math.abs(prev - z) < 1e-3 && a && scroller) {
          // No re-layout will happen; pan directly so a pinch with moving fingers still tracks.
          const g = geom.current;
          scroller.scrollLeft = (g.boxW - g.imgW) / 2 + a.fx * g.imgW - a.vx;
          scroller.scrollTop = (g.boxH - g.imgH) / 2 + a.fy * g.imgH - a.vy;
          anchorRef.current = null;
        }
        return z;
      });
    },
    [anchorAt, scroller],
  );

  // Pinch, double tap (touch), ctrl/cmd + wheel and Safari gesture events (desktop).
  useEffect(() => {
    const el = scroller;
    if (!el) return;
    let pinch: { d0: number; z0: number; anchor: Anchor | null } | null = null;
    let gesture: { z0: number } | null = null;
    let lastTap: { t: number; x: number; y: number } | null = null;
    let moved = false;
    const local = (x: number, y: number) => {
      const r = el.getBoundingClientRect();
      return { x: x - r.left, y: y - r.top };
    };
    const two = (e: TouchEvent) => {
      const [a, b] = [e.touches[0], e.touches[1]];
      const m = local((a.clientX + b.clientX) / 2, (a.clientY + b.clientY) / 2);
      return { d: Math.hypot(a.clientX - b.clientX, a.clientY - b.clientY) || 1, ...m };
    };
    const toggleAt = (x: number, y: number) => zoomAround(geom.current.zoom > 1.01 ? 1 : TAP_ZOOM, x, y);

    const onTouchStart = (e: TouchEvent) => {
      if (e.touches.length === 1) moved = false;
      if (e.touches.length !== 2) return;
      lastTap = null;
      const t = two(e);
      pinch = { d0: t.d, z0: geom.current.zoom, anchor: anchorAt(t.x, t.y) };
      if (e.cancelable) e.preventDefault();
    };
    const onTouchMove = (e: TouchEvent) => {
      moved = true;
      if (!pinch || e.touches.length < 2) return;
      if (e.cancelable) e.preventDefault();
      const t = two(e);
      zoomAround(pinch.z0 * (t.d / pinch.d0), t.x, t.y, pinch.anchor);
    };
    const onTouchEnd = (e: TouchEvent) => {
      if (pinch) {
        if (e.touches.length < 2) pinch = null;
        return;
      }
      if (e.touches.length || moved || e.changedTouches.length !== 1) return;
      const p = local(e.changedTouches[0].clientX, e.changedTouches[0].clientY);
      const now = e.timeStamp;
      if (lastTap && now - lastTap.t < DOUBLE_TAP_MS && Math.hypot(p.x - lastTap.x, p.y - lastTap.y) < DOUBLE_TAP_PX) {
        lastTap = null;
        if (e.cancelable) e.preventDefault(); // suppress the synthetic dblclick and click
        toggleAt(p.x, p.y);
      } else {
        lastTap = { t: now, x: p.x, y: p.y };
      }
    };
    const onDblClick = (e: MouseEvent) => {
      const p = local(e.clientX, e.clientY);
      toggleAt(p.x, p.y);
    };
    const onWheel = (e: WheelEvent) => {
      if (!e.ctrlKey && !e.metaKey) return;
      e.preventDefault();
      const p = local(e.clientX, e.clientY);
      zoomAround(geom.current.zoom * Math.exp(-e.deltaY * 0.01), p.x, p.y);
    };
    const onGesture = (ev: Event) => {
      ev.preventDefault();
      if (pinch) return;
      const e = ev as Event & { scale: number; clientX: number; clientY: number };
      if (ev.type === 'gesturestart') gesture = { z0: geom.current.zoom };
      else if (ev.type === 'gestureend') gesture = null;
      else if (gesture) {
        const p = local(e.clientX, e.clientY);
        zoomAround(gesture.z0 * e.scale, p.x, p.y);
      }
    };
    el.addEventListener('touchstart', onTouchStart, { passive: false });
    el.addEventListener('touchmove', onTouchMove, { passive: false });
    el.addEventListener('touchend', onTouchEnd, { passive: false });
    el.addEventListener('touchcancel', onTouchEnd);
    el.addEventListener('dblclick', onDblClick);
    el.addEventListener('wheel', onWheel, { passive: false });
    for (const t of ['gesturestart', 'gesturechange', 'gestureend']) el.addEventListener(t, onGesture);
    return () => {
      el.removeEventListener('touchstart', onTouchStart);
      el.removeEventListener('touchmove', onTouchMove);
      el.removeEventListener('touchend', onTouchEnd);
      el.removeEventListener('touchcancel', onTouchEnd);
      el.removeEventListener('dblclick', onDblClick);
      el.removeEventListener('wheel', onWheel);
      for (const t of ['gesturestart', 'gesturechange', 'gestureend']) el.removeEventListener(t, onGesture);
    };
  }, [scroller, anchorAt, zoomAround]);

  return (
    <div className="relative h-full min-h-0 w-full bg-panel">
      <div
        ref={setScroller}
        className="scroll-thin h-full w-full overflow-auto overscroll-contain select-none"
        style={{ touchAction: 'pan-x pan-y' }}
        data-zoom={zoom.toFixed(2)}
      >
        <div className="relative" style={{ width: boxW || '100%', height: boxH || '100%' }}>
          <img
            src={url}
            alt={name}
            draggable={false}
            onLoad={(e) => {
              const img = e.currentTarget;
              setNatural({ w: img.naturalWidth || 1, h: img.naturalHeight || 1 });
              setStatus('ready');
            }}
            onError={() => setStatus('error')}
            className="absolute max-w-none"
            style={
              natural
                ? { width: imgW, height: imgH, left: (boxW - imgW) / 2, top: (boxH - imgH) / 2 }
                : { width: 1, height: 1, opacity: 0, left: 0, top: 0 }
            }
          />
        </div>
      </div>
      {status === 'loading' && (
        <div className="pointer-events-none absolute inset-0 flex items-center justify-center text-muted">
          <Spinner size={20} />
        </div>
      )}
      {status === 'error' && (
        <div className="absolute inset-0 flex flex-col items-center justify-center gap-2 px-6 text-center">
          <ImageOff size={28} className="text-faint" aria-hidden />
          <span className="text-sm font-medium text-fg">无法显示图片</span>
          <span className="max-w-full text-xs break-all text-muted">{name}</span>
        </div>
      )}
    </div>
  );
}
