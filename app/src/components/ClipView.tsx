import { useEffect, useMemo, useRef, useState } from "react";
import { revealItemInDir } from "@tauri-apps/plugin-opener";
import { ArrowLeft, CaretLeft, CaretRight, Copy, DiscordLogo, FolderOpen, PencilSimple, Trash } from "@phosphor-icons/react";
import { api, errorText, fileUrl } from "../lib/api";
import { useApp, type Filter } from "../lib/store";
import { DISCORD_FREE_BYTES, clipTitle, formatBytes, formatDateTime, formatDuration } from "../lib/format";
import { filterClips } from "./Library";
import { TopBar } from "./TopBar";
import { Button, GameTile, IconButton, KeyCap } from "./ui";

export function ClipView({ path, filter }: { path: string; filter: Filter }) {
  const clips = useApp((s) => s.clips);
  const search = useApp((s) => s.search);
  const setView = useApp((s) => s.setView);
  const toast = useApp((s) => s.toast);
  const replaceClipPath = useApp((s) => s.replaceClipPath);
  const removeClip = useApp((s) => s.removeClip);

  const list = useMemo(() => filterClips(clips, filter, search), [clips, filter, search]);
  const index = list.findIndex((c) => c.path === path);
  const clip = list[index] ?? clips.find((c) => c.path === path);
  const prev = index > 0 ? list[index - 1] : undefined;
  const next = index >= 0 && index < list.length - 1 ? list[index + 1] : undefined;

  const [renaming, setRenaming] = useState(false);
  const [name, setName] = useState("");
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [res, setRes] = useState<string | null>(null);
  const video = useRef<HTMLVideoElement>(null);

  const back = () => setView({ page: "library", filter });
  const goTo = (p: string) => setView({ page: "clip", path: p, filter });

  useEffect(() => {
    setConfirmDelete(false);
    setRenaming(false);
    setRes(null);
  }, [path]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.target as HTMLElement).tagName === "INPUT") return;
      if (e.key === "Escape") back();
      if (e.key === "ArrowLeft" && e.altKey && prev) goTo(prev.path);
      if (e.key === "ArrowRight" && e.altKey && next) goTo(next.path);
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
      toast({
        tone: "saved",
        title: "Copied to clipboard",
        body: "Paste it into any Discord chat with Ctrl+V.",
      });
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
      await api.deleteClip(clip.path);
      const target = next ?? prev;
      removeClip(clip.path);
      toast({ tone: "info", title: "Moved to the Recycle Bin" });
      if (target) goTo(target.path);
      else back();
    } catch (e) {
      toast({ tone: "error", title: "Could not delete", body: errorText(e) });
    }
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
        <div className="flex min-w-0 flex-1 items-center justify-center bg-black/40 p-6">
          <video
            key={clip.path}
            ref={video}
            src={fileUrl(clip.path)}
            controls
            autoPlay
            onLoadedMetadata={(e) => setRes(`${e.currentTarget.videoWidth}×${e.currentTarget.videoHeight}`)}
            className="max-h-full max-w-full rounded-card bg-black shadow-[0_20px_60px_-20px_rgb(0_0_0/0.8)]"
          />
        </div>

        <aside className="flex w-[340px] shrink-0 flex-col overflow-y-auto border-l border-line bg-panel">
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
              <button
                onClick={() => {
                  setName(clip.title);
                  setRenaming(true);
                }}
                className="group flex w-full items-start gap-2 text-left"
                title="Rename"
              >
                <h2 className="min-w-0 flex-1 text-[17px] leading-snug font-semibold tracking-tight break-words">
                  {clipTitle(clip)}
                </h2>
                <PencilSimple size={15} className="mt-1 shrink-0 text-faint opacity-0 transition-opacity group-hover:opacity-100" />
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
            <Button variant="ghost" className="justify-start" onClick={() => revealItemInDir(clip.path)}>
              <FolderOpen size={17} />
              Show in folder
            </Button>
            <Button
              variant="ghost"
              className="justify-start"
              onClick={() => {
                setName(clip.title);
                setRenaming(true);
              }}
            >
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
