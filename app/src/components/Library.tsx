import { memo, useEffect, useMemo, useRef, useState } from "react";
import { FilmSlate } from "@phosphor-icons/react";
import { fileUrl, type Clip } from "../lib/api";
import { useApp, type Filter } from "../lib/store";
import { clipTitle, formatBytes, formatDuration, formatTime, groupByDay } from "../lib/format";
import { ensureThumbs } from "../lib/thumbs";
import { TopBar } from "./TopBar";
import { Combo, GameTile, cx } from "./ui";

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

  const shown = useMemo(() => filterClips(clips, filter, search), [clips, filter, search]);
  const groups = useMemo(() => groupByDay(shown), [shown]);

  useEffect(() => {
    ensureThumbs(shown, recordingPath);
  }, [shown, recordingPath]);

  const totalBytes = shown.reduce((n, c) => n + c.bytes, 0);
  const title =
    filter.kind === "all" ? "All clips" : filter.kind === "recordings" ? "Recordings" : filter.game;

  return (
    <div className="flex min-w-0 flex-1 flex-col">
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
      <div className="min-h-0 flex-1 overflow-y-auto px-6 pt-5 pb-10">
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
                  <ClipCard key={c.path} clip={c} filter={filter} live={c.path === recordingPath} />
                ))}
              </div>
            </section>
          ))
        )}
      </div>
    </div>
  );
}

const ClipCard = memo(function ClipCard({ clip, filter, live }: { clip: Clip; filter: Filter; live: boolean }) {
  const setView = useApp((s) => s.setView);
  const [hover, setHover] = useState(false);
  const [preview, setPreview] = useState(false);
  const timer = useRef<number | undefined>(undefined);

  // Hover preview starts after a short pause so sweeping the mouse across the grid stays cheap.
  useEffect(() => {
    if (hover && !live) timer.current = window.setTimeout(() => setPreview(true), 450);
    else setPreview(false);
    return () => window.clearTimeout(timer.current);
  }, [hover, live]);

  const open = () => !live && setView({ page: "clip", path: clip.path, filter });

  return (
    <button
      onClick={open}
      onMouseEnter={() => setHover(true)}
      onMouseLeave={() => setHover(false)}
      onFocus={() => setHover(true)}
      onBlur={() => setHover(false)}
      className="anim-rise group text-left outline-none"
    >
      <div
        className={cx(
          "relative aspect-video overflow-hidden rounded-card border bg-raised transition-[border-color,transform] duration-200",
          "border-line group-hover:border-line-strong group-focus-visible:border-accent",
        )}
      >
        {clip.thumb ? (
          <img
            src={fileUrl(clip.thumb)}
            alt=""
            draggable={false}
            className="absolute inset-0 size-full object-cover transition-transform duration-500 ease-out group-hover:scale-[1.03]"
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
          <span className="absolute top-2 left-2 inline-flex items-center gap-1.5 rounded-ctl bg-black/60 px-1.5 py-0.5 text-[11px] font-medium text-fg backdrop-blur-sm">
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
