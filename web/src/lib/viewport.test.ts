import { describe, expect, it } from 'vitest';
import { computeBand } from './viewport';

const base = { innerHeight: 800, vvHeight: 800, vvOffsetTop: 0, vvScale: 1, editableFocused: false };

describe('computeBand', () => {
  it('fills the window when nothing is focused', () => {
    expect(computeBand(base)).toEqual({ top: 0, height: 800, keyboard: false });
  });

  it('ignores a stale offset left behind after the keyboard closed', () => {
    expect(computeBand({ ...base, vvOffsetTop: 380, vvHeight: 440 })).toEqual({ top: 0, height: 800, keyboard: false });
    expect(computeBand({ ...base, vvOffsetTop: 380 })).toEqual({ top: 0, height: 800, keyboard: false });
  });

  it('follows the visual viewport while typing', () => {
    expect(computeBand({ ...base, editableFocused: true, vvHeight: 460, vvOffsetTop: 120 })).toEqual({
      top: 120,
      height: 460,
      keyboard: true,
    });
  });

  it('clamps the offset so the band stays on screen', () => {
    expect(computeBand({ ...base, editableFocused: true, vvHeight: 460, vvOffsetTop: 600 }).top).toBe(340);
    expect(computeBand({ ...base, editableFocused: true, vvHeight: 460, vvOffsetTop: -40 }).top).toBe(0);
  });

  it('keeps the full layout while pinch zoomed or for small insets', () => {
    expect(computeBand({ ...base, editableFocused: true, vvScale: 1.5, vvHeight: 400, vvOffsetTop: 100 }).top).toBe(0);
    expect(computeBand({ ...base, editableFocused: true, vvHeight: 720, vvOffsetTop: 30 }).keyboard).toBe(false);
  });
});
