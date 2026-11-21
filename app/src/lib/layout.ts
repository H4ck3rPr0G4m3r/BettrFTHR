import { create } from "zustand";

/**
 * Panel widths the user dragged or set in Settings. A per-machine view preference, kept in
 * the webview's local storage (losing it only resets to the defaults).
 */
export const SIDEBAR = { min: 200, max: 380, default: 248 };
export const PANEL = { min: 260, max: 480, default: 320 };

interface Layout {
  sidebar: number;
  panel: number;
  setSidebar: (w: number) => void;
  setPanel: (w: number) => void;
  reset: () => void;
}

const KEY = "layout.v1";

function load(): { sidebar: number; panel: number } {
  try {
    const v = JSON.parse(localStorage.getItem(KEY) ?? "{}");
    return {
      sidebar: clamp(Number(v.sidebar) || SIDEBAR.default, SIDEBAR),
      panel: clamp(Number(v.panel) || PANEL.default, PANEL),
    };
  } catch {
    return { sidebar: SIDEBAR.default, panel: PANEL.default };
  }
}

function save(l: { sidebar: number; panel: number }) {
  try {
    localStorage.setItem(KEY, JSON.stringify(l));
  } catch {
    /* storage unavailable: the widths just aren't remembered */
  }
}

const clamp = (v: number, r: { min: number; max: number }) => Math.round(Math.min(r.max, Math.max(r.min, v)));

export const useLayout = create<Layout>((set, get) => ({
  ...load(),
  setSidebar: (w) => {
    set({ sidebar: clamp(w, SIDEBAR) });
    save(get());
  },
  setPanel: (w) => {
    set({ panel: clamp(w, PANEL) });
    save(get());
  },
  reset: () => {
    set({ sidebar: SIDEBAR.default, panel: PANEL.default });
    save(get());
  },
}));
