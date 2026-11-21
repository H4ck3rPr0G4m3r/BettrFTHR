import { forwardRef, useCallback, useEffect, useImperativeHandle, useRef, useState, type ReactNode } from "react";
import {
  ArrowsIn,
  ArrowsOut,
  Pause,
  Play,
  SpeakerHigh,
  SpeakerLow,
  SpeakerSlash,
} from "@phosphor-icons/react";
import { cx } from "./ui";

export interface PlayerHandle {
  toggle: () => void;
  seekBy: (s: number) => void;
  seekTo: (s: number) => void;
  step: (frames: number) => void;
  toggleMute: () => void;
  toggleFullscreen: () => void;
  video: () => HTMLVideoElement | null;
}

const FRAME = 1 / 60;

function readPref(key: string, fallback: number) {
  try {
    const v = Number(localStorage.getItem(key));
    return Number.isFinite(v) && localStorage.getItem(key) !== null ? v : fallback;
  } catch {
    return fallback;
  }
}
function writePref(key: string, v: number) {
  try {
    localStorage.setItem(key, String(v));
  } catch {
    /* storage unavailable: preference just isn't remembered */
  }
}

/** m:ss.t (tenths) for the precise readout. */
export function preciseTime(t: number) {
  if (!Number.isFinite(t) || t < 0) t = 0;
  const m = Math.floor(t / 60);
  const s = t - m * 60;
  return `${m}:${s.toFixed(1).padStart(4, "0")}`;
}

/**
 * Video player with a large, precise timeline (ruler, hover time, drag scrubbing).
 * The timeline takes `overlay` children so the editor can draw trim ranges on it.
 */
export const Player = forwardRef<
  PlayerHandle,
  { src: string; autoPlay?: boolean; overlay?: (duration: number) => ReactNode; onDuration?: (d: number) => void }
>(function Player({ src, autoPlay = true, overlay, onDuration }, ref) {
  const video = useRef<HTMLVideoElement>(null);
  const shell = useRef<HTMLDivElement>(null);
  const timeText = useRef<HTMLSpanElement>(null);
  const [playing, setPlaying] = useState(false);
  const [duration, setDuration] = useState(0);
  const [volume, setVolume] = useState(() => readPref("player.volume", 1));
  const [muted, setMuted] = useState(false);
  const [rate, setRate] = useState(1);
  const [full, setFull] = useState(false);
  const setPlayhead = useRef<(t: number) => void>(() => {});

  const sync = useCallback(() => {
    const v = video.current;
    if (!v) return;
    setPlayhead.current(v.currentTime);
    if (timeText.current) timeText.current.textContent = preciseTime(v.currentTime);
  }, []);

  // Smooth playhead while playing; DOM updates only, no React re-render per frame.
  useEffect(() => {
    if (!playing) return;
    let raf = 0;
    const loop = () => {
      sync();
      raf = requestAnimationFrame(loop);
    };
    raf = requestAnimationFrame(loop);
    return () => cancelAnimationFrame(raf);
  }, [playing, sync]);

  useEffect(() => {
    const v = video.current;
    if (v) {
      v.volume = Math.min(1, volume);
      v.muted = muted;
      v.playbackRate = rate;
    }
  }, [volume, muted, rate, src]);

  useEffect(() => {
    const onFs = () => setFull(document.fullscreenElement === shell.current);
    document.addEventListener("fullscreenchange", onFs);
    return () => document.removeEventListener("fullscreenchange", onFs);
  }, []);

  const seekTo = useCallback(
    (t: number) => {
      const v = video.current;
      if (!v || !Number.isFinite(v.duration)) return;
      v.currentTime = Math.max(0, Math.min(v.duration, t));
      sync();
    },
    [sync],
  );

  const api: PlayerHandle = {
    toggle: () => {
      const v = video.current;
      if (!v) return;
      if (v.paused) {
        if (v.ended) v.currentTime = 0;
        void v.play();
      } else v.pause();
    },
    seekBy: (s) => seekTo((video.current?.currentTime ?? 0) + s),
    seekTo,
    step: (frames) => {
      video.current?.pause();
      seekTo((video.current?.currentTime ?? 0) + frames * FRAME);
    },
    toggleMute: () => setMuted((m) => !m),
    toggleFullscreen: () => {
      if (document.fullscreenElement) void document.exitFullscreen();
      else void shell.current?.requestFullscreen();
    },
    video: () => video.current,
  };
  useImperativeHandle(ref, () => api);

  const VolIcon = muted || volume === 0 ? SpeakerSlash : volume < 0.5 ? SpeakerLow : SpeakerHigh;

  return (
    <div ref={shell} className="flex h-full min-h-0 flex-col bg-bg">
      <div className="relative flex min-h-0 flex-1 items-center justify-center bg-black/50">
        <video
          ref={video}
          src={src}
          autoPlay={autoPlay}
          playsInline
          onClick={api.toggle}
          onDoubleClick={api.toggleFullscreen}
          onPlay={() => setPlaying(true)}
          onPause={() => {
            setPlaying(false);
            sync();
          }}
          onSeeked={sync}
          onLoadedMetadata={(e) => {
            const d = e.currentTarget.duration;
            setDuration(Number.isFinite(d) ? d : 0);
            if (Number.isFinite(d)) onDuration?.(d);
            sync();
          }}
          className="max-h-full max-w-full cursor-pointer"
        />
      </div>

      <div className="shrink-0 border-t border-line bg-panel px-5 pt-2 pb-3">
        <Timeline
          duration={duration}
          onSeek={seekTo}
          bind={(fn) => (setPlayhead.current = fn)}
          onScrub={(active) => {
            const v = video.current;
            if (!v) return;
            if (active) {
              v.dataset.resume = v.paused ? "" : "1";
              v.pause();
            } else if (v.dataset.resume) void v.play();
          }}
        >
          {overlay?.(duration)}
        </Timeline>

        <div className="mt-1 flex items-center gap-3">
          <button
            onClick={api.toggle}
            aria-label={playing ? "Pause (Space)" : "Play (Space)"}
            title={playing ? "Pause (Space)" : "Play (Space)"}
            className="flex size-9 items-center justify-center rounded-ctl bg-raised text-fg hover:bg-hover"
          >
            {playing ? <Pause size={18} weight="fill" /> : <Play size={18} weight="fill" />}
          </button>
          <div className="font-mono text-[13px] whitespace-nowrap tabular-nums">
            <span ref={timeText} className="text-fg">
              0:00.0
            </span>
            <span className="text-faint"> / {preciseTime(duration)}</span>
          </div>
          <span className="hidden text-[12px] whitespace-nowrap text-faint 2xl:inline">
            Space play, arrows 1 s, comma / period 1 frame
          </span>
          <div className="flex-1" />
          <div className="flex items-center rounded-ctl border border-line bg-bg p-0.5" role="radiogroup" aria-label="Speed">
            {[0.25, 0.5, 1].map((r) => (
              <button
                key={r}
                role="radio"
                aria-checked={rate === r}
                onClick={() => setRate(r)}
                className={cx(
                  "h-7 rounded-[4px] px-2 font-mono text-[12px]",
                  rate === r ? "bg-hover text-fg" : "text-muted hover:text-fg",
                )}
              >
                {r}x
              </button>
            ))}
          </div>
          <div className="flex items-center gap-1.5">
            <button
              onClick={api.toggleMute}
              aria-label={muted ? "Unmute (M)" : "Mute (M)"}
              title={muted ? "Unmute (M)" : "Mute (M)"}
              className="flex size-8 items-center justify-center rounded-ctl text-muted hover:bg-hover hover:text-fg"
            >
              <VolIcon size={18} />
            </button>
            <input
              type="range"
              min={0}
              max={1}
              step={0.02}
              value={muted ? 0 : volume}
              onChange={(e) => {
                const v = Number(e.target.value);
                setVolume(v);
                setMuted(v === 0);
                writePref("player.volume", v);
              }}
              aria-label="Volume"
              className="w-24"
            />
          </div>
          <button
            onClick={api.toggleFullscreen}
            aria-label="Fullscreen (F)"
            title="Fullscreen (F)"
            className="flex size-8 items-center justify-center rounded-ctl text-muted hover:bg-hover hover:text-fg"
          >
            {full ? <ArrowsIn size={18} /> : <ArrowsOut size={18} />}
          </button>
        </div>
      </div>
    </div>
  );
});

/** Picks a ruler label interval giving at most ~12 labels. */
function rulerStep(duration: number) {
  for (const s of [1, 2, 5, 10, 15, 30, 60, 120, 300, 600, 1800]) if (duration / s <= 12) return s;
  return 3600;
}

function Timeline({
  duration,
  onSeek,
  onScrub,
  bind,
  children,
}: {
  duration: number;
  onSeek: (t: number) => void;
  onScrub: (active: boolean) => void;
  bind: (fn: (t: number) => void) => void;
  children?: ReactNode;
}) {
  const track = useRef<HTMLDivElement>(null);
  const fill = useRef<HTMLDivElement>(null);
  const head = useRef<HTMLDivElement>(null);
  const hoverLine = useRef<HTMLDivElement>(null);
  const hoverLabel = useRef<HTMLDivElement>(null);
  const dragging = useRef(false);

  useEffect(() => {
    bind((t) => {
      const p = duration > 0 ? Math.min(1, t / duration) : 0;
      if (fill.current) fill.current.style.width = `${p * 100}%`;
      if (head.current) head.current.style.left = `${p * 100}%`;
    });
  }, [bind, duration]);

  const timeAt = (clientX: number) => {
    const r = track.current!.getBoundingClientRect();
    return Math.max(0, Math.min(1, (clientX - r.left) / r.width)) * duration;
  };

  const showHover = (clientX: number) => {
    if (!track.current || duration <= 0) return;
    const r = track.current.getBoundingClientRect();
    const p = Math.max(0, Math.min(1, (clientX - r.left) / r.width));
    if (hoverLine.current) {
      hoverLine.current.style.left = `${p * 100}%`;
      hoverLine.current.style.opacity = "1";
    }
    if (hoverLabel.current) {
      hoverLabel.current.style.left = `${p * 100}%`;
      hoverLabel.current.style.opacity = "1";
      hoverLabel.current.textContent = preciseTime(p * duration);
    }
  };
  const hideHover = () => {
    if (hoverLine.current) hoverLine.current.style.opacity = "0";
    if (hoverLabel.current) hoverLabel.current.style.opacity = "0";
  };

  const step = rulerStep(duration || 1);
  const minor = step >= 5 ? step / 5 : step / 2;
  const ticks: { t: number; major: boolean }[] = [];
  if (duration > 0) {
    for (let t = 0; t <= duration + 1e-6; t += minor) {
      ticks.push({ t, major: Math.abs(t / step - Math.round(t / step)) < 1e-6 });
    }
  }

  return (
    <div className="relative select-none">
      {/* Ruler */}
      <div className="relative h-5">
        {ticks.map(({ t, major }) => (
          <div key={t} className="absolute bottom-0" style={{ left: `${(t / duration) * 100}%` }}>
            <div className={cx("w-px", major ? "h-2 bg-line-strong" : "h-1 bg-line")} />
            {major && (
              <span className="absolute bottom-2.5 -translate-x-1/2 font-mono text-[10.5px] text-faint">
                {t >= 60 ? `${Math.floor(t / 60)}:${String(Math.round(t % 60)).padStart(2, "0")}` : `${t}s`}
              </span>
            )}
          </div>
        ))}
      </div>

      {/* Track: tall hit area, visible bar grows on hover */}
      <div
        ref={track}
        role="slider"
        aria-label="Seek"
        aria-valuemin={0}
        aria-valuemax={Math.round(duration)}
        tabIndex={-1}
        className="group relative h-10 cursor-pointer touch-none"
        onPointerDown={(e) => {
          if (duration <= 0) return;
          e.currentTarget.setPointerCapture(e.pointerId);
          dragging.current = true;
          onScrub(true);
          onSeek(timeAt(e.clientX));
        }}
        onPointerMove={(e) => {
          showHover(e.clientX);
          if (dragging.current) onSeek(timeAt(e.clientX));
        }}
        onPointerUp={(e) => {
          if (!dragging.current) return;
          dragging.current = false;
          e.currentTarget.releasePointerCapture(e.pointerId);
          onScrub(false);
        }}
        onPointerLeave={hideHover}
      >
        <div className="absolute inset-x-0 top-1/2 h-2.5 -translate-y-1/2 overflow-hidden rounded-full bg-line-strong transition-[height] duration-150 group-hover:h-3.5">
          <div ref={fill} className="h-full bg-accent" style={{ width: 0 }} />
        </div>
        {children && <div className="pointer-events-none absolute inset-0">{children}</div>}
        <div
          ref={hoverLine}
          className="pointer-events-none absolute top-1 bottom-1 w-px -translate-x-1/2 bg-fg/50 opacity-0 transition-opacity"
        />
        <div
          ref={hoverLabel}
          className="pointer-events-none absolute -top-6 -translate-x-1/2 rounded-[4px] bg-fg px-1.5 py-0.5 font-mono text-[11px] font-medium text-bg opacity-0 transition-opacity"
        />
        <div ref={head} className="pointer-events-none absolute top-0 bottom-0 -translate-x-1/2" style={{ left: 0 }}>
          <div className="absolute top-1/2 left-1/2 size-4 -translate-x-1/2 -translate-y-1/2 rounded-full border-2 border-bg bg-fg shadow" />
        </div>
      </div>
    </div>
  );
}
