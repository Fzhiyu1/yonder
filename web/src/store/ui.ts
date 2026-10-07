import { create } from 'zustand';
import type { NoticeLevel } from '../proto/generated/NoticeLevel';
import { DEFAULT_SETTINGS, saveSettings, type Settings } from '../storage';

export interface Toast {
  id: number;
  level: NoticeLevel | 'success';
  message: string;
}

export interface ConfirmRequest {
  title: string;
  message?: string;
  confirmLabel?: string;
  destructive?: boolean;
  resolve: (ok: boolean) => void;
}

export interface NewSessionPreset {
  host?: string;
  cwd?: string;
  kind?: 'chat' | 'terminal';
}

interface UiState {
  toasts: Toast[];
  confirm?: ConfirmRequest;
  settings: Settings;
  newSession?: NewSessionPreset;
  pairInput?: string;
  toast(level: Toast['level'], message: string): void;
  dismissToast(id: number): void;
  ask(req: Omit<ConfirmRequest, 'resolve'>): Promise<boolean>;
  closeConfirm(ok: boolean): void;
  setSettings(patch: Partial<Settings>): void;
  openNewSession(p?: NewSessionPreset): void;
  closeNewSession(): void;
  openPair(input: string): void;
  closePair(): void;
}

let toastId = 1;

export const useUi = create<UiState>((set, get) => ({
  toasts: [],
  settings: DEFAULT_SETTINGS,
  toast(level, message) {
    const id = toastId++;
    set((s) => ({ toasts: [...s.toasts.slice(-3), { id, level, message }] }));
    setTimeout(() => get().dismissToast(id), level === 'error' ? 6000 : 3500);
  },
  dismissToast(id) {
    set((s) => ({ toasts: s.toasts.filter((t) => t.id !== id) }));
  },
  ask(req) {
    return new Promise<boolean>((resolve) => {
      get().confirm?.resolve(false);
      set({ confirm: { ...req, resolve } });
    });
  },
  closeConfirm(ok) {
    const c = get().confirm;
    set({ confirm: undefined });
    c?.resolve(ok);
  },
  setSettings(patch) {
    const settings = { ...get().settings, ...patch };
    set({ settings });
    void saveSettings(settings);
  },
  openNewSession(p = {}) {
    set({ newSession: p });
  },
  closeNewSession() {
    set({ newSession: undefined });
  },
  openPair(input) {
    set({ pairInput: input });
  },
  closePair() {
    set({ pairInput: undefined });
  },
}));

export function toastError(prefix: string, err: unknown): void {
  const msg = err instanceof Error ? err.message : String(err);
  useUi.getState().toast('error', `${prefix}：${msg}`);
}
