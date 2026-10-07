/**
 * iOS Safari overlays the soft keyboard on the page instead of resizing it, so bottom-anchored
 * controls (composer, sheet footers, the terminal key bar) end up under the keyboard. Track the
 * visual viewport in CSS variables; the app root and overlays size themselves to it.
 *
 *   --vv-top     visible band offset inside the layout viewport (iOS pans it while typing)
 *   --vv-height  visible band height
 *   html[data-kb] the soft keyboard covers a significant part of the screen
 *
 * WebKit can leave `visualViewport.offsetTop` (and a shrunken height) stale after the keyboard
 * is dismissed or while the page bounces, which shifted the whole fixed app down the screen.
 * The visual viewport is therefore only trusted while an editable element is focused and the
 * band is clearly shorter than the window; otherwise the app fills the window from the top.
 */
const KEYBOARD_MIN_PX = 120;

export interface ViewportSample {
  innerHeight: number;
  vvHeight: number;
  vvOffsetTop: number;
  vvScale: number;
  editableFocused: boolean;
}

export interface Band {
  top: number;
  height: number;
  keyboard: boolean;
}

export function computeBand(s: ViewportSample): Band {
  const full: Band = { top: 0, height: s.innerHeight, keyboard: false };
  // Pinch zoom also shrinks the visual viewport: keep the full layout then.
  if (Math.abs(s.vvScale - 1) > 0.01) return full;
  if (!s.editableFocused || s.innerHeight - s.vvHeight <= KEYBOARD_MIN_PX) return full;
  const height = Math.max(0, s.vvHeight);
  const top = Math.max(0, Math.min(s.vvOffsetTop, s.innerHeight - height));
  return { top, height, keyboard: true };
}

export function isEditable(el: Element | null): boolean {
  if (!el) return false;
  if (el instanceof HTMLTextAreaElement) return !el.readOnly && !el.disabled;
  if (el instanceof HTMLInputElement) {
    const nonText = ['button', 'checkbox', 'color', 'file', 'hidden', 'image', 'radio', 'range', 'reset', 'submit'];
    return !nonText.includes(el.type) && !el.readOnly && !el.disabled;
  }
  return el instanceof HTMLElement && el.isContentEditable;
}

export function trackVisualViewport(win: Window = window): () => void {
  const vv = win.visualViewport;
  const doc = win.document;
  const root = doc.documentElement;
  if (!vv) return () => undefined;
  let frame = 0;
  let settle = 0;
  const apply = () => {
    frame = 0;
    const band = computeBand({
      innerHeight: win.innerHeight,
      vvHeight: vv.height,
      vvOffsetTop: vv.offsetTop,
      vvScale: vv.scale,
      editableFocused: isEditable(doc.activeElement),
    });
    root.style.setProperty('--vv-height', `${Math.round(band.height)}px`);
    root.style.setProperty('--vv-top', `${Math.round(band.top)}px`);
    root.toggleAttribute('data-kb', band.keyboard);
    // The app is a fixed layer; any document scroll left behind by the keyboard is stale.
    if (!band.keyboard && (win.scrollY || doc.documentElement.scrollTop)) win.scrollTo(0, 0);
  };
  const schedule = () => {
    if (!frame) frame = win.requestAnimationFrame(apply);
  };
  // Keyboard show/hide animations finish after focus changes; re-check once they settle.
  const scheduleSettled = () => {
    schedule();
    if (settle) win.clearTimeout(settle);
    settle = win.setTimeout(() => {
      settle = 0;
      schedule();
    }, 350);
  };
  apply();
  vv.addEventListener('resize', schedule);
  vv.addEventListener('scroll', schedule);
  win.addEventListener('resize', schedule);
  win.addEventListener('scroll', schedule, { passive: true });
  win.addEventListener('orientationchange', scheduleSettled);
  doc.addEventListener('focusin', scheduleSettled);
  doc.addEventListener('focusout', scheduleSettled);
  doc.addEventListener('visibilitychange', scheduleSettled);
  return () => {
    if (frame) win.cancelAnimationFrame(frame);
    if (settle) win.clearTimeout(settle);
    vv.removeEventListener('resize', schedule);
    vv.removeEventListener('scroll', schedule);
    win.removeEventListener('resize', schedule);
    win.removeEventListener('scroll', schedule);
    win.removeEventListener('orientationchange', scheduleSettled);
    doc.removeEventListener('focusin', scheduleSettled);
    doc.removeEventListener('focusout', scheduleSettled);
    doc.removeEventListener('visibilitychange', scheduleSettled);
  };
}
