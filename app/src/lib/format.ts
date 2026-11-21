import type { Clip } from "./api";

export function formatBytes(n: number): string {
  if (n < 1024 * 1024) return `${Math.max(1, Math.round(n / 1024))} KB`;
  if (n < 1024 * 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} MB`;
  return `${(n / 1024 / 1024 / 1024).toFixed(2)} GB`;
}

export function formatDuration(s: number | null | undefined): string {
  if (s == null || !isFinite(s) || s <= 0) return "";
  const total = Math.round(s);
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const sec = total % 60;
  const mm = h ? String(m).padStart(2, "0") : String(m);
  return `${h ? `${h}:` : ""}${mm}:${String(sec).padStart(2, "0")}`;
}

export function formatTime(ms: number): string {
  return new Date(ms).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

export function formatDateTime(ms: number): string {
  return new Date(ms).toLocaleString([], {
    weekday: "short",
    day: "numeric",
    month: "short",
    hour: "2-digit",
    minute: "2-digit",
  });
}

function startOfDay(ms: number) {
  const d = new Date(ms);
  d.setHours(0, 0, 0, 0);
  return d.getTime();
}

/** "Today", "Yesterday", a weekday within the last week, else a date. */
export function dayLabel(ms: number, now = Date.now()): string {
  const days = Math.round((startOfDay(now) - startOfDay(ms)) / 86_400_000);
  if (days <= 0) return "Today";
  if (days === 1) return "Yesterday";
  const d = new Date(ms);
  if (days < 7) return d.toLocaleDateString([], { weekday: "long" });
  return d.toLocaleDateString([], { day: "numeric", month: "long", year: "numeric" });
}

export function groupByDay(clips: Clip[]): { label: string; clips: Clip[] }[] {
  const groups: { label: string; clips: Clip[] }[] = [];
  for (const c of clips) {
    const label = dayLabel(c.modified);
    const last = groups[groups.length - 1];
    if (last && last.label === label) last.clips.push(c);
    else groups.push({ label, clips: [c] });
  }
  return groups;
}

const DEFAULT_NAME = /^(.*?)\s*(Recording\s+)?\d{4}-\d{2}-\d{2} \d{2}-\d{2}-\d{2}( \(\d+\))?$/;

/** Engine file names are timestamps; show those as "Clip at 10:12" and keep custom names. */
export function clipTitle(c: Clip): string {
  if (DEFAULT_NAME.test(c.title)) {
    return `${c.kind === "recording" ? "Recording" : "Clip"} at ${formatTime(c.modified)}`;
  }
  return c.title;
}

/** Discord's upload limit without Nitro. */
export const DISCORD_FREE_BYTES = 10 * 1024 * 1024;

/** Stable, muted color for a game's monogram tile. */
export function gameHue(name: string): number {
  let h = 0;
  for (let i = 0; i < name.length; i++) h = (h * 31 + name.charCodeAt(i)) >>> 0;
  return h % 360;
}

export function monogram(name: string): string {
  const words = name.replace(/[^\p{L}\p{N} ]/gu, " ").split(/\s+/).filter(Boolean);
  if (words.length >= 2) return (words[0][0] + words[1][0]).toUpperCase();
  return (words[0] ?? "?").slice(0, 2).toUpperCase();
}
