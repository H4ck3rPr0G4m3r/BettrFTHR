import { useMemo } from "react";
import { FilmStrip, GearSix, Record as RecordIcon, VideoCamera } from "@phosphor-icons/react";
import { useApp, type Filter } from "../lib/store";
import { api, errorText } from "../lib/api";
import { formatDuration } from "../lib/format";
import { Button, Combo, GameTile, cx } from "./ui";
import { ResizeHandle } from "./ResizeHandle";
import { SIDEBAR, useLayout } from "../lib/layout";

function NavItem({
  active,
  icon,
  label,
  count,
  onClick,
}: {
  active: boolean;
  icon: React.ReactNode;
  label: string;
  count?: number;
  onClick: () => void;
}) {
  return (
    <button
      onClick={onClick}
      aria-current={active ? "page" : undefined}
      className={cx(
        "group relative flex h-9 w-full items-center gap-3 rounded-ctl px-2.5 text-left text-[13.5px] transition-colors",
        active ? "bg-hover text-fg" : "text-muted hover:bg-hover/60 hover:text-fg",
      )}
    >
      {active && <span className="absolute -left-3 top-2 bottom-2 w-[3px] rounded-r bg-accent" />}
      <span className="flex size-7 shrink-0 items-center justify-center">{icon}</span>
      <span className="min-w-0 flex-1 truncate">{label}</span>
      {count != null && <span className="font-mono text-[11.5px] text-faint">{count}</span>}
    </button>
  );
}

const same = (a: Filter, b: Filter) =>
  a.kind === b.kind && (a.kind !== "game" || (b.kind === "game" && a.game === b.game));

export function Sidebar() {
  const view = useApp((s) => s.view);
  const setView = useApp((s) => s.setView);
  const clips = useApp((s) => s.clips);

  const games = useMemo(() => {
    const m = new Map<string, { count: number; latest: number }>();
    for (const c of clips) {
      if (!c.game) continue;
      const g = m.get(c.game) ?? { count: 0, latest: 0 };
      g.count++;
      g.latest = Math.max(g.latest, c.modified);
      m.set(c.game, g);
    }
    return [...m.entries()].sort((a, b) => b[1].latest - a[1].latest);
  }, [clips]);

  const current: Filter | null = view.page === "settings" ? null : view.filter;
  const go = (filter: Filter) => setView({ page: "library", filter });
  const recordings = clips.filter((c) => c.kind === "recording").length;
  const width = useLayout((s) => s.sidebar);
  const setWidth = useLayout((s) => s.setSidebar);

  return (
    <aside className="relative flex shrink-0 flex-col border-r border-line bg-panel" style={{ width }}>
      <ResizeHandle
        edge="right"
        width={width}
        onWidth={setWidth}
        onReset={() => setWidth(SIDEBAR.default)}
        label="Resize sidebar"
      />
      <div data-tauri-drag-region className="flex h-14 shrink-0 items-center gap-2.5 px-5">
        <img src="/logo.png" alt="" className="pointer-events-none size-6" draggable={false} />
        <span className="pointer-events-none text-[15px] font-semibold tracking-tight">
          Bettr<span className="text-accent">FTHR</span>
        </span>
      </div>

      <nav className="flex min-h-0 flex-1 flex-col gap-0.5 overflow-y-auto px-3 pb-3">
        <NavItem
          active={!!current && same(current, { kind: "all" })}
          icon={<FilmStrip size={18} />}
          label="All clips"
          count={clips.length}
          onClick={() => go({ kind: "all" })}
        />
        <NavItem
          active={!!current && same(current, { kind: "recordings" })}
          icon={<VideoCamera size={18} />}
          label="Recordings"
          count={recordings}
          onClick={() => go({ kind: "recordings" })}
        />

        <div className="mt-5 mb-1.5 px-2.5 text-[12px] font-medium text-faint">Games</div>
        {games.length === 0 ? (
          <p className="px-2.5 text-[12.5px] leading-relaxed text-faint">
            Clips are filed under the game you were playing.
          </p>
        ) : (
          games.map(([name, g]) => (
            <NavItem
              key={name}
              active={!!current && same(current, { kind: "game", game: name })}
              icon={<GameTile name={name} size={24} />}
              label={name}
              count={g.count}
              onClick={() => go({ kind: "game", game: name })}
            />
          ))
        )}
      </nav>

      <CapturePanel />

      <div className="border-t border-line px-3 py-2">
        <NavItem
          active={view.page === "settings"}
          icon={<GearSix size={18} />}
          label="Settings"
          onClick={() => setView({ page: "settings" })}
        />
      </div>
    </aside>
  );
}

function BufferMeter({ buffered, target }: { buffered: number; target: number }) {
  // One segment per second up to 60, then proportionally: the meter reads like a timeline.
  const segments = Math.min(target, 60);
  const filled = Math.round((Math.min(buffered, target) / target) * segments);
  return (
    <div>
      <div className="flex h-5 items-end gap-[2px]" aria-hidden>
        {Array.from({ length: segments }, (_, i) => (
          <span
            key={i}
            className={cx(
              "flex-1 rounded-[1px] transition-colors duration-300",
              i < filled ? "bg-accent" : "bg-line",
              i % 5 === 4 ? "h-full" : "h-3/5",
            )}
          />
        ))}
      </div>
      <div className="mt-1.5 flex justify-between font-mono text-[11px] text-faint">
        <span>-{target}s</span>
        <span>{buffered < target ? `${Math.floor(buffered)}s buffered` : "now"}</span>
      </div>
    </div>
  );
}

function CapturePanel() {
  const engine = useApp((s) => s.engine);
  const status = useApp((s) => s.status);
  const game = useApp((s) => s.game);
  const settings = useApp((s) => s.settings);
  const toast = useApp((s) => s.toast);

  const phase = engine?.phase ?? "starting";
  const info = engine?.info;
  const recording = !!engine?.recording;
  const since = engine?.recording_since;
  const replay = settings?.replay_seconds ?? 30;

  const send = (cmd: string) =>
    api.engine({ cmd }).catch((e) => toast({ tone: "error", title: "Not available", body: errorText(e) }));

  const headline = {
    running: recording ? "Recording" : "Replay buffer on",
    starting: "Starting capture",
    idle: "Waiting for a game",
    crashed: "Capture restarting",
    missing: "Capture engine missing",
  }[phase];

  return (
    <section className="mx-3 mb-3 rounded-card border border-line bg-bg/60 p-3.5" aria-label="Capture">
      <div className="flex items-center justify-between gap-2">
        <div className="flex min-w-0 items-center gap-2">
          <span
            className={cx(
              "size-2 shrink-0 rounded-full",
              recording && "anim-rec bg-rec",
              !recording && phase === "running" && "bg-accent",
              (phase === "starting" || phase === "idle") && "bg-faint",
              (phase === "crashed" || phase === "missing") && "bg-warn",
            )}
          />
          <span className="truncate text-[13px] font-medium">{headline}</span>
        </div>
        {info && (
          <span className="font-mono text-[11px] text-faint">
            {info.height}p{info.fps}
          </span>
        )}
      </div>

      <div className="mt-2.5 flex min-w-0 items-center gap-2 text-[12.5px]">
        {game ? (
          <>
            <GameTile name={game.name} size={18} />
            <span className="truncate text-muted">{game.name}</span>
          </>
        ) : (
          <span className="text-faint">{settings?.game_detection === false ? "Game detection off" : "No game detected"}</span>
        )}
      </div>

      {(phase === "crashed" || phase === "missing") && engine?.last_error && (
        <p className="mt-2 line-clamp-3 text-[12px] leading-snug text-warn/90">{engine.last_error}</p>
      )}

      {phase === "running" && (
        <div className="mt-3">
          <BufferMeter buffered={status?.buffered_seconds ?? 0} target={replay} />
        </div>
      )}

      <div className="mt-3 flex flex-col gap-1.5">
        <Button
          variant="primary"
          className="w-full justify-between"
          disabled={phase !== "running"}
          onClick={() => send("save")}
        >
          <span>Save clip</span>
          {settings && <Combo combo={settings.hotkeys.save} className="[&_kbd]:border-accent-ink/25 [&_kbd]:bg-accent-ink/10 [&_kbd]:text-accent-ink" />}
        </Button>
        <Button
          variant={recording ? "danger" : "secondary"}
          className="w-full justify-between"
          disabled={phase !== "running"}
          onClick={() => send("record_toggle")}
        >
          <span className="flex items-center gap-2">
            <RecordIcon size={15} weight="fill" className={recording ? "text-rec" : "text-faint"} />
            {recording ? <span className="font-mono">{formatDuration(since ? (Date.now() - since) / 1000 : 0) || "0:00"}</span> : "Record"}
          </span>
          {settings && <Combo combo={settings.hotkeys.record} />}
        </Button>
      </div>
    </section>
  );
}
