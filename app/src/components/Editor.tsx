import { useEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import {
  ArrowCounterClockwise,
  DiscordLogo,
  FloppyDisk,
  Microphone,
  SpeakerHigh,
  SpeakerSlash,
  X,
} from "@phosphor-icons/react";
import type { Probe } from "../lib/api";
import { DISCORD_FREE_BYTES, formatBytes } from "../lib/format";
import { preciseTime } from "./Player";
import { Button, KeyCap, cx } from "./ui";

export interface SourceState {
  gain: number;
  muted: boolean;
  mutes: [number, number][];
}

export interface EditState {
  start: number;
  end: number;
  sources: SourceState[];
}

export function initialEdit(p: Probe): EditState {
  return {
    start: 0,
    end: p.duration,
    sources: p.sources.map(() => ({ gain: 1, muted: false, mutes: [] })),
  };
}

export function isDirty(e: EditState, p: Probe) {
  return (
    e.start > 0.01 ||
    e.end < p.duration - 0.01 ||
    e.sources.some((s) => s.gain !== 1 || s.muted || s.mutes.length > 0)
  );
}

/** Latest keyframe at or before `t`: where a lossless cut actually starts. */
export function snapStart(t: number, keyframes: number[]) {
  let best = 0;
  for (const k of keyframes) if (k <= t + 1e-6) best = k;
  return best;
}

/** Merges overlapping or touching ranges and drops tiny ones. */
function normalize(ranges: [number, number][]): [number, number][] {
  const sorted = ranges
    .map(([a, b]) => [Math.min(a, b), Math.max(a, b)] as [number, number])
    .filter(([a, b]) => b - a >= 0.05)
    .sort((x, y) => x[0] - y[0]);
  const out: [number, number][] = [];
  for (const r of sorted) {
    const last = out[out.length - 1];
    if (last && r[0] <= last[1] + 0.02) last[1] = Math.max(last[1], r[1]);
    else out.push([...r]);
  }
  return out;
}

const pct = (t: number, d: number) => `${(Math.max(0, Math.min(d, t)) / d) * 100}%`;

/** Trim handles and dimmed outside regions, drawn on the player's timeline. */
export function TrimOverlay({
  duration,
  edit,
  keyframes,
  onChange,
}: {
  duration: number;
  edit: EditState;
  keyframes: number[];
  onChange: (start: number, end: number) => void;
}) {
  const box = useRef<HTMLDivElement>(null);
  if (duration <= 0) return null;

  const drag = (which: "start" | "end") => (e: React.PointerEvent) => {
    e.stopPropagation();
    e.preventDefault();
    const el = e.currentTarget as HTMLElement;
    el.setPointerCapture(e.pointerId);
    const move = (ev: PointerEvent) => {
      const r = box.current!.getBoundingClientRect();
      const t = Math.max(0, Math.min(1, (ev.clientX - r.left) / r.width)) * duration;
      if (which === "start") onChange(Math.min(snapStart(t, keyframes), edit.end - 0.5), edit.end);
      else onChange(edit.start, Math.max(edit.start + 0.5, Math.min(duration, t)));
    };
    const up = () => {
      el.removeEventListener("pointermove", move);
      el.removeEventListener("pointerup", up);
    };
    el.addEventListener("pointermove", move);
    el.addEventListener("pointerup", up);
  };

  const handle = (which: "start" | "end") => (
    <div
      role="slider"
      aria-label={which === "start" ? "Clip start" : "Clip end"}
      aria-valuenow={which === "start" ? edit.start : edit.end}
      onPointerDown={drag(which)}
      className={cx(
        "pointer-events-auto absolute top-0 bottom-0 z-10 flex w-3 cursor-ew-resize items-center justify-center bg-accent",
        which === "start" ? "-translate-x-full rounded-l-ctl" : "rounded-r-ctl",
      )}
      style={{ left: pct(which === "start" ? edit.start : edit.end, duration) }}
    >
      <span className="h-3.5 w-px bg-accent-ink/60" />
    </div>
  );

  return (
    <div ref={box} className="absolute inset-0">
      <div className="absolute inset-y-0 left-0 bg-bg/75" style={{ width: pct(edit.start, duration) }} />
      <div className="absolute inset-y-0 right-0 bg-bg/75" style={{ left: pct(edit.end, duration) }} />
      <div
        className="absolute inset-y-0 border-y-2 border-accent"
        style={{ left: pct(edit.start, duration), right: `calc(100% - ${pct(edit.end, duration)})` }}
      />
      {handle("start")}
      {handle("end")}
    </div>
  );
}

/** One lane per audio source: name, mute, volume, waveform with muted sections. */
export function Tracks({
  probe,
  edit,
  setEdit,
  video,
}: {
  probe: Probe;
  edit: EditState;
  setEdit: (e: EditState) => void;
  video: () => HTMLVideoElement | null;
}) {
  const heads = useRef<(HTMLDivElement | null)[]>([]);

  // Playhead across all lanes, updated without re-rendering React.
  useEffect(() => {
    let raf = 0;
    const loop = () => {
      const v = video();
      if (v) for (const h of heads.current) if (h) h.style.left = pct(v.currentTime, probe.duration);
      raf = requestAnimationFrame(loop);
    };
    raf = requestAnimationFrame(loop);
    return () => cancelAnimationFrame(raf);
  }, [video, probe.duration]);

  const update = (i: number, patch: Partial<SourceState>) =>
    setEdit({ ...edit, sources: edit.sources.map((s, j) => (j === i ? { ...s, ...patch } : s)) });

  if (probe.sources.length === 0) {
    return <p className="border-t border-line bg-panel px-5 py-3 text-[12.5px] text-faint">This clip has no audio.</p>;
  }

  return (
    <div className="shrink-0 border-t border-line bg-panel px-5 pt-2 pb-3">
      {probe.sources.map((src, i) => {
        const s = edit.sources[i];
        const Icon = /mic|input|line|cable/i.test(src.name) ? Microphone : SpeakerHigh;
        return (
          <div key={i} className="mt-1.5">
            <div className="mb-1 flex items-center gap-3">
              <Icon size={15} className="text-muted" />
              <span className="w-28 truncate text-[12.5px] font-medium">{src.name}</span>
              <button
                onClick={() => update(i, { muted: !s.muted })}
                aria-pressed={s.muted}
                title={s.muted ? "Unmute track" : "Mute whole track"}
                className={cx(
                  "flex h-6 items-center gap-1.5 rounded-ctl px-2 text-[11.5px] transition-colors",
                  s.muted ? "bg-rec/15 text-rec" : "text-muted hover:bg-hover hover:text-fg",
                )}
              >
                {s.muted ? <SpeakerSlash size={13} /> : <SpeakerHigh size={13} />}
                {s.muted ? "Muted" : "Mute"}
              </button>
              <input
                type="range"
                min={0}
                max={2}
                step={0.05}
                value={s.gain}
                disabled={s.muted}
                onChange={(e) => update(i, { gain: Number(e.target.value) })}
                aria-label={`${src.name} volume`}
                className="w-28 disabled:opacity-40"
              />
              <span className="w-10 font-mono text-[11.5px] text-muted">{Math.round(s.gain * 100)}%</span>
              {s.mutes.length > 0 && (
                <span className="text-[11.5px] text-faint">
                  {s.mutes.length} muted {s.mutes.length === 1 ? "section" : "sections"}
                </span>
              )}
            </div>
            <Lane
              peaks={src.peaks}
              pps={probe.peaks_per_second}
              duration={probe.duration}
              edit={edit}
              source={s}
              onMutes={(mutes) => update(i, { mutes: normalize(mutes) })}
              onSeek={(t) => {
                const v = video();
                if (v) v.currentTime = t;
              }}
              headRef={(el) => (heads.current[i] = el)}
            />
          </div>
        );
      })}
      <p className="mt-2 text-[11.5px] text-faint">
        Drag across a waveform to mute that part. <KeyCap>I</KeyCap> / <KeyCap>O</KeyCap> set the start and end at the
        playhead. The preview plays the original sound; the export applies your changes.
      </p>
    </div>
  );
}

function Lane({
  peaks,
  pps,
  duration,
  edit,
  source,
  onMutes,
  onSeek,
  headRef,
}: {
  peaks: number[];
  pps: number;
  duration: number;
  edit: EditState;
  source: SourceState;
  onMutes: (m: [number, number][]) => void;
  onSeek: (t: number) => void;
  headRef: (el: HTMLDivElement | null) => void;
}) {
  const wrap = useRef<HTMLDivElement>(null);
  const canvas = useRef<HTMLCanvasElement>(null);
  const [draft, setDraft] = useState<[number, number] | null>(null);
  const [width, setWidth] = useState(0);

  useEffect(() => {
    const el = wrap.current;
    if (!el) return;
    const ro = new ResizeObserver(() => setWidth(el.clientWidth));
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  // Draw the waveform: bright inside the trim, faint outside or when muted.
  useEffect(() => {
    const c = canvas.current;
    if (!c || width === 0) return;
    const dpr = window.devicePixelRatio || 1;
    const h = 40;
    c.width = Math.round(width * dpr);
    c.height = Math.round(h * dpr);
    const ctx = c.getContext("2d")!;
    ctx.scale(dpr, dpr);
    ctx.clearRect(0, 0, width, h);
    const styles = getComputedStyle(document.documentElement);
    const accent = styles.getPropertyValue("--color-accent").trim() || "#c8f54a";
    const faint = styles.getPropertyValue("--color-line-strong").trim() || "#33353c";
    const bar = 2;
    for (let x = 0; x < width; x += bar + 1) {
      const t0 = (x / width) * duration;
      const t1 = ((x + bar) / width) * duration;
      let peak = 0;
      for (let k = Math.floor(t0 * pps); k <= Math.min(peaks.length - 1, Math.ceil(t1 * pps)); k++) {
        peak = Math.max(peak, peaks[k] ?? 0);
      }
      const amp = Math.max(1, (peak / 255) * (h - 4));
      const inside = t0 >= edit.start - 1e-6 && t1 <= edit.end + 1e-6;
      ctx.fillStyle = inside && !source.muted ? accent : faint;
      ctx.globalAlpha = inside && !source.muted ? 0.85 : 0.6;
      ctx.fillRect(x, (h - amp) / 2, bar, amp);
    }
  }, [peaks, pps, duration, width, edit.start, edit.end, source.muted]);

  const timeAt = (clientX: number) => {
    const r = wrap.current!.getBoundingClientRect();
    return Math.max(0, Math.min(1, (clientX - r.left) / r.width)) * duration;
  };

  return (
    <div
      ref={wrap}
      className="relative h-10 cursor-crosshair touch-none overflow-hidden rounded-ctl bg-bg"
      onPointerDown={(e) => {
        if ((e.target as HTMLElement).closest("button")) return;
        e.currentTarget.setPointerCapture(e.pointerId);
        const t = timeAt(e.clientX);
        setDraft([t, t]);
      }}
      onPointerMove={(e) => {
        if (draft) setDraft([draft[0], timeAt(e.clientX)]);
      }}
      onPointerUp={() => {
        if (!draft) return;
        const [a, b] = draft;
        setDraft(null);
        if (Math.abs(b - a) < 0.08) onSeek(a);
        else onMutes([...source.mutes, [a, b]]);
      }}
    >
      <canvas ref={canvas} className="absolute inset-0 size-full" />
      {source.mutes.map(([a, b], i) => (
        <div
          key={i}
          className="group/m absolute inset-y-0 border-x border-rec/70 bg-[repeating-linear-gradient(135deg,rgb(255_77_79/0.28)_0_4px,rgb(255_77_79/0.1)_4px_8px)]"
          style={{ left: pct(a, duration), width: `calc(${pct(b, duration)} - ${pct(a, duration)})` }}
          title={`Muted ${preciseTime(a)} to ${preciseTime(b)}`}
        >
          <button
            aria-label="Remove muted section"
            onClick={() => onMutes(source.mutes.filter((_, j) => j !== i))}
            className="absolute top-0.5 right-0.5 flex size-4 items-center justify-center rounded-[3px] bg-rec text-white opacity-0 transition-opacity group-hover/m:opacity-100 focus-visible:opacity-100"
          >
            <X size={10} weight="bold" />
          </button>
        </div>
      ))}
      {draft && (
        <div
          className="pointer-events-none absolute inset-y-0 bg-rec/25"
          style={{
            left: pct(Math.min(...draft), duration),
            width: `calc(${pct(Math.max(...draft), duration)} - ${pct(Math.min(...draft), duration)})`,
          }}
        />
      )}
      <div ref={headRef} className="pointer-events-none absolute inset-y-0 w-px bg-fg/80" style={{ left: 0 }} />
    </div>
  );
}

/** Right-hand panel while editing: what will be exported, and the two exports. */
export function EditPanel({
  probe,
  edit,
  bytes,
  busy,
  onSave,
  onDiscord,
  onReset,
  onCancel,
}: {
  probe: Probe;
  edit: EditState;
  bytes: number;
  busy: "library" | "discord" | null;
  onSave: () => void;
  onDiscord: () => void;
  onReset: () => void;
  onCancel: () => void;
}) {
  const [progress, setProgress] = useState(0);
  useEffect(() => {
    if (!busy) return;
    setProgress(0);
    const off = listen<number>("export-progress", (e) => setProgress(e.payload));
    return () => void off.then((f) => f());
  }, [busy]);

  const length = edit.end - edit.start;
  // Video dominates the size and is copied, so size scales with length. Discord copies
  // also drop the per-source tracks (192 kbps each).
  const extra = Math.max(0, probe.sources.length > 1 ? probe.sources.length : 0) * 24_000 * length;
  const est = Math.max(0, (bytes * length) / probe.duration - (probe.sources.length > 1 ? extra : 0));
  const tooBig = est > DISCORD_FREE_BYTES;
  const mutedSections = edit.sources.reduce((n, s) => n + s.mutes.length, 0);
  const mutedTracks = edit.sources.filter((s) => s.muted).length;

  return (
    <div className="flex flex-col">
      <div className="border-b border-line p-5">
        <div className="flex items-center justify-between">
          <h2 className="text-[17px] font-semibold tracking-tight">Edit clip</h2>
          <button onClick={onCancel} className="text-[12.5px] text-muted hover:text-fg" disabled={!!busy}>
            Cancel
          </button>
        </div>
        <dl className="mt-4 grid grid-cols-2 gap-x-4 gap-y-3 text-[12.5px]">
          <div>
            <dt className="text-faint">Start</dt>
            <dd className="mt-0.5 font-mono text-fg">{preciseTime(edit.start)}</dd>
          </div>
          <div>
            <dt className="text-faint">End</dt>
            <dd className="mt-0.5 font-mono text-fg">{preciseTime(edit.end)}</dd>
          </div>
          <div>
            <dt className="text-faint">Length</dt>
            <dd className="mt-0.5 font-mono text-fg">{length.toFixed(1)} s</dd>
          </div>
          <div>
            <dt className="text-faint">Audio</dt>
            <dd className="mt-0.5 text-fg">
              {mutedTracks + mutedSections === 0
                ? "Unchanged"
                : [mutedTracks && `${mutedTracks} muted`, mutedSections && `${mutedSections} cut out`]
                    .filter(Boolean)
                    .join(", ")}
            </dd>
          </div>
        </dl>
        <p className="mt-3 text-[12px] leading-relaxed text-faint">
          The start snaps to the nearest keyframe so the video is cut without losing quality.
        </p>
      </div>

      <div className="flex flex-col gap-2 border-b border-line p-5">
        <Button variant="primary" size="lg" onClick={onSave} disabled={!!busy}>
          <FloppyDisk size={17} weight="bold" />
          {busy === "library" ? `Saving ${Math.round(progress * 100)}%` : "Save as new clip"}
        </Button>
        <Button size="lg" onClick={onDiscord} disabled={!!busy}>
          <DiscordLogo size={17} weight="fill" className="text-[#8c9eff]" />
          {busy === "discord" ? `Preparing ${Math.round(progress * 100)}%` : "Copy edit for Discord"}
        </Button>
        {busy && (
          <div className="h-1 overflow-hidden rounded-full bg-line">
            <div className="h-full bg-accent transition-[width] duration-150" style={{ width: `${progress * 100}%` }} />
          </div>
        )}
        <p className={cx("text-[12.5px] leading-relaxed", tooBig ? "text-warn" : "text-faint")}>
          About {formatBytes(est)} for Discord.
          {tooBig && " That's over the 10 MB free limit: trim it shorter, or send it with Nitro."}
        </p>
        <p className="text-[12px] leading-relaxed text-faint">The original clip stays as it is.</p>
      </div>

      <div className="p-3">
        <Button variant="ghost" className="w-full justify-start" onClick={onReset} disabled={!!busy}>
          <ArrowCounterClockwise size={17} />
          Reset changes
        </Button>
      </div>
    </div>
  );
}
