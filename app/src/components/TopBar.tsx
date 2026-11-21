import { getCurrentWindow } from "@tauri-apps/api/window";
import { MagnifyingGlass, Minus, Square, X } from "@phosphor-icons/react";
import type { ReactNode } from "react";
import { useApp } from "../lib/store";

/** Custom title bar: page title, search, and window controls. The empty space drags the window. */
export function TopBar({ title, children, search = true }: { title: ReactNode; children?: ReactNode; search?: boolean }) {
  const value = useApp((s) => s.search);
  const setSearch = useApp((s) => s.setSearch);
  const win = getCurrentWindow();

  return (
    <header data-tauri-drag-region className="flex h-14 shrink-0 items-center gap-4 border-b border-line pl-6">
      <div className="pointer-events-none flex min-w-0 items-baseline gap-3">{title}</div>
      <div data-tauri-drag-region className="flex-1 self-stretch" />
      {children}
      {search && (
        <label className="relative flex h-8 w-64 items-center">
          <MagnifyingGlass size={15} className="pointer-events-none absolute left-2.5 text-faint" />
          <input
            value={value}
            onChange={(e) => setSearch(e.target.value)}
            placeholder="Search clips"
            aria-label="Search clips"
            className="h-full w-full rounded-ctl border border-line bg-raised pl-8 pr-2 text-[13px] text-fg outline-none placeholder:text-faint focus:border-line-strong"
          />
        </label>
      )}
      <div className="flex h-full items-stretch">
        <button
          aria-label="Minimize"
          onClick={() => win.minimize()}
          className="flex w-12 items-center justify-center text-muted hover:bg-hover hover:text-fg"
        >
          <Minus size={15} />
        </button>
        <button
          aria-label="Maximize"
          onClick={() => win.toggleMaximize()}
          className="flex w-12 items-center justify-center text-muted hover:bg-hover hover:text-fg"
        >
          <Square size={13} />
        </button>
        <button
          aria-label="Close"
          onClick={() => win.close()}
          className="flex w-12 items-center justify-center text-muted hover:bg-[#c42b1c] hover:text-white"
        >
          <X size={15} />
        </button>
      </div>
    </header>
  );
}
