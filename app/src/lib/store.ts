import { create } from "zustand";
import { listen } from "@tauri-apps/api/event";
import { api, errorText, type Clip, type EngineState, type Game, type Settings, type Status } from "./api";

export type Filter = { kind: "all" } | { kind: "recordings" } | { kind: "game"; game: string };

export type View =
  | { page: "library"; filter: Filter }
  | { page: "clip"; path: string; filter: Filter }
  | { page: "settings" };

export interface Toast {
  id: number;
  tone: "saved" | "info" | "error";
  title: string;
  body?: string;
  action?: { label: string; run: () => void };
}

interface AppStore {
  engine: EngineState | null;
  status: Status | null;
  game: Game | null;
  settings: Settings | null;
  clips: Clip[];
  clipsLoaded: boolean;
  view: View;
  search: string;
  toasts: Toast[];

  setView: (v: View) => void;
  setSearch: (s: string) => void;
  refreshClips: () => Promise<void>;
  patchClip: (path: string, patch: Partial<Clip>) => void;
  replaceClipPath: (oldPath: string, newPath: string, title: string) => void;
  removeClip: (path: string) => void;
  setSettings: (s: Settings) => void;
  toast: (t: Omit<Toast, "id">) => void;
  dismiss: (id: number) => void;
}

let toastId = 0;

export const useApp = create<AppStore>((set, get) => ({
  engine: null,
  status: null,
  game: null,
  settings: null,
  clips: [],
  clipsLoaded: false,
  view: { page: "library", filter: { kind: "all" } },
  search: "",
  toasts: [],

  setView: (view) => set({ view }),
  setSearch: (search) => set({ search }),
  refreshClips: async () => {
    try {
      const clips = await api.listClips();
      set({ clips, clipsLoaded: true });
    } catch (e) {
      set({ clipsLoaded: true });
      get().toast({ tone: "error", title: "Could not read the clip folder", body: errorText(e) });
    }
  },
  patchClip: (path, patch) => set({ clips: get().clips.map((c) => (c.path === path ? { ...c, ...patch } : c)) }),
  replaceClipPath: (oldPath, newPath, title) => {
    const view = get().view;
    set({
      clips: get().clips.map((c) => (c.path === oldPath ? { ...c, path: newPath, title, thumb: null } : c)),
      view: view.page === "clip" && view.path === oldPath ? { ...view, path: newPath } : view,
    });
  },
  removeClip: (path) => set({ clips: get().clips.filter((c) => c.path !== path) }),
  setSettings: (settings) => set({ settings }),
  toast: (t) => {
    const id = ++toastId;
    set({ toasts: [...get().toasts.slice(-3), { ...t, id }] });
    setTimeout(() => get().dismiss(id), t.tone === "error" ? 9000 : 5000);
  },
  dismiss: (id) => set({ toasts: get().toasts.filter((t) => t.id !== id) }),
}));

/** Subscribes to backend events once; returns an unsubscribe function. */
export async function connect(): Promise<() => void> {
  const s = useApp.getState();
  const [engine, settings, game] = await Promise.all([api.engineState(), api.getSettings(), api.currentGame()]);
  useApp.setState({ engine, settings, game, status: engine.status });
  void s.refreshClips();

  const offs = await Promise.all([
    listen<EngineState>("engine-state", (e) => {
      useApp.setState({ engine: e.payload, ...(e.payload.phase !== "running" ? { status: null } : {}) });
    }),
    listen<Game | null>("game", (e) => useApp.setState({ game: e.payload })),
    listen<Record<string, unknown>>("engine-event", (e) => onEngineEvent(e.payload)),
  ]);
  return () => offs.forEach((off) => off());
}

function onEngineEvent(ev: Record<string, unknown>) {
  const st = useApp.getState();
  switch (ev.event) {
    case "status":
      useApp.setState({ status: ev as unknown as Status });
      break;
    case "clip_saved": {
      const path = String(ev.path);
      void st.refreshClips();
      const seconds = Number(ev.seconds ?? 0);
      st.toast({
        tone: "saved",
        title: "Clip saved",
        body: `${ev.game ? `${ev.game}, ` : ""}${Math.round(seconds)} seconds`,
        action: {
          label: "View",
          run: () => useApp.setState({ view: { page: "clip", path, filter: { kind: "all" } } }),
        },
      });
      break;
    }
    case "recording_started":
      void st.refreshClips();
      st.toast({ tone: "info", title: "Recording started" });
      break;
    case "recording_stopped": {
      void st.refreshClips();
      const path = String(ev.path);
      st.toast({
        tone: "saved",
        title: ev.reason === "stopped" ? "Recording saved" : "Recording saved early",
        body: ev.reason === "stopped" ? undefined : `Reason: ${ev.reason}. A new file continues if capture recovered.`,
        action: {
          label: "View",
          run: () => useApp.setState({ view: { page: "clip", path, filter: { kind: "all" } } }),
        },
      });
      break;
    }
    case "error":
    case "warning":
      st.toast({ tone: "error", title: ev.event === "error" ? "Capture error" : "Heads up", body: String(ev.message) });
      break;
    case "restarted":
      st.toast({ tone: "info", title: "Capture recovered", body: "The video pipeline restarted after a display change or stall." });
      break;
  }
}
