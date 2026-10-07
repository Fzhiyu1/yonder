import { create } from 'zustand';
import type { Artifact } from '../lib/artifacts';

/** Open artifacts of one chat: tabs of the viewer above the chat. */
export interface ViewerTabs {
  tabs: Artifact[];
  active?: string;
  /** The viewer is shown (tabs survive closing it, so it reopens where it was). */
  open: boolean;
}

interface ViewerState {
  byChat: Record<string, ViewerTabs>;
  /** Bumped per artifact ref by "reload". */
  reloads: Record<string, number>;
  openArtifact(chat: string, a: Artifact): void;
  show(chat: string, ref?: string): void;
  hide(chat: string): void;
  closeTab(chat: string, ref: string): void;
  reload(ref: string): void;
}

const MAX_TABS = 8;
const empty: ViewerTabs = { tabs: [], open: false };

export const useViewer = create<ViewerState>((set) => ({
  byChat: {},
  reloads: {},
  openArtifact(chat, a) {
    set((s) => {
      const v = s.byChat[chat] ?? empty;
      const tabs = v.tabs.some((t) => t.ref === a.ref) ? v.tabs : [...v.tabs, a].slice(-MAX_TABS);
      return { byChat: { ...s.byChat, [chat]: { tabs, active: a.ref, open: true } } };
    });
  },
  show(chat, ref) {
    set((s) => {
      const v = s.byChat[chat] ?? empty;
      return { byChat: { ...s.byChat, [chat]: { ...v, active: ref ?? v.active, open: true } } };
    });
  },
  hide(chat) {
    set((s) => {
      const v = s.byChat[chat];
      return v ? { byChat: { ...s.byChat, [chat]: { ...v, open: false } } } : s;
    });
  },
  closeTab(chat, ref) {
    set((s) => {
      const v = s.byChat[chat];
      if (!v) return s;
      const i = v.tabs.findIndex((t) => t.ref === ref);
      const tabs = v.tabs.filter((t) => t.ref !== ref);
      const active = v.active === ref ? (tabs[Math.min(i, tabs.length - 1)]?.ref ?? undefined) : v.active;
      return { byChat: { ...s.byChat, [chat]: { tabs, active, open: v.open && tabs.length > 0 } } };
    });
  },
  reload(ref) {
    set((s) => ({ reloads: { ...s.reloads, [ref]: (s.reloads[ref] ?? 0) + 1 } }));
  },
}));

export const viewerOf = (chat: string) => (s: ViewerState) => s.byChat[chat] ?? empty;
