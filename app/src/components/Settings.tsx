import { useEffect, useState, type ReactNode } from "react";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { openPath } from "@tauri-apps/plugin-opener";
import { Microphone, Plus, SpeakerHigh, Trash, Warning } from "@phosphor-icons/react";
import { api, errorText, type AudioDevice, type AudioSource, type Settings as S, type SourceKind } from "../lib/api";
import { useApp } from "../lib/store";
import { captureHotkey } from "../lib/hotkey";
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
    <div className="flex min-w-0 flex-1 flex-col">
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
                <Button size="sm" variant="ghost" onClick={() => openPath(saved.out_dir)}>
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
            <Row title="Sound on save" help="A short chime when a clip or recording is saved.">
              <Toggle label="Sound on save" checked={draft.save_sound} onChange={(v) => set("save_sound", v)} />
            </Row>
            <Row title="Keep running in the tray" help="Closing the window keeps clipping in the background.">
              <Toggle
                label="Keep running in the tray"
                checked={draft.close_to_tray}
                onChange={(v) => set("close_to_tray", v)}
              />
            </Row>
          </Section>
        </div>
      </div>

      <div
        className={cx(
          "pointer-events-none absolute inset-x-0 bottom-0 flex justify-center pb-5 pl-[248px] transition-[opacity,transform] duration-300",
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
