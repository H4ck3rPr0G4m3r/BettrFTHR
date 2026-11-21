import { useEffect, useMemo, useRef, useState } from "react";
import { ArrowLeft, CaretLeft, CaretRight, Copy, DiscordLogo, FolderOpen, PencilSimple, Trash } from "@phosphor-icons/react";
import { api, errorText, fileUrl, type Clip } from "../lib/api";
import { useApp, type Filter } from "../lib/store";
import { DISCORD_FREE_BYTES, clipTitle, formatBytes, formatDateTime, formatDuration } from "../lib/format";
import { ensureThumbs } from "../lib/thumbs";
import { filterClips } from "./Library";
import { Player, type PlayerHandle } from "./Player";
import { TopBar } from "./TopBar";
import { Button, GameTile, IconButton, KeyCap, cx } from "./ui";

export function ClipView({ path, filter }: { path: string; filter: Filter }) {
  const clips = useApp((s) => s.clips);
  const search = useApp((s) => s.search);
  const setView = useApp((s) => s.setView);
  const toast = useApp((s) => s.toast);
  const replaceClipPath = useApp((s) => s.replaceClipPath);
  const removeClips = useApp((s) => s.removeClips);
  const recordingPath = useApp((s) => s.engine?.recording ?? null);

  const list = useMemo(
    () => filterClips(clips, filter, search).filter((c) => c.path !== recordingPath),
    [clips, filter, search, recordingPath],
  );
  const index = list.findIndex((c) => c.path === path);
  const clip = list[index] ?? clips.find((c) => c.path === path);
  const prev = index > 0 ? list[index - 1] : undefined;
  const next = index >= 0 && index < list.length - 1 ? list[index + 1] : undefined;

  const [renaming, setRenaming] = useState(false);
  const [name, setName] = useState("");
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [res, setRes] = useState<string | null>(null);
  const player = useRef<PlayerHandle>(null);

  const back = () => setView({ page: "library", filter });
  const goTo = (p: string) => setView({ page: "clip", path: p, filter });

  useEffect(() => {
    setConfirmDelete(false);
    setRenaming(false);
    setRes(null);
  }, [path]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const tag = (e.target as HTMLElement).tagName;
      if (tag === "INPUT" || tag === "TEXTAREA") return;
      const p = player.current;
      if (e.altKey && e.key === "ArrowLeft") {
        e.preventDefault();
        if (prev) goTo(prev.path);
      } else if (e.altKey && e.key === "ArrowRight") {
        e.preventDefault();
        if (next) goTo(next.path);
      } else if (e.key === " " || e.key === "k") {
        e.preventDefault();
        p?.toggle();
      } else if (e.key === "ArrowLeft") {
        e.preventDefault();
        p?.seekBy(e.shiftKey ? -5 : -1);
      } else if (e.key === "ArrowRight") {
        e.preventDefault();
        p?.seekBy(e.shiftKey ? 5 : 1);
      } else if (e.key === ",") p?.step(-1);
      else if (e.key === ".") p?.step(1);
      else if (e.key === "Home") p?.seekTo(0);
      else if (e.key === "End") p?.seekTo(1e9);
      else if (e.key === "m") p?.toggleMute();
      else if (e.key === "f") p?.toggleFullscreen();
      else if (e.key === "Escape" && !document.fullscreenElement) back();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  });

  if (!clip) {
    return (
      <div className="flex min-w-0 flex-1 flex-col">
        <TopBar title={<h1 className="text-[17px] font-semibold">Clip not found</h1>} search={false} />
        <div className="p-8">
          <p className="text-muted">It may have been renamed or deleted outside the app.</p>
          <Button className="mt-4" onClick={back}>
            Back to library
          </Button>
        </div>
      </div>
    );
  }

  const tooBig = clip.bytes > DISCORD_FREE_BYTES;

  const copy = async () => {
    try {
      await api.copyClip(clip.path);
      toast({ tone: "saved", title: "Copied to clipboard", body: "Paste it into any Discord chat with Ctrl+V." });
    } catch (e) {
      toast({ tone: "error", title: "Could not copy", body: errorText(e) });
    }
  };

  const saveName = async () => {
    const t = name.trim();
    setRenaming(false);
    if (!t || t === clip.title) return;
    try {
      const newPath = await api.renameClip(clip.path, t);
      replaceClipPath(clip.path, newPath, t);
    } catch (e) {
      toast({ tone: "error", title: "Could not rename", body: errorText(e) });
    }
  };

  const del = async () => {
    if (!confirmDelete) {
      setConfirmDelete(true);
      return;
    }
    try {
      await api.deleteClips([clip.path]);
      const target = next ?? prev;
      removeClips([clip.path]);
      toast({ tone: "info", title: "Moved to the Recycle Bin" });
      if (target) goTo(target.path);
      else back();
    } catch (e) {
      toast({ tone: "error", title: "Could not delete", body: errorText(e) });
    }
  };

  const startRename = () => {
    setName(clip.title);
    setRenaming(true);
  };

  return (
    <div className="flex min-w-0 flex-1 flex-col">
      <TopBar
        search={false}
        title={
          <span className="pointer-events-auto flex items-center gap-2">
            <IconButton label="Back to library (Esc)" onClick={back}>
              <ArrowLeft size={17} />
            </IconButton>
            <h1 className="truncate text-[15px] font-medium text-muted">
              {filter.kind === "game" ? filter.game : filter.kind === "recordings" ? "Recordings" : "All clips"}
            </h1>
          </span>
        }
      >
        <div className="flex items-center gap-1">
          <IconButton label="Previous clip (Alt+Left)" disabled={!prev} onClick={() => prev && goTo(prev.path)}>
            <CaretLeft size={16} />
          </IconButton>
          {index >= 0 && (
            <span className="font-mono text-[12px] text-faint">
              {index + 1}/{list.length}
            </span>
          )}
          <IconButton label="Next clip (Alt+Right)" disabled={!next} onClick={() => next && goTo(next.path)}>
            <CaretRight size={16} />
          </IconButton>
        </div>
      </TopBar>

      <div className="flex min-h-0 flex-1">
        <div className="flex min-w-0 flex-1 flex-col">
          <Player
            key={clip.path}
            ref={player}
            src={fileUrl(clip.path)}
            onDuration={() => {
              const v = player.current?.video();
              if (v) setRes(`${v.videoWidth}×${v.videoHeight}`);
            }}
          />
        </div>

        <aside className="flex w-[320px] shrink-0 flex-col overflow-y-auto border-l border-line bg-panel">
          <div className="border-b border-line p-5">
            {renaming ? (
              <input
                autoFocus
                value={name}
                onChange={(e) => setName(e.target.value)}
                onBlur={saveName}
                onKeyDown={(e) => {
                  if (e.key === "Enter") void saveName();
                  if (e.key === "Escape") setRenaming(false);
                }}
                aria-label="Clip name"
                className="h-9 w-full rounded-ctl border border-accent bg-raised px-2.5 text-[15px] font-semibold text-fg outline-none"
              />
            ) : (
              <button onClick={startRename} className="group flex w-full items-start gap-2 text-left" title="Rename">
                <h2 className="min-w-0 flex-1 text-[17px] leading-snug font-semibold tracking-tight break-words">
                  {clipTitle(clip)}
                </h2>
                <PencilSimple
                  size={15}
                  className="mt-1 shrink-0 text-faint opacity-0 transition-opacity group-hover:opacity-100"
                />
              </button>
            )}
            <div className="mt-2.5 flex items-center gap-2 text-[13px] text-muted">
              {clip.game && <GameTile name={clip.game} size={20} />}
              <span>{clip.game ?? "Desktop"}</span>
            </div>

            <dl className="mt-5 grid grid-cols-2 gap-x-4 gap-y-3 text-[12.5px]">
              <Meta label="Saved" value={formatDateTime(clip.modified)} wide />
              <Meta label="Length" value={formatDuration(clip.duration) || "-"} />
              <Meta label="Size" value={formatBytes(clip.bytes)} />
              <Meta label="Resolution" value={res ?? "-"} />
              <Meta label="Type" value={clip.kind === "recording" ? "Recording" : "Replay clip"} />
            </dl>
          </div>

          <div className="border-b border-line p-5">
            <div className="flex items-center gap-2 text-[13px] font-medium">
              <DiscordLogo size={17} weight="fill" className="text-[#8c9eff]" />
              Share to Discord
            </div>
            <Button variant="primary" size="lg" className="mt-3 w-full" onClick={copy}>
              <Copy size={17} weight="bold" />
              Copy clip
            </Button>
            <p className="mt-2.5 text-[12.5px] leading-relaxed text-faint">
              Then press <KeyCap>Ctrl</KeyCap> <KeyCap>V</KeyCap> in any Discord chat.
            </p>
            {tooBig && (
              <p className="mt-3 rounded-ctl border border-warn/25 bg-warn/8 px-3 py-2 text-[12.5px] leading-relaxed text-warn">
                {formatBytes(clip.bytes)} is over Discord's 10 MB limit for accounts without Nitro.
              </p>
            )}
          </div>

          <div className="flex flex-col gap-1 p-3">
            <Button
              variant="ghost"
              className="justify-start"
              onClick={() =>
                api.revealClip(clip.path).catch((e) => toast({ tone: "error", title: "Could not open", body: errorText(e) }))
              }
            >
              <FolderOpen size={17} />
              Show in folder
            </Button>
            <Button variant="ghost" className="justify-start" onClick={startRename}>
              <PencilSimple size={17} />
              Rename
            </Button>
            <Button
              variant={confirmDelete ? "danger" : "ghost"}
              className="justify-start"
              onClick={del}
              onBlur={() => setConfirmDelete(false)}
            >
              <Trash size={17} />
              {confirmDelete ? "Click again to move to Recycle Bin" : "Delete"}
            </Button>
          </div>
        </aside>
      </div>

      <Filmstrip clips={list} current={clip.path} onPick={goTo} />
    </div>
  );
}

/** The other clips in the same view, along the bottom: jump between them without going back. */
function Filmstrip({ clips, current, onPick }: { clips: Clip[]; current: string; onPick: (p: string) => void }) {
  const strip = useRef<HTMLDivElement>(null);
  const items = useRef(new Map<string, HTMLButtonElement>());

  useEffect(() => {
    ensureThumbs(clips, null);
  }, [clips]);

  useEffect(() => {
    const el = items.current.get(current);
    const reduce = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
    el?.scrollIntoView({ inline: "center", block: "nearest", behavior: reduce ? "auto" : "smooth" });
  }, [current]);

  if (clips.length <= 1) return null;

  return (
    <div
      ref={strip}
      onWheel={(e) => {
        // Vertical wheel scrolls the strip sideways.
        if (strip.current && Math.abs(e.deltaY) > Math.abs(e.deltaX)) strip.current.scrollLeft += e.deltaY;
      }}
      className="flex h-[150px] shrink-0 gap-3 overflow-x-auto overflow-y-hidden border-t border-line bg-panel px-5 pt-3 pb-2"
      aria-label="Other clips"
    >
      {clips.map((c) => {
        const on = c.path === current;
        return (
          <button
            key={c.path}
            ref={(el) => {
              if (el) items.current.set(c.path, el);
              else items.current.delete(c.path);
            }}
            onClick={() => onPick(c.path)}
            aria-current={on ? "true" : undefined}
            className="group w-[160px] shrink-0 text-left outline-none"
          >
            <div
              className={cx(
                "relative aspect-video overflow-hidden rounded-ctl border bg-raised transition-[border-color,opacity] duration-150",
                on ? "border-accent shadow-[0_0_0_1px_var(--color-accent)]" : "border-line opacity-70 group-hover:opacity-100 group-hover:border-line-strong",
              )}
            >
              {c.thumb ? (
                <img src={fileUrl(c.thumb)} alt="" draggable={false} className="absolute inset-0 size-full object-cover" />
              ) : (
                <div className="skeleton absolute inset-0" />
              )}
              {formatDuration(c.duration) && (
                <span className="absolute right-1 bottom-1 rounded-[3px] bg-black/70 px-1 font-mono text-[10.5px] text-fg">
                  {formatDuration(c.duration)}
                </span>
              )}
            </div>
            <div className={cx("mt-1.5 truncate text-[12px]", on ? "text-fg" : "text-muted")}>{clipTitle(c)}</div>
          </button>
        );
      })}
    </div>
  );
}

function Meta({ label, value, wide }: { label: string; value: string; wide?: boolean }) {
  return (
    <div className={wide ? "col-span-2" : undefined}>
      <dt className="text-faint">{label}</dt>
      <dd className="mt-0.5 font-mono text-[12.5px] text-fg">{value}</dd>
    </div>
  );
}
