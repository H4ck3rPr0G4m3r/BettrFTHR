import { useEffect, useState, type ReactNode } from "react";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { Microphone, Play, Plus, SpeakerHigh, Trash, Warning } from "@phosphor-icons/react";
import { api, errorText, type AudioDevice, type AudioSource, type Settings as S, type SourceKind, type StorageStatus } from "../lib/api";
import { formatBytes, formatDateTime } from "../lib/format";
import { useApp } from "../lib/store";
import { captureHotkey } from "../lib/hotkey";
import { PANEL, SIDEBAR, useLayout } from "../lib/layout";
import { TopBar } from "./TopBar";
import { Button, Combo, IconButton, Row, Segmented, Toggle, cx } from "./ui";

const LENGTHS = [15, 30, 60, 120, 300];
const FPS = [30, 60, 120, 144, 240];
const HEIGHTS = [0, 1440, 1080, 720];
const QUALITY = [
  { kbps: 8_000, label: "Light" },
  { kbps: 20_000, label: "Balanced" },
  { kbps: 35_000, label: "High" },
  { kbps: 60_000, label: "Max" },
];

/** Settings that only take effect when the engine restarts (which clears the replay buffer). */
const ENGINE_KEYS: (keyof S)[] = ["out_dir", "replay_seconds", "fps", "bitrate_kbps", "height", "cursor", "hotkeys", "audio"];

export function Settings() {
  const saved = useApp((s) => s.settings);
  const setSaved = useApp((s) => s.setSettings);
  const toast = useApp((s) => s.toast);
  const engine = useApp((s) => s.engine);
  const [draft, setDraft] = useState<S | null>(saved);
  const [busy, setBusy] = useState(false);
  const [version, setVersion] = useState("");
  useEffect(() => {
    api.appVersion().then(setVersion, () => {});
  }, []);

  useEffect(() => {
    if (!draft && saved) setDraft(saved);
  }, [saved, draft]);

  if (!draft || !saved) return null;

  const dirty = JSON.stringify(draft) !== JSON.stringify(saved);
  const restarts = ENGINE_KEYS.some((k) => JSON.stringify(draft[k]) !== JSON.stringify(saved[k]));
  const set = <K extends keyof S>(k: K, v: S[K]) => setDraft({ ...draft, [k]: v });

  const apply = async () => {
    setBusy(true);
    try {
      const next = await api.setSettings(draft);
      setSaved(next);
      setDraft(next);
      void useApp.getState().refreshClips();
      toast({ tone: "info", title: restarts ? "Saved. Capture is restarting." : "Saved" });
    } catch (e) {
      toast({ tone: "error", title: "Could not save settings", body: errorText(e) });
    } finally {
      setBusy(false);
    }
  };

  const memMb = Math.round(((draft.bitrate_kbps * 1000) / 8) * draft.replay_seconds / 1_000_000);
  const info = engine?.info;

  return (
    <div className="relative flex min-w-0 flex-1 flex-col">
      <TopBar title={<h1 className="text-[17px] font-semibold tracking-tight">Settings</h1>} search={false} />
      <div className="min-h-0 flex-1 overflow-y-auto">
        <div className="mx-auto max-w-[760px] px-8 pt-6 pb-32">
          <Section title="Replay clips">
            <Row title="Clip length" help={`How far back a saved clip reaches. Keeps about ${memMb} MB in memory.`}>
              <Segmented
                label="Clip length"
                value={draft.replay_seconds}
                options={withCustom(LENGTHS, draft.replay_seconds).map((s) => ({
                  value: s,
                  label: s < 60 ? `${s}s` : `${s / 60}m`,
                }))}
                onChange={(v) => set("replay_seconds", v)}
              />
            </Row>
            <Row title="Frame rate" help="Higher than your monitor's refresh rate only repeats frames.">
              <Segmented
                label="Frame rate"
                value={draft.fps}
                options={withCustom(FPS, draft.fps).map((f) => ({ value: f, label: String(f) }))}
                onChange={(v) => set("fps", v)}
              />
            </Row>
            <Row title="Resolution" help="Native keeps your monitor's resolution.">
              <Segmented
                label="Resolution"
                value={draft.height}
                options={withCustom(HEIGHTS, draft.height).map((h) => ({ value: h, label: h === 0 ? "Native" : `${h}p` }))}
                onChange={(v) => set("height", v)}
              />
            </Row>
            <Row title="Quality" help={`${draft.bitrate_kbps / 1000} Mbps. Light keeps short clips small enough for Discord.`}>
              <Segmented
                label="Quality"
                value={draft.bitrate_kbps}
                options={withCustom(
                  QUALITY.map((q) => q.kbps),
                  draft.bitrate_kbps,
                ).map((k) => ({ value: k, label: QUALITY.find((q) => q.kbps === k)?.label ?? `${k / 1000}M` }))}
                onChange={(v) => set("bitrate_kbps", v)}
              />
            </Row>
            <Row title="Show mouse cursor">
              <Toggle label="Show mouse cursor" checked={draft.cursor} onChange={(v) => set("cursor", v)} />
            </Row>
            {info && (
              <p className="pt-1 font-mono text-[12px] text-faint">
                {info.encoder} on {info.adapter}
              </p>
            )}
          </Section>

          <Section title="Hotkeys">
            <Row title="Save clip">
              <HotkeyField value={draft.hotkeys.save} onChange={(v) => set("hotkeys", { ...draft.hotkeys, save: v })} />
            </Row>
            <Row title="Start / stop recording">
              <HotkeyField
                value={draft.hotkeys.record}
                onChange={(v) => set("hotkeys", { ...draft.hotkeys, record: v })}
              />
            </Row>
            {draft.hotkeys.save === draft.hotkeys.record && (
              <p className="flex items-center gap-2 pb-2 text-[12.5px] text-warn">
                <Warning size={15} /> Both actions use the same key.
              </p>
            )}
          </Section>

          <AudioSection sources={draft.audio} onChange={(a) => set("audio", a)} />

          <Section title="Library">
            <Row title="Clip folder" help={<span className="font-mono break-all">{draft.out_dir}</span>}>
              <div className="flex gap-2">
                <Button
                  size="sm"
                  variant="ghost"
                  onClick={() =>
                    api.openClipFolder().catch((e) => toast({ tone: "error", title: "Could not open the folder", body: errorText(e) }))
                  }
                >
                  Open
                </Button>
                <Button
                  size="sm"
                  onClick={async () => {
                    const dir = await openDialog({ directory: true, defaultPath: draft.out_dir });
                    if (typeof dir === "string") set("out_dir", dir);
                  }}
                >
                  Change
                </Button>
              </div>
            </Row>
          </Section>

          <StorageSection draft={draft} set={set} />

          <Section title="Discord">
            <Row
              title="Upload limit"
              help="Copies for Discord are compressed to fit. Free accounts: 10 MB, Nitro Basic: 50 MB, Nitro: 500 MB."
            >
              <Segmented
                label="Discord upload limit"
                value={draft.discord_limit_mb}
                options={withCustom([10, 50, 500], draft.discord_limit_mb).map((v) => ({ value: v, label: `${v} MB` }))}
                onChange={(v) => set("discord_limit_mb", v)}
              />
            </Row>
          </Section>

          <SoundSection draft={draft} set={set} />

          <AppearanceSection />

          <Section title="Behavior">
            <Row title="Detect games" help="Files clips under the game you're playing and names them after it.">
              <Toggle label="Detect games" checked={draft.game_detection} onChange={(v) => set("game_detection", v)} />
            </Row>
            <Row
              title="Replay buffer"
              help={
                draft.buffer_mode === "games"
                  ? "Captures only while a detected game runs. Saves power on the desktop."
                  : "Always capturing, so desktop moments can be clipped too."
              }
            >
              <Segmented
                label="Replay buffer"
                value={draft.buffer_mode}
                options={[
                  { value: "always", label: "Always" },
                  { value: "games", label: "In games" },
                ]}
                onChange={(v) => set("buffer_mode", v)}
              />
            </Row>
            <Row title="Start with Windows" help="Starts hidden in the tray, so clipping works right after you log in.">
              <Toggle
                label="Start with Windows"
                checked={draft.start_with_windows}
                onChange={(v) => set("start_with_windows", v)}
              />
            </Row>
            <Row title="Keep running in the tray" help="Closing the window keeps clipping in the background.">
              <Toggle
                label="Keep running in the tray"
                checked={draft.close_to_tray}
                onChange={(v) => set("close_to_tray", v)}
              />
            </Row>
          </Section>

          <Section title="About">
            <Row
              title={`BettrFTHR ${version}`}
              help="Open source (GPL-3.0). If something breaks, the engine log helps find out why."
            >
              <Button
                size="sm"
                onClick={() =>
                  api.openLogs().catch((e) => toast({ tone: "error", title: "Could not open logs", body: errorText(e) }))
                }
              >
                Open logs folder
              </Button>
            </Row>
          </Section>
        </div>
      </div>

      <div
        className={cx(
          "pointer-events-none absolute inset-x-0 bottom-0 flex justify-center pb-5 transition-[opacity,transform] duration-300",
          dirty ? "translate-y-0 opacity-100" : "translate-y-3 opacity-0",
        )}
      >
        <div className="pointer-events-auto flex items-center gap-4 rounded-card border border-line-strong bg-raised py-2 pr-2 pl-4 shadow-[0_16px_40px_-12px_rgb(0_0_0/0.7)]">
          <span className="text-[13px] text-muted">
            {restarts ? "Applying restarts capture and clears the replay buffer." : "Unsaved changes"}
          </span>
          <Button size="sm" variant="ghost" onClick={() => setDraft(saved)} disabled={busy}>
            Discard
          </Button>
          <Button size="sm" variant="primary" onClick={apply} disabled={busy || !dirty}>
            Apply
          </Button>
        </div>
      </div>
    </div>
  );
}

const SOUNDS: { value: S["sound_style"]; label: string }[] = [
  { value: "glass", label: "Glass" },
  { value: "pop", label: "Pop" },
  { value: "shutter", label: "Shutter" },
  { value: "rise", label: "Rise" },
  { value: "tick", label: "Tick" },
];

function SoundSection({ draft, set }: { draft: S; set: <K extends keyof S>(k: K, v: S[K]) => void }) {
  const preview = (style: S["sound_style"], volume = draft.sound_volume) => void api.previewSound(style, volume);
  return (
    <Section title="Sounds">
      <Row title="Play a sound on save" help="When a clip is saved and when a recording starts or stops.">
        <Toggle label="Play a sound on save" checked={draft.save_sound} onChange={(v) => set("save_sound", v)} />
      </Row>
      <div className={cx("py-3.5", !draft.save_sound && "pointer-events-none opacity-40")}>
        <div className="text-[14px]">Clip saved sound</div>
        <div className="mt-0.5 text-[12.5px] text-faint">Click one to hear it.</div>
        <div className="mt-3 grid grid-cols-5 gap-2" role="radiogroup" aria-label="Clip saved sound">
          {SOUNDS.map((s) => {
            const on = draft.sound_style === s.value;
            return (
              <button
                key={s.value}
                role="radio"
                aria-checked={on}
                onClick={() => {
                  set("sound_style", s.value);
                  preview(s.value);
                }}
                className={cx(
                  "flex h-11 items-center justify-center gap-2 rounded-ctl border text-[13px] transition-colors",
                  on ? "border-accent bg-accent/10 text-fg" : "border-line bg-raised text-muted hover:border-line-strong hover:text-fg",
                )}
              >
                <Play size={13} weight="fill" className={on ? "text-accent" : "text-faint"} />
                {s.label}
              </button>
            );
          })}
        </div>
      </div>
      <Row title="Volume">
        <label className={cx("flex items-center gap-3", !draft.save_sound && "pointer-events-none opacity-40")}>
          <input
            type="range"
            min={0}
            max={1}
            step={0.05}
            value={draft.sound_volume}
            onChange={(e) => set("sound_volume", Number(e.target.value))}
            onPointerUp={(e) => preview(draft.sound_style, Number((e.target as HTMLInputElement).value))}
            onKeyUp={(e) => preview(draft.sound_style, Number((e.target as HTMLInputElement).value))}
            aria-label="Sound volume"
            className="w-40"
          />
          <span className="w-10 text-right font-mono text-[12px] text-muted">{Math.round(draft.sound_volume * 100)}%</span>
        </label>
      </Row>
    </Section>
  );
}

function AppearanceSection() {
  const { sidebar, panel, setSidebar, setPanel, reset } = useLayout();
  const slider = (value: number, range: { min: number; max: number }, onChange: (v: number) => void, label: string) => (
    <label className="flex items-center gap-3">
      <input
        type="range"
        min={range.min}
        max={range.max}
        step={4}
        value={value}
        onChange={(e) => onChange(Number(e.target.value))}
        aria-label={label}
        className="w-40"
      />
      <span className="w-14 text-right font-mono text-[12px] text-muted">{value} px</span>
    </label>
  );
  return (
    <Section title="Layout">
      <Row title="Sidebar width" help="You can also drag the sidebar's edge.">
        {slider(sidebar, SIDEBAR, setSidebar, "Sidebar width")}
      </Row>
      <Row title="Clip panel width" help="The panel next to the player. Drag its edge in the clip view.">
        {slider(panel, PANEL, setPanel, "Clip panel width")}
      </Row>
      <Row title="Reset layout">
        <Button size="sm" variant="ghost" onClick={reset} disabled={sidebar === SIDEBAR.default && panel === PANEL.default}>
          Reset
        </Button>
      </Row>
    </Section>
  );
}

const GiB = 1024 ** 3;

function StorageSection({ draft, set }: { draft: S; set: <K extends keyof S>(k: K, v: S[K]) => void }) {
  const clips = useApp((s) => s.clips);
  const status = useApp((s) => s.storage);
  const saved = useApp((s) => s.settings);
  const toast = useApp((s) => s.toast);
  const st = draft.storage;
  const update = (patch: Partial<S["storage"]>) => set("storage", { ...st, ...patch });

  const used = clips.reduce((n, c) => n + c.bytes, 0);
  const limit = st.limit_gb * GiB;
  const over = st.enabled && used > limit;
  // The bar's full width is the larger of the two, so going over the limit stays visible.
  const scale = Math.max(used, st.enabled ? limit : used, 1);
  const pct = (v: number) => `${(v / scale) * 100}%`;
  const off = !st.enabled;
  const last = status?.last;


  return (
    <Section title="Storage">
      <div className="py-3.5">
        <div className="flex items-baseline justify-between gap-4 font-mono text-[12px]">
          <span className={over ? "text-warn" : "text-muted"}>
            {formatBytes(used)} in {clips.length} {clips.length === 1 ? "clip" : "clips"}
          </span>
          <span className="text-faint">{st.enabled ? `limit ${formatBytes(limit)}` : "no limit"}</span>
        </div>
        <div
          className="relative mt-2 h-2 overflow-hidden rounded-full bg-line"
          role="meter"
          aria-label="Library size"
          aria-valuemin={0}
          aria-valuemax={st.enabled ? limit : used}
          aria-valuenow={used}
        >
          <span
            className={cx("absolute inset-y-0 left-0 transition-[width] duration-500 ease-out-quint", off ? "bg-faint" : "bg-accent")}
            style={{ width: pct(st.enabled ? Math.min(used, limit) : used) }}
          />
          {over && (
            <span
              className="absolute inset-y-0 bg-warn transition-[width,left] duration-500 ease-out-quint"
              style={{ left: pct(limit), width: pct(used - limit) }}
            />
          )}
          {st.enabled && used > 0 && <span className="absolute inset-y-0 w-[2px] bg-bg" style={{ left: pct(limit) }} />}
        </div>
        <StorageActivityLine status={status} enabled={!!saved?.storage.enabled} last={last} onRun={() =>
          api.storageRun().catch((e) => toast({ tone: "error", title: "Could not start the cleanup", body: errorText(e) }))
        } />
      </div>

      <Row title="Keep the library under a size limit" help="When it grows past the limit, the oldest clips make room. Off by default.">
        <Toggle label="Keep the library under a size limit" checked={st.enabled} onChange={(v) => update({ enabled: v })} />
      </Row>
      <div className={cx("divide-y divide-line/70 transition-opacity", off && "pointer-events-none opacity-40")} aria-disabled={off}>
        <Row title="Size limit" help="Everything in the clip folder counts, recordings included.">
          <NumberField
            label="Size limit in GB"
            value={st.limit_gb}
            min={1}
            decimals={1}
            suffix="GB"
            onCommit={(v) => update({ limit_gb: v })}
          />
        </Row>
        <Row
          title="Compress the oldest clips first"
          help="Re-encodes them to about 40% of their size (1080p becomes 720p). Each clip only once, and only while no game is running."
        >
          <Toggle label="Compress the oldest clips first" checked={st.compress} onChange={(v) => update({ compress: v })} />
        </Row>
        <div className={cx("transition-opacity", !st.compress && "pointer-events-none opacity-40")} aria-disabled={!st.compress}>
          <Row
            title="How many to compress"
            help={
              st.delete
                ? `Only the ${st.compress_oldest} oldest are compressed. When that isn't enough, the oldest goes to the Recycle Bin and the next one in line is compressed, until it fits.`
                : `Only the ${st.compress_oldest} oldest are compressed.`
            }
          >
            <NumberField
              label="How many of the oldest clips to compress"
              value={st.compress_oldest}
              min={1}
              suffix={st.compress_oldest === 1 ? "clip" : "clips"}
              onCommit={(v) => update({ compress_oldest: v })}
            />
          </Row>
        </div>
        <Row
          title="Then move the oldest to the Recycle Bin"
          help="When compressing isn't enough. Clips from the last 10 minutes are never touched."
        >
          <Toggle label="Then move the oldest to the Recycle Bin" checked={st.delete} onChange={(v) => update({ delete: v })} />
        </Row>
        {st.enabled && !st.compress && !st.delete && (
          <p className="flex items-center gap-2 py-3 text-[12.5px] text-warn">
            <Warning size={15} /> Both are off, so nothing happens when the limit is reached.
          </p>
        )}
      </div>
    </Section>
  );
}

/** Text field for a number: typing is free, the value is checked when leaving the field. */
function NumberField({
  label,
  value,
  min,
  decimals = 0,
  suffix,
  onCommit,
}: {
  label: string;
  value: number;
  min: number;
  decimals?: number;
  suffix: string;
  onCommit: (v: number) => void;
}) {
  const [text, setText] = useState(String(value));
  useEffect(() => setText(String(value)), [value]);
  const commit = () => {
    const v = Number(text.trim().replace(",", "."));
    const f = 10 ** decimals;
    if (Number.isFinite(v) && v >= min) {
      const r = Math.round(v * f) / f;
      onCommit(r);
      setText(String(r));
    } else setText(String(value));
  };
  return (
    <label className="flex items-center gap-2">
      <input
        type="text"
        inputMode={decimals ? "decimal" : "numeric"}
        value={text}
        onChange={(e) => setText(e.target.value)}
        onBlur={commit}
        onKeyDown={(e) => e.key === "Enter" && (e.target as HTMLInputElement).blur()}
        aria-label={label}
        className="h-8 w-20 rounded-ctl border border-line bg-raised px-2.5 text-right font-mono text-[13px] outline-none focus:border-line-strong"
      />
      <span className="w-9 font-mono text-[12.5px] text-muted">{suffix}</span>
    </label>
  );
}

function StorageActivityLine({
  status,
  enabled,
  last,
  onRun,
}: {
  status: StorageStatus | null;
  enabled: boolean;
  last: StorageStatus["last"] | undefined;
  onRun: () => void;
}) {
  let line: ReactNode = null;
  if (status?.busy) {
    line = (
      <span className="flex min-w-0 items-center gap-2 text-muted">
        <span className="anim-rec size-1.5 shrink-0 rounded-full bg-accent" />
        <span className="truncate">{status.busy}</span>
      </span>
    );
  } else if (status?.note) {
    line = (
      <span className="flex min-w-0 items-center gap-2 text-warn">
        <Warning size={14} className="shrink-0" />
        <span className="truncate">{status.note}</span>
      </span>
    );
  } else if (last) {
    const parts = [];
    if (last.compressed) parts.push(`compressed ${last.compressed} (saved ${formatBytes(last.saved_bytes)})`);
    if (last.recycled) parts.push(`moved ${last.recycled} to the Recycle Bin (${formatBytes(last.recycled_bytes)})`);
    line = (
      <span className="truncate text-faint">
        {formatDateTime(last.at)}: {parts.join(", ")}
      </span>
    );
  }
  if (!line && !enabled) return null;
  return (
    <div className="mt-2.5 flex min-h-7 items-center justify-between gap-4 text-[12.5px]">
      <div className="min-w-0">{line}</div>
      {enabled && (
        <Button size="sm" variant="ghost" onClick={onRun} disabled={!!status?.busy}>
          Check now
        </Button>
      )}
    </div>
  );
}

function withCustom(list: number[], value: number) {
  return list.includes(value) ? list : [...list, value].sort((a, b) => a - b);
}

function Section({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section className="mb-9">
      <h2 className="mb-1 text-[15px] font-semibold tracking-tight">{title}</h2>
      <div className="divide-y divide-line/70">{children}</div>
    </section>
  );
}

function HotkeyField({ value, onChange }: { value: string; onChange: (v: string) => void }) {
  const [listening, setListening] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!listening) return;
    const onKey = (e: KeyboardEvent) => {
      e.preventDefault();
      e.stopPropagation();
      if (e.code === "Escape") {
        setListening(false);
        return;
      }
      const r = captureHotkey(e);
      if ("pending" in r) return;
      if ("error" in r) {
        setError(r.error);
        return;
      }
      onChange(r.combo);
      setError(null);
      setListening(false);
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [listening, onChange]);

  return (
    <div className="flex flex-col items-end gap-1">
      <button
        onClick={() => {
          setError(null);
          setListening(!listening);
        }}
        onBlur={() => setListening(false)}
        className={cx(
          "flex h-9 min-w-[150px] items-center justify-center rounded-ctl border px-3 text-[13px] transition-colors",
          listening ? "border-accent bg-accent/8 text-accent" : "border-line bg-raised hover:border-line-strong",
        )}
      >
        {listening ? "Press a key combination" : <Combo combo={value} />}
      </button>
      {error && listening && <span className="text-[12px] text-warn">{error}</span>}
    </div>
  );
}

function AudioSection({ sources, onChange }: { sources: AudioSource[]; onChange: (a: AudioSource[]) => void }) {
  const [devices, setDevices] = useState<AudioDevice[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    api
      .listAudioDevices()
      .then(setDevices)
      .catch((e) => setError(errorText(e)));
  }, []);

  const update = (i: number, patch: Partial<AudioSource>) =>
    onChange(sources.map((s, j) => (j === i ? { ...s, ...patch } : s)));

  const add = (kind: SourceKind) => {
    const n = sources.filter((s) => s.kind === kind).length;
    const base = kind === "output" ? "Game" : "Mic";
    onChange([...sources, { kind, device: null, device_name: null, label: n ? `${base} ${n + 1}` : base, gain: 1 }]);
  };

  return (
    <Section title="Audio">
      <p className="py-3 text-[12.5px] leading-relaxed text-faint">
        Every source is saved as its own track, plus a mix that Discord and players hear. Up to 8 sources.
      </p>
      {sources.length === 0 && <p className="py-3 text-[13px] text-muted">No audio. Clips will be silent.</p>}
      {sources.map((s, i) => (
        <SourceRow
          key={i}
          source={s}
          devices={devices?.filter((d) => d.kind === s.kind) ?? null}
          onChange={(p) => update(i, p)}
          onRemove={() => onChange(sources.filter((_, j) => j !== i))}
        />
      ))}
      {error && <p className="py-2 text-[12.5px] text-warn">Could not list devices: {error}</p>}
      <div className="flex gap-2 pt-3">
        <Button size="sm" onClick={() => add("output")} disabled={sources.length >= 8}>
          <Plus size={14} /> Output device
        </Button>
        <Button size="sm" onClick={() => add("input")} disabled={sources.length >= 8}>
          <Plus size={14} /> Microphone
        </Button>
      </div>
    </Section>
  );
}

function SourceRow({
  source,
  devices,
  onChange,
  onRemove,
}: {
  source: AudioSource;
  devices: AudioDevice[] | null;
  onChange: (p: Partial<AudioSource>) => void;
  onRemove: () => void;
}) {
  const Icon = source.kind === "output" ? SpeakerHigh : Microphone;
  const missing = source.device && devices && !devices.some((d) => d.id === source.device);
  const def = devices?.find((d) => d.default);

  return (
    <div className="grid grid-cols-[28px_140px_1fr_150px_32px] items-center gap-3 py-3">
      <Icon size={18} className="text-muted" />
      <input
        value={source.label}
        onChange={(e) => onChange({ label: e.target.value })}
        aria-label="Track name"
        className="h-8 rounded-ctl border border-line bg-raised px-2.5 text-[13px] outline-none focus:border-line-strong"
      />
      <select
        value={source.device ?? ""}
        onChange={(e) => {
          const id = e.target.value || null;
          onChange({ device: id, device_name: devices?.find((d) => d.id === id)?.name ?? null });
        }}
        aria-label="Device"
        className="h-8 min-w-0 rounded-ctl border border-line bg-raised px-2 text-[13px] text-fg outline-none focus:border-line-strong"
      >
        <option value="">Windows default{def ? ` (${def.name})` : ""}</option>
        {devices?.map((d) => (
          <option key={d.id} value={d.id}>
            {d.name}
          </option>
        ))}
        {missing && <option value={source.device!}>{source.device_name ?? "Unknown device"} (not connected)</option>}
      </select>
      <label className="flex items-center gap-2" title="Volume in the mix">
        <input
          type="range"
          min={0}
          max={2}
          step={0.05}
          value={source.gain}
          onChange={(e) => onChange({ gain: Number(e.target.value) })}
          aria-label="Volume"
          className="w-full"
        />
        <span className="w-10 text-right font-mono text-[12px] text-muted">{Math.round(source.gain * 100)}%</span>
      </label>
      <IconButton label="Remove source" onClick={onRemove}>
        <Trash size={15} />
      </IconButton>
    </div>
  );
}
