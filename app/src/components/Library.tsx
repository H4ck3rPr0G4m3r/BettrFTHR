import { memo, useEffect, useMemo, useRef, useState } from "react";
import { Check, FilmSlate, Trash, X } from "@phosphor-icons/react";
import { api, errorText, fileUrl, type Clip } from "../lib/api";
import { useApp, type Filter } from "../lib/store";
import { clipTitle, formatBytes, formatDuration, formatTime, groupByDay } from "../lib/format";
import { ensureThumbs } from "../lib/thumbs";
import { TopBar } from "./TopBar";
import { Button, Combo, GameTile, cx } from "./ui";

export function filterClips(clips: Clip[], filter: Filter, search: string): Clip[] {
  const q = search.trim().toLowerCase();
  return clips.filter((c) => {
    if (filter.kind === "recordings" && c.kind !== "recording") return false;
    if (filter.kind === "game" && c.game !== filter.game) return false;
    if (!q) return true;
    return `${c.title} ${c.game ?? ""} ${clipTitle(c)}`.toLowerCase().includes(q);
  });
}

export function Library({ filter }: { filter: Filter }) {
  const clips = useApp((s) => s.clips);
  const loaded = useApp((s) => s.clipsLoaded);
  const search = useApp((s) => s.search);
  const recordingPath = useApp((s) => s.engine?.recording ?? null);
  const selected = useApp((s) => s.selected);
  const setSelected = useApp((s) => s.setSelected);

  const shown = useMemo(() => filterClips(clips, filter, search), [clips, filter, search]);
  const groups = useMemo(() => groupByDay(shown), [shown]);
  const order = useMemo(() => shown.map((c) => c.path), [shown]);

  useEffect(() => {
    ensureThumbs(shown, recordingPath);
  }, [shown, recordingPath]);

  // Ctrl+A selects everything shown, Esc clears.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.target as HTMLElement).tagName === "INPUT") return;
      if (e.key === "a" && (e.ctrlKey || e.metaKey)) {
        e.preventDefault();
        setSelected(order.filter((p) => p !== recordingPath));
      }
      if (e.key === "Escape" && selected.length) setSelected([]);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [order, selected.length, setSelected, recordingPath]);

  const totalBytes = shown.reduce((n, c) => n + c.bytes, 0);
  const title =
    filter.kind === "all" ? "All clips" : filter.kind === "recordings" ? "Recordings" : filter.game;
  const selecting = selected.length > 0;

  return (
    <div className="relative flex min-w-0 flex-1 flex-col">
      <TopBar
        title={
          <>
            {filter.kind === "game" && <GameTile name={filter.game} size={24} />}
            <h1 className="truncate text-[17px] font-semibold tracking-tight">{title}</h1>
            {loaded && shown.length > 0 && (
              <span className="font-mono text-[12px] text-faint">
                {shown.length} · {formatBytes(totalBytes)}
              </span>
            )}
          </>
        }
      />
      <div className="min-h-0 flex-1 overflow-y-auto px-6 pt-5 pb-28">
        {!loaded ? (
          <Skeleton />
        ) : shown.length === 0 ? (
          <Empty filter={filter} searching={!!search.trim()} />
        ) : (
          groups.map((g) => (
            <section key={g.label} className="mb-8">
              <h2 className="mb-3 text-[13px] font-medium text-muted">{g.label}</h2>
              <div className="grid grid-cols-[repeat(auto-fill,minmax(250px,1fr))] gap-x-4 gap-y-6">
                {g.clips.map((c) => (
                  <ClipCard
                    key={c.path}
                    clip={c}
                    filter={filter}
                    live={c.path === recordingPath}
                    selected={selected.includes(c.path)}
                    selecting={selecting}
                    order={order}
                  />
                ))}
              </div>
            </section>
          ))
        )}
      </div>
      <SelectionBar shown={shown} recordingPath={recordingPath} />
    </div>
  );
}

function SelectionBar({ shown, recordingPath }: { shown: Clip[]; recordingPath: string | null }) {
  const selected = useApp((s) => s.selected);
  const setSelected = useApp((s) => s.setSelected);
  const removeClips = useApp((s) => s.removeClips);
  const toast = useApp((s) => s.toast);
  const [confirm, setConfirm] = useState(false);
  const [busy, setBusy] = useState(false);

  useEffect(() => setConfirm(false), [selected.length]);

  // Delete key arms the confirm, a second press (or the button) deletes.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.target as HTMLElement).tagName === "INPUT" || !selected.length) return;
      if (e.key === "Delete") {
        e.preventDefault();
        if (confirm) void del();
        else setConfirm(true);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  });

  const bytes = shown.filter((c) => selected.includes(c.path)).reduce((n, c) => n + c.bytes, 0);
  const selectable = shown.filter((c) => c.path !== recordingPath);
  const all = selectable.length > 0 && selectable.every((c) => selected.includes(c.path));

  const del = async () => {
    setBusy(true);
    const paths = [...selected];
    try {
      await api.deleteClips(paths);
      removeClips(paths);
      toast({
        tone: "info",
        title: `Moved ${paths.length} ${paths.length === 1 ? "clip" : "clips"} to the Recycle Bin`,
        body: `${formatBytes(bytes)} freed once the Recycle Bin is emptied.`,
      });
    } catch (e) {
      toast({ tone: "error", title: "Could not delete", body: errorText(e) });
      void useApp.getState().refreshClips();
    } finally {
      setBusy(false);
      setConfirm(false);
    }
  };

  const open = selected.length > 0;
  return (
    <div
      className={cx(
        "pointer-events-none absolute inset-x-0 bottom-0 flex justify-center pb-5 transition-[opacity,transform] duration-200",
        open ? "translate-y-0 opacity-100" : "translate-y-3 opacity-0",
      )}
      aria-hidden={!open}
    >
      <div
        className={cx(
          "flex items-center gap-2 rounded-card border border-line-strong bg-raised py-2 pr-2 pl-4 shadow-[0_16px_40px_-12px_rgb(0_0_0/0.75)]",
          open && "pointer-events-auto",
        )}
      >
        <span className="mr-2 text-[13.5px] font-medium">
          {selected.length} selected
          <span className="ml-2 font-mono text-[12px] font-normal text-faint">{formatBytes(bytes)}</span>
        </span>
        <Button
          size="sm"
          variant="ghost"
          onClick={() => setSelected(all ? [] : selectable.map((c) => c.path))}
          disabled={busy}
        >
          {all ? "Select none" : `Select all ${selectable.length}`}
        </Button>
        {confirm ? (
          <Button size="sm" variant="danger" onClick={del} disabled={busy}>
            <Trash size={15} />
            Move {selected.length} to Recycle Bin
          </Button>
        ) : (
          <Button size="sm" variant="danger" onClick={() => setConfirm(true)} disabled={busy} title="Delete (Del)">
            <Trash size={15} />
            Delete
          </Button>
        )}
        <button
          aria-label="Clear selection (Esc)"
          title="Clear selection (Esc)"
          onClick={() => setSelected([])}
          className="flex size-7 items-center justify-center rounded-ctl text-faint hover:bg-hover hover:text-fg"
        >
          <X size={15} />
        </button>
      </div>
    </div>
  );
}

const ClipCard = memo(function ClipCard({
  clip,
  filter,
  live,
  selected,
  selecting,
  order,
}: {
  clip: Clip;
  filter: Filter;
  live: boolean;
  selected: boolean;
  selecting: boolean;
  order: string[];
}) {
  const setView = useApp((s) => s.setView);
  const toggleSelected = useApp((s) => s.toggleSelected);
  const [hover, setHover] = useState(false);
  const [preview, setPreview] = useState(false);
  const timer = useRef<number | undefined>(undefined);

  // Hover preview starts after a short pause so sweeping the mouse across the grid stays cheap.
  useEffect(() => {
    if (hover && !live && !selecting) timer.current = window.setTimeout(() => setPreview(true), 450);
    else setPreview(false);
    return () => window.clearTimeout(timer.current);
  }, [hover, live, selecting]);

  const select = (shift: boolean) => {
    if (live) return;
    let range: string[] | null = null;
    const anchor = useApp.getState().anchor;
    if (shift && anchor && anchor !== clip.path) {
      const a = order.indexOf(anchor);
      const b = order.indexOf(clip.path);
      if (a >= 0 && b >= 0) range = order.slice(Math.min(a, b), Math.max(a, b) + 1);
    }
    toggleSelected(clip.path, range);
  };

  const onClick = (e: React.MouseEvent) => {
    if (live) return;
    // While selecting, a click toggles; Ctrl/Shift-click always selects.
    if (selecting || e.ctrlKey || e.metaKey || e.shiftKey) select(e.shiftKey);
    else setView({ page: "clip", path: clip.path, filter });
  };

  return (
    <div
      onMouseEnter={() => setHover(true)}
      onMouseLeave={() => setHover(false)}
      className="anim-rise group relative"
    >
      <button
        onClick={onClick}
        onFocus={() => setHover(true)}
        onBlur={() => setHover(false)}
        aria-pressed={selecting ? selected : undefined}
        className="block w-full text-left outline-none"
      >
        <div
          className={cx(
            "relative aspect-video overflow-hidden rounded-card border bg-raised transition-[border-color,box-shadow] duration-150",
            selected
              ? "border-accent shadow-[0_0_0_1px_var(--color-accent)]"
              : "border-line group-hover:border-line-strong group-focus-within:border-accent",
          )}
        >
          {clip.thumb ? (
            <img
              src={fileUrl(clip.thumb)}
              alt=""
              draggable={false}
              className={cx(
                "absolute inset-0 size-full object-cover transition-[transform,opacity] duration-500 ease-out",
                !selecting && "group-hover:scale-[1.03]",
                selected && "opacity-75",
              )}
            />
          ) : (
            <div className={cx("absolute inset-0 flex items-center justify-center", !live && "skeleton")}>
              {live && <span className="text-[12px] text-faint">Recording in progress</span>}
            </div>
          )}
          {preview && (
            <video
              src={fileUrl(clip.path)}
              muted
              autoPlay
              loop
              playsInline
              className="absolute inset-0 size-full object-cover"
            />
          )}
          <div className="pointer-events-none absolute inset-x-0 bottom-0 h-14 bg-gradient-to-t from-black/60 to-transparent" />
          {(clip.kind === "recording" || live) && (
            <span className="absolute bottom-2 left-2 inline-flex items-center gap-1.5 rounded-[4px] bg-black/65 px-1.5 py-0.5 text-[11px] font-medium text-fg">
              <span className={cx("size-1.5 rounded-full bg-rec", live && "anim-rec")} />
              {live ? "Live" : "Recording"}
            </span>
          )}
          {formatDuration(clip.duration) && (
            <span className="absolute right-2 bottom-2 rounded-[4px] bg-black/65 px-1.5 py-0.5 font-mono text-[11px] text-fg">
              {formatDuration(clip.duration)}
            </span>
          )}
        </div>
        <div className="mt-2.5 flex items-start gap-2.5 px-0.5">
          {clip.game && <GameTile name={clip.game} size={22} />}
          <div className="min-w-0 flex-1">
            <div className="truncate text-[13.5px] font-medium text-fg">{clipTitle(clip)}</div>
            <div className="mt-0.5 truncate text-[12px] text-faint">
              {clip.game ?? "Desktop"} · {formatTime(clip.modified)} · {formatBytes(clip.bytes)}
            </div>
          </div>
        </div>
      </button>

      {!live && (
        <button
          role="checkbox"
          aria-checked={selected}
          aria-label={selected ? "Deselect clip" : "Select clip"}
          onClick={(e) => select(e.shiftKey)}
          className={cx(
            "absolute top-2 left-2 flex size-6 items-center justify-center rounded-ctl border transition-[opacity,background-color,border-color] duration-150",
            selected
              ? "border-accent bg-accent text-accent-ink opacity-100"
              : "border-white/50 bg-black/45 text-transparent hover:border-white hover:bg-black/60",
            !selected && !selecting && "opacity-0 group-hover:opacity-100 focus-visible:opacity-100",
          )}
        >
          <Check size={14} weight="bold" />
        </button>
      )}
    </div>
  );
});

function Skeleton() {
  return (
    <div className="grid grid-cols-[repeat(auto-fill,minmax(250px,1fr))] gap-x-4 gap-y-6 pt-8">
      {Array.from({ length: 8 }, (_, i) => (
        <div key={i}>
          <div className="skeleton aspect-video rounded-card border border-line bg-raised" />
          <div className="skeleton mt-3 h-3.5 w-2/3 rounded bg-raised" />
          <div className="skeleton mt-2 h-3 w-1/3 rounded bg-raised" />
        </div>
      ))}
    </div>
  );
}

function Empty({ filter, searching }: { filter: Filter; searching: boolean }) {
  const settings = useApp((s) => s.settings);
  if (searching) {
    return <p className="pt-16 text-center text-[14px] text-muted">No clips match that search.</p>;
  }
  return (
    <div className="flex max-w-[520px] flex-col items-start pt-16 pl-2">
      <FilmSlate size={34} className="text-faint" />
      <h2 className="mt-4 text-[20px] font-semibold tracking-tight">
        {filter.kind === "recordings" ? "No recordings yet" : "No clips yet"}
      </h2>
      <p className="mt-2 text-[14px] leading-relaxed text-muted">
        {filter.kind === "recordings"
          ? "Start a recording from the sidebar or with the record hotkey. It stops when you press it again."
          : `Something worth keeping just happened? Press the save hotkey and the last ${settings?.replay_seconds ?? 30} seconds land here.`}
      </p>
      {settings && (
        <div className="mt-5 flex items-center gap-3 text-[13px] text-muted">
          <Combo combo={filter.kind === "recordings" ? settings.hotkeys.record : settings.hotkeys.save} />
          <span>{filter.kind === "recordings" ? "start / stop recording" : "save a clip"}</span>
        </div>
      )}
    </div>
  );
}
