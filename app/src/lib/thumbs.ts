import { api, fileUrl, type Clip } from "./api";
import { useApp } from "./store";

/**
 * Thumbnails are rendered here, from the clip itself, one at a time: load the video, seek a
 * little in, draw a frame, hand the JPEG to the backend cache. The measured duration rides along.
 */
const queued = new Set<string>();
const failed = new Set<string>();
let running = false;

export function ensureThumbs(clips: Clip[], skip: string | null) {
  for (const c of clips) {
    if (!c.thumb && c.path !== skip && !queued.has(c.path) && !failed.has(c.path)) {
      queued.add(c.path);
    }
  }
  if (!running) void pump();
}

async function pump() {
  running = true;
  while (queued.size) {
    const path = queued.values().next().value as string;
    queued.delete(path);
    try {
      const { jpeg, duration } = await renderThumb(path);
      const thumb = await api.storeThumb(path, jpeg, duration);
      useApp.getState().patchClip(path, { thumb, duration: duration > 0 ? duration : null });
    } catch (e) {
      failed.add(path);
      console.warn("thumbnail failed", path, e);
    }
  }
  running = false;
}

function once(el: HTMLElement, event: string, ms: number) {
  return new Promise<void>((resolve, reject) => {
    const t = setTimeout(() => reject(new Error(`${event} timed out`)), ms);
    el.addEventListener(
      event,
      () => {
        clearTimeout(t);
        resolve();
      },
      { once: true },
    );
    el.addEventListener("error", () => reject(new Error("video error")), { once: true });
  });
}

async function renderThumb(path: string): Promise<{ jpeg: Uint8Array; duration: number }> {
  const v = document.createElement("video");
  v.muted = true;
  v.preload = "auto";
  // The asset protocol is another origin; without CORS mode the canvas is "tainted" and
  // refuses to export the frame.
  v.crossOrigin = "anonymous";
  v.src = fileUrl(path);
  try {
    await once(v, "loadedmetadata", 10_000);
    const duration = isFinite(v.duration) ? v.duration : 0;
    v.currentTime = duration > 0 ? Math.min(2, duration * 0.3) : 1;
    await once(v, "seeked", 10_000);
    const w = 480;
    const h = 270;
    const canvas = document.createElement("canvas");
    canvas.width = w;
    canvas.height = h;
    const ctx = canvas.getContext("2d")!;
    // Cover-fit so ultrawide or 4:3 captures still fill the 16:9 card.
    const scale = Math.max(w / v.videoWidth, h / v.videoHeight);
    const dw = v.videoWidth * scale;
    const dh = v.videoHeight * scale;
    ctx.drawImage(v, (w - dw) / 2, (h - dh) / 2, dw, dh);
    const blob = await new Promise<Blob>((res, rej) =>
      canvas.toBlob((b) => (b ? res(b) : rej(new Error("encode failed"))), "image/jpeg", 0.82),
    );
    return { jpeg: new Uint8Array(await blob.arrayBuffer()), duration };
  } finally {
    v.removeAttribute("src");
    v.load();
  }
}
