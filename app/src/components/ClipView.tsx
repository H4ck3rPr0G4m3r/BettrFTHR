import { useEffect, useMemo, useRef, useState } from "react";
import {
  ArrowLeft,
  CaretLeft,
  CaretRight,
  DiscordLogo,
  FolderOpen,
  PencilSimple,
  Scissors,
  Trash,
} from "@phosphor-icons/react";
import { api, errorText, fileUrl, type Clip, type Probe } from "../lib/api";
import {
  EditPanel,
  TrimOverlay,
  Tracks,
  initialEdit,
  isDirty,
  snapStart,
  useExportProgress,
  type EditState,
} from "./Editor";
import { useApp, type Filter } from "../lib/store";
import { clipTitle, discordLimitBytes, formatBytes, formatDateTime, formatDuration } from "../lib/format";
import { ensureThumbs } from "../lib/thumbs";
import { filterClips } from "./Library";
import { Player, type PlayerHandle } from "./Player";
import { TopBar } from "./TopBar";
import { Button, GameTile, IconButton, cx } from "./ui";
import { ResizeHandle } from "./ResizeHandle";
import { PANEL, useLayout } from "../lib/layout";

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
  const panel = useLayout((s) => s.panel);
  const setPanel = useLayout((s) => s.setPanel);
  const sidebar = useLayout((s) => s.sidebar);
  const [editing, setEditing] = useState(false);
  const [probe, setProbe] = useState<Probe | null>(null);
  const [edit, setEdit] = useState<EditState | null>(null);
  const [busy, setBusy] = useState<"library" | "discord" | null>(null);
  const [copying, setCopying] = useState(false);
  const copyProgress = useExportProgress(copying);
  const limitMb = useApp((s) => s.settings?.discord_limit_mb ?? 10);
  const limit = discordLimitBytes(limitMb);

  const back = () => setView({ page: "library", filter });
  const goTo = (p: string) => setView({ page: "clip", path: p, filter });

  useEffect(() => {
    setConfirmDelete(false);
    setRenaming(false);
    setRes(null);
    setEditing(false);
    setProbe(null);
    setEdit(null);
  }, [path]);

  const startEditing = async () => {
    setEditing(true);
    try {
      const p = probe ?? (await api.probeClip(path));
      setProbe(p);
      setEdit((e) => e ?? initialEdit(p));
    } catch (e) {
      setEditing(false);
      toast({ tone: "error", title: "Could not open the editor", body: errorText(e) });
    }
  };

  // While editing, playback stays inside the trimmed range.
  useEffect(() => {
    if (!editing || !edit) return;
    const v = player.current?.video();
    if (!v) return;
    // Watches every frame while playing only; a paused editor schedules nothing.
    let raf = 0;
    const loop = () => {
      if (v.paused) return; // the next "play" restarts it
      if (v.currentTime >= edit.end) {
        v.pause();
        v.currentTime = edit.end;
        return;
      }
      raf = requestAnimationFrame(loop);
    };
    const onPlay = () => {
      if (v.currentTime < edit.start - 0.05 || v.currentTime >= edit.end - 0.05) v.currentTime = edit.start;
      cancelAnimationFrame(raf);
      raf = requestAnimationFrame(loop);
    };
    if (!v.paused) raf = requestAnimationFrame(loop);
    v.addEventListener("play", onPlay);
    return () => {
      cancelAnimationFrame(raf);
      v.removeEventListener("play", onPlay);
    };
  }, [editing, edit]);

  const runExport = async (target: "library" | "discord") => {
    if (!edit || !probe || !clip) return;
    setBusy(target);
    try {
      const res = await api.exportClip({
        input: clip.path,
        start: edit.start,
        end: edit.end,
        target,
        sources: edit.sources.map((s) => ({ gain: s.muted ? 0 : s.gain, mutes: s.mutes })),
      });
      if (target === "discord") {
        toast({
          tone: "saved",
          title: "Edit copied to clipboard",
          body: `${formatBytes(res.bytes)}${res.reencoded ? `, compressed to ${res.reencoded.height}p` : ""}. Paste it into Discord with Ctrl+V.`,
        });
      } else {
        await useApp.getState().refreshClips();
        setEditing(false);
        toast({
          tone: "saved",
          title: "Saved as a new clip",
          body: `${Math.round(res.seconds)} seconds, ${formatBytes(res.bytes)}`,
          action: { label: "View", run: () => goTo(res.path) },
        });
      }
    } catch (e) {
      toast({ tone: "error", title: "Export failed", body: errorText(e) });
    } finally {
      setBusy(null);
    }
  };

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const tag = (e.target as HTMLElement).tagName;
      if (tag === "INPUT" || tag === "TEXTAREA") return;
      const p = player.current;
      const v = p?.video();
      if (editing && edit && probe && v && (e.key === "i" || e.key === "o")) {
        e.preventDefault();
        if (e.key === "i") setEdit({ ...edit, start: Math.min(snapStart(v.currentTime, probe.keyframes), edit.end - 0.5) });
        else setEdit({ ...edit, end: Math.max(edit.start + 0.5, v.currentTime) });
        return;
      }
      if (e.key === "e" && !editing && !e.ctrlKey) {
        void startEditing();
        return;
      }
      if (e.key === "Escape" && editing && !document.fullscreenElement) {
        if (!busy) setEditing(false);
        return;
      }
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

  const tooBig = clip.bytes > limit;

  // Small enough: the file itself goes on the clipboard. Too big for the user's Discord
  // limit: a compressed copy is made first.
  const copy = async () => {
    try {
      if (!tooBig) {
        await api.copyClip(clip.path);
        toast({ tone: "saved", title: "Copied to clipboard", body: "Paste it into any Discord chat with Ctrl+V." });
        return;
      }
      setCopying(true);
      const res = await api.exportClip({ input: clip.path, start: 0, end: 1e9, sources: [], target: "discord" });
      toast({
        tone: "saved",
        title: "Compressed copy on the clipboard",
        body: `${formatBytes(res.bytes)}${res.reencoded ? ` at ${res.reencoded.height}p${res.reencoded.fps}` : ""}. Paste it into Discord with Ctrl+V.`,
      });
    } catch (e) {
      toast({ tone: "error", title: "Could not copy", body: errorText(e) });
    } finally {
      setCopying(false);
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
            overlay={(duration) =>
              editing && probe && edit ? (
                <TrimOverlay
                  duration={duration}
                  edit={edit}
                  keyframes={probe.keyframes}
                  onChange={(start, end) => setEdit({ ...edit, start, end })}
                />
              ) : null
            }
          />
          {editing &&
            (probe && edit ? (
              <Tracks probe={probe} edit={edit} setEdit={setEdit} video={() => player.current?.video() ?? null} />
            ) : (
              <div className="shrink-0 border-t border-line bg-panel px-5 py-3">
                <div className="skeleton h-10 rounded-ctl bg-raised" />
                <div className="skeleton mt-3 h-10 rounded-ctl bg-raised" />
              </div>
            ))}
        </div>

        <aside
          className="relative flex shrink-0 flex-col border-l border-line bg-panel"
          // The player keeps at least 380 px; on a narrow window the panel gives way.
          style={{ width: `max(${PANEL.min}px, min(${panel}px, calc(100vw - ${sidebar}px - 380px)))` }}
        >
          <ResizeHandle
            edge="left"
            width={panel}
            onWidth={setPanel}
            onReset={() => setPanel(PANEL.default)}
            label="Resize clip panel"
          />
          <div className="flex min-h-0 flex-1 flex-col overflow-y-auto">
          {editing ? (
            probe && edit ? (
              <EditPanel
                probe={probe}
                edit={edit}
                bytes={clip.bytes}
                limit={limit}
                busy={busy}
                onSave={() => runExport("library")}
                onDiscord={() => runExport("discord")}
                onReset={() => setEdit(initialEdit(probe))}
                onCancel={() => setEditing(false)}
              />
            ) : (
              <div className="p-5 text-[13px] text-muted">Reading the clip's audio tracks</div>
            )
          ) : (
          <>
          <div className="border-b border-line px-5 pt-4 pb-4">
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
            <div className="mt-2 flex min-w-0 items-center gap-2 text-[13px] text-muted">
              {clip.game && <GameTile name={clip.game} size={18} />}
              <span className="truncate">{clip.game ?? "Desktop"}</span>
              <span className="text-faint">·</span>
              <span className="shrink-0 text-faint">{formatDateTime(clip.modified)}</span>
            </div>

            <dl className="mt-4 grid grid-cols-3 gap-x-3 text-[12px]">
              <Meta label="Length" value={formatDuration(clip.duration) || "-"} />
              <Meta label="Size" value={formatBytes(clip.bytes)} />
              <Meta label="Resolution" value={res ? res.replace("×", "x") : "-"} />
            </dl>
          </div>

          <div className="border-b border-line px-5 py-4">
            <Button variant="primary" size="lg" className="w-full" onClick={copy} disabled={copying}>
              <DiscordLogo size={18} weight="fill" />
              {copying ? `Compressing ${Math.round(copyProgress * 100)}%` : "Copy for Discord"}
            </Button>
            {copying && (
              <div className="mt-2 h-1 overflow-hidden rounded-full bg-line">
                <div className="h-full bg-accent transition-[width] duration-150" style={{ width: `${copyProgress * 100}%` }} />
              </div>
            )}
            <p className="mt-2 text-[12px] leading-relaxed text-faint">
              {tooBig
                ? `Over your ${limitMb} MB limit, so the copy is compressed to fit. Then press Ctrl+V in Discord.`
                : "Then press Ctrl+V in any Discord chat."}
            </p>
            <Button className="mt-3 w-full" onClick={startEditing} title="Edit (E)">
              <Scissors size={16} />
              Trim and edit audio
              {edit && probe && isDirty(edit, probe) && <span className="text-[11.5px] text-accent">unsaved</span>}
            </Button>
          </div>

          <div className="grid grid-cols-3 gap-1 p-2">
            <ActionButton
              icon={<FolderOpen size={17} />}
              label="Folder"
              title="Show in folder"
              onClick={() =>
                api.revealClip(clip.path).catch((e) => toast({ tone: "error", title: "Could not open", body: errorText(e) }))
              }
            />
            <ActionButton icon={<PencilSimple size={17} />} label="Rename" onClick={startRename} />
            <ActionButton
              icon={<Trash size={17} />}
              label={confirmDelete ? "Confirm" : "Delete"}
              title={confirmDelete ? "Click again to move to the Recycle Bin" : "Move to the Recycle Bin"}
              danger={confirmDelete}
              onClick={del}
              onBlur={() => setConfirmDelete(false)}
            />
          </div>
          </>
          )}
          </div>
        </aside>
      </div>

      {!editing && <Filmstrip clips={list} current={clip.path} onPick={goTo} />}
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
      className="no-scrollbar flex h-[132px] shrink-0 gap-3 overflow-x-auto overflow-y-hidden border-t border-line bg-panel px-5 py-3 [@media(max-height:820px)]:h-[118px] [@media(max-height:820px)]:py-2"
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
            className="group w-[160px] shrink-0 text-left outline-none [@media(max-height:820px)]:w-[132px]"
          >
            <div
              className={cx(
                "relative aspect-video overflow-hidden rounded-ctl border bg-raised transition-[border-color,opacity] duration-150",
                on ? "border-accent shadow-[0_0_0_1px_var(--color-accent)]" : "border-line opacity-70 group-hover:opacity-100 group-hover:border-line-strong",
              )}
            >
              {c.thumb ? (
                <img
                  src={fileUrl(c.thumb)}
                  alt=""
                  draggable={false}
                  loading="lazy"
                  decoding="async"
                  className="absolute inset-0 size-full object-cover"
                />
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

function ActionButton({
  icon,
  label,
  title,
  danger,
  onClick,
  onBlur,
}: {
  icon: React.ReactNode;
  label: string;
  title?: string;
  danger?: boolean;
  onClick: () => void;
  onBlur?: () => void;
}) {
  return (
    <button
      onClick={onClick}
      onBlur={onBlur}
      title={title ?? label}
      className={cx(
        "flex h-14 flex-col items-center justify-center gap-1 rounded-ctl text-[12px] transition-colors",
        danger ? "bg-rec/12 text-rec" : "text-muted hover:bg-hover hover:text-fg",
      )}
    >
      {icon}
      {label}
    </button>
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
