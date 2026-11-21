import { useEffect, useState } from "react";
import { connect, useApp } from "./lib/store";
import { errorText } from "./lib/api";
import { Sidebar } from "./components/Sidebar";
import { Library } from "./components/Library";
import { ClipView } from "./components/ClipView";
import { Settings } from "./components/Settings";
import { Toasts } from "./components/Toasts";

export function App() {
  const view = useApp((s) => s.view);
  const refresh = useApp((s) => s.refreshClips);
  const [fatal, setFatal] = useState<string | null>(null);

  useEffect(() => {
    let off: (() => void) | undefined;
    let disposed = false;
    connect()
      .then((f) => {
        // The effect may already be cleaned up (StrictMode remount): don't leak listeners.
        if (disposed) f();
        else off = f;
      })
      .catch((e) => setFatal(errorText(e)));
    // Files may change outside the app (Explorer, other tools): rescan when we come back.
    const onFocus = () => void refresh();
    window.addEventListener("focus", onFocus);
    return () => {
      disposed = true;
      off?.();
      window.removeEventListener("focus", onFocus);
    };
  }, [refresh]);

  if (fatal) {
    return (
      <div className="flex h-full items-center justify-center p-10">
        <div className="max-w-md">
          <h1 className="text-[18px] font-semibold">BettrFTHR could not start</h1>
          <p className="mt-2 font-mono text-[13px] text-muted">{fatal}</p>
        </div>
      </div>
    );
  }

  return (
    <div className="relative flex h-full">
      <Sidebar />
      {view.page === "library" && <Library filter={view.filter} />}
      {view.page === "clip" && <ClipView path={view.path} filter={view.filter} />}
      {view.page === "settings" && <Settings />}
      <Toasts />
    </div>
  );
}
