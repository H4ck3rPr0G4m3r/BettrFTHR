import { invoke, convertFileSrc } from "@tauri-apps/api/core";

export type SourceKind = "output" | "input";

export interface AudioSource {
  kind: SourceKind;
  /** Endpoint ID; null follows the Windows default device. */
  device: string | null;
  device_name: string | null;
  label: string;
  gain: number;
}

export interface Settings {
  out_dir: string;
  replay_seconds: number;
  fps: number;
  bitrate_kbps: number;
  /** 0 = monitor resolution. */
  height: number;
  cursor: boolean;
  hotkeys: { save: string; record: string };
  audio: AudioSource[];
  buffer_mode: "always" | "games";
  game_detection: boolean;
  save_sound: boolean;
  close_to_tray: boolean;
}

export type Phase = "idle" | "starting" | "running" | "crashed" | "missing";

export interface ReadyInfo {
  adapter: string;
  encoder: string;
  width: number;
  height: number;
  fps: number;
  audio_tracks: string[] | null;
}

export interface Status {
  ticks: number;
  encoded: number;
  dropped: number;
  captured: number;
  buffered_seconds: number;
  buffered_mb: number;
  audio_sources: { track: string; kind: SourceKind; device: string | null }[];
  recording: string | null;
  game: string | null;
}

export interface EngineState {
  phase: Phase;
  info: ReadyInfo | null;
  status: Status | null;
  recording: string | null;
  recording_since: number | null;
  game: string | null;
  last_error: string | null;
  restarts: number;
}

export interface Clip {
  path: string;
  title: string;
  game: string | null;
  kind: "clip" | "recording";
  bytes: number;
  modified: number;
  duration: number | null;
  thumb: string | null;
}

export interface Game {
  name: string;
  exe: string;
}

export interface AudioDevice {
  kind: SourceKind;
  id: string;
  name: string;
  default: boolean;
}

export const api = {
  getSettings: () => invoke<Settings>("get_settings"),
  setSettings: (settings: Settings) => invoke<Settings>("set_settings", { settings }),
  engineState: () => invoke<EngineState>("engine_state"),
  engine: (cmd: Record<string, unknown>) => invoke<void>("engine_command", { cmd }),
  restartEngine: () => invoke<void>("restart_engine"),
  currentGame: () => invoke<Game | null>("current_game"),
  listClips: () => invoke<Clip[]>("list_clips"),
  storeThumb: (path: string, jpeg: Uint8Array, duration: number) =>
    invoke<string>("store_thumb", { path, jpeg: Array.from(jpeg), duration }),
  renameClip: (path: string, title: string) => invoke<string>("rename_clip", { path, title }),
  deleteClips: (paths: string[]) => invoke<void>("delete_clips", { paths }),
  openClipFolder: () => invoke<void>("open_clip_folder"),
  revealClip: (path: string) => invoke<void>("reveal_clip", { path }),
  copyClip: (path: string) => invoke<void>("copy_clip", { path }),
  listAudioDevices: () => invoke<AudioDevice[]>("list_audio_devices"),
};

/** URL the webview can load a local file from (asset protocol, scoped to the library). */
export const fileUrl = (path: string) => convertFileSrc(path);

export const errorText = (e: unknown) => (typeof e === "string" ? e : e instanceof Error ? e.message : String(e));
