import { ArrowDown, ArrowLeft, ArrowRight, ArrowUp, ClipboardPaste, Keyboard, KeyboardOff } from 'lucide-react';
import type { ReactNode } from 'react';
import { cx } from '../ui/primitives';

export interface Modifiers {
  ctrl: boolean;
  alt: boolean;
}

function Key({ label, children, onPress, active, wide }: { label: string; children: ReactNode; onPress: () => void; active?: boolean; wide?: boolean }) {
  return (
    <button
      type="button"
      title={label}
      aria-label={label}
      aria-pressed={active}
      // Keep focus in the terminal so the soft keyboard stays open. Not on pointerdown: WebKit
      // then drops the click of a touch tap.
      onMouseDown={(e) => e.preventDefault()}
      onClick={onPress}
      className={cx(
        'inline-flex h-11 shrink-0 items-center justify-center rounded-md border font-mono text-[13px] select-none',
        wide ? 'min-w-12 px-2.5' : 'min-w-10 px-2',
        active ? 'border-accent bg-accent text-accent-fg' : 'border-line bg-bg text-fg active:bg-active',
      )}
    >
      {children}
    </button>
  );
}

export function KeyBar({
  mods,
  setMods,
  send,
  paste,
  keyboard,
  toggleKeyboard,
}: {
  mods: Modifiers;
  setMods: (m: Modifiers) => void;
  send: (seq: string) => void;
  paste: () => void;
  /** The soft keyboard is up. */
  keyboard: boolean;
  toggleKeyboard: () => void;
}) {
  return (
    <div className="flex shrink-0 border-t border-line bg-panel">
      <div className="scroll-thin flex min-w-0 flex-1 gap-1.5 overflow-x-auto overscroll-x-contain px-2 py-1.5">
      <Key label="Esc" onPress={() => send('\x1b')} wide>
        esc
      </Key>
      <Key label="Tab" onPress={() => send('\t')} wide>
        tab
      </Key>
      <Key label="Ctrl" active={mods.ctrl} onPress={() => setMods({ ...mods, ctrl: !mods.ctrl })} wide>
        ctrl
      </Key>
      <Key label="Alt" active={mods.alt} onPress={() => setMods({ ...mods, alt: !mods.alt })} wide>
        alt
      </Key>
      <Key label="上" onPress={() => send('\x1b[A')}>
        <ArrowUp size={15} />
      </Key>
      <Key label="下" onPress={() => send('\x1b[B')}>
        <ArrowDown size={15} />
      </Key>
      <Key label="左" onPress={() => send('\x1b[D')}>
        <ArrowLeft size={15} />
      </Key>
      <Key label="右" onPress={() => send('\x1b[C')}>
        <ArrowRight size={15} />
      </Key>
      <Key label="Ctrl+C" onPress={() => send('\x03')} wide>
        ^C
      </Key>
      <Key label="Ctrl+D" onPress={() => send('\x04')} wide>
        ^D
      </Key>
      <Key label="|" onPress={() => send('|')}>
        |
      </Key>
      <Key label="~" onPress={() => send('~')}>
        ~
      </Key>
      <Key label="/" onPress={() => send('/')}>
        /
      </Key>
      <Key label="-" onPress={() => send('-')}>
        -
      </Key>
      <Key label="粘贴" onPress={paste} wide>
        <ClipboardPaste size={15} />
      </Key>
      </div>
      <div className="flex shrink-0 items-center border-l border-line px-1.5">
        <Key label={keyboard ? '收起键盘' : '键盘'} onPress={toggleKeyboard} active={false} wide>
          {keyboard ? <KeyboardOff size={16} /> : <Keyboard size={16} />}
        </Key>
      </div>
    </div>
  );
}

/** Applies sticky Ctrl/Alt to typed data. */
export function applyModifiers(data: string, mods: Modifiers): string {
  let out = data;
  if (mods.ctrl && out.length === 1) {
    const c = out.toUpperCase().charCodeAt(0);
    if (c >= 64 && c <= 95) out = String.fromCharCode(c - 64);
    else if (out === ' ') out = '\x00';
    else if (out === '?') out = '\x7f';
  }
  if (mods.alt) out = '\x1b' + out;
  return out;
}
