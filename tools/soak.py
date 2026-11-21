"""Soak test for bettr-engine.

Runs the engine for a while, saves a clip every N seconds and validates every clip:
- ffprobe: one H.264 stream + AAC streams, video at least the requested length;
- every audio stream ends within 60 ms of the video;
- ffmpeg full decode of all streams with -xerror (any corrupt packet fails);
- engine status: dropped frames and memory over time (catches leaks).

Usage:
    python tools/soak.py --minutes 30 --interval 20 [--engine target/release/bettr-engine.exe]
                         [--ffmpeg-dir .tools/ffmpeg-master-latest-win64-gpl/bin] [-- extra engine args]
"""

import argparse
import json
import queue
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--minutes", type=float, default=10)
    ap.add_argument("--interval", type=float, default=20)
    ap.add_argument("--seconds", type=int, default=15, help="replay length")
    ap.add_argument("--engine", default=str(ROOT / "target/release/bettr-engine.exe"))
    ap.add_argument("--ffmpeg-dir", default=str(ROOT / ".tools/ffmpeg-master-latest-win64-gpl/bin"))
    ap.add_argument("--keep", action="store_true", help="keep passing clips too")
    ap.add_argument("extra", nargs="*", help="extra engine arguments (after --)")
    a = ap.parse_args()

    ffprobe = str(Path(a.ffmpeg_dir) / "ffprobe.exe")
    ffmpeg = str(Path(a.ffmpeg_dir) / "ffmpeg.exe")
    out = Path(tempfile.mkdtemp(prefix="bettr-soak-"))
    cmd = [a.engine, "--seconds", str(a.seconds), "--out", str(out), *a.extra]
    log = open(out / "engine.log", "w", encoding="utf8")
    eng = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=log, text=True, bufsize=1)
    events: "queue.Queue[dict]" = queue.Queue()

    def reader():
        for line in eng.stdout:
            try:
                events.put(json.loads(line))
            except json.JSONDecodeError:
                pass

    threading.Thread(target=reader, daemon=True).start()

    def send(c: str):
        try:
            eng.stdin.write(json.dumps({"cmd": c}) + "\n")
            eng.stdin.flush()
        except OSError:
            pass  # engine gone; reported via poll()

    def wait_for(kinds, timeout):
        end = time.time() + timeout
        while time.time() < end:
            try:
                e = events.get(timeout=0.2)
            except queue.Empty:
                continue
            if e.get("event") in ("restarting", "restarted", "warning", "error"):
                print("  engine:", e)
                results["engine_events"].append(e)
            if e.get("event") in kinds:
                return e
        return None

    results = {"clips": 0, "failures": [], "engine_events": [], "status": []}
    ready = wait_for({"ready"}, 20)
    if not ready:
        print("engine did not start; see", out / "engine.log")
        return 2
    print("engine ready:", ready)
    start = time.time()
    first_save = start + a.seconds + 2  # let the buffer fill
    next_save = first_save
    while time.time() - start < a.minutes * 60:
        time.sleep(max(0.0, min(next_save, time.time() + 10) - time.time()))
        if eng.poll() is not None:
            results["failures"].append({"error": f"engine exited with {eng.returncode}"})
            break
        send("status")
        st = wait_for({"status"}, 5)
        if st:
            st["t"] = round(time.time() - start, 1)
            st["mem_mb"] = mem_mb(eng.pid)
            results["status"].append(st)
        if time.time() >= next_save:
            next_save += a.interval
            send("save")
            ev = wait_for({"clip_saved"}, 10)
            if not ev:
                results["failures"].append({"error": "no clip_saved within 10 s"})
                continue
            results["clips"] += 1
            problems = validate(ffprobe, ffmpeg, Path(ev["path"]), a.seconds)
            tag = "OK " if not problems else "BAD"
            mem = results["status"][-1]["mem_mb"] if results["status"] else "?"
            print(f"[{tag}] clip {results['clips']:3d}  {ev['seconds']:.2f}s  {ev['bytes'] / 1e6:.1f} MB  "
                  f"dropped={st and st.get('dropped')}  mem={mem} MB  {'; '.join(problems)}")
            if problems:
                results["failures"].append({"clip": ev["path"], "problems": problems})
            elif not a.keep:
                Path(ev["path"]).unlink(missing_ok=True)

    send("quit")
    try:
        eng.wait(10)
    except subprocess.TimeoutExpired:
        eng.kill()
        results["failures"].append({"error": "engine did not quit within 10 s"})
    mems = [s["mem_mb"] for s in results["status"] if s.get("mem_mb")]
    summary = {
        "minutes": round((time.time() - start) / 60, 1),
        "clips": results["clips"],
        "failures": len(results["failures"]),
        "dropped_frames": results["status"][-1].get("dropped") if results["status"] else None,
        "ticks": results["status"][-1].get("ticks") if results["status"] else None,
        "mem_mb_first_last_max": [mems[0], mems[-1], max(mems)] if mems else None,
        "engine_events": len(results["engine_events"]),
        "output_dir": str(out),
    }
    (out / "soak.json").write_text(json.dumps({"summary": summary, **results}, indent=2))
    print(json.dumps(summary, indent=2))
    if not results["failures"] and not a.keep:
        shutil.rmtree(out, ignore_errors=True)
    return 1 if results["failures"] else 0


def mem_mb(pid: int):
    try:
        r = subprocess.run(["tasklist", "/fi", f"PID eq {pid}", "/fo", "csv", "/nh"], capture_output=True, text=True)
        field = r.stdout.strip().split('","')[-1].strip('"')
        return round(int("".join(c for c in field if c.isdigit())) / 1024, 1)
    except Exception:
        return None


def validate(ffprobe: str, ffmpeg: str, clip: Path, want_seconds: int):
    problems = []
    r = subprocess.run([ffprobe, "-v", "error", "-show_entries", "stream=codec_type,codec_name,duration",
                        "-of", "json", str(clip)], capture_output=True, text=True)
    if r.returncode != 0:
        return [f"ffprobe failed: {r.stderr.strip()[:200]}"]
    streams = json.loads(r.stdout).get("streams", [])
    video = [s for s in streams if s["codec_type"] == "video"]
    audio = [s for s in streams if s["codec_type"] == "audio"]
    if len(video) != 1 or video[0]["codec_name"] != "h264":
        problems.append(f"video streams: {video}")
        return problems
    vdur = float(video[0]["duration"])
    if vdur < want_seconds - 0.05:
        problems.append(f"video {vdur:.2f}s < {want_seconds}s")
    for s in audio:
        d = float(s.get("duration", 0))
        if abs(d - vdur) > 0.06:
            problems.append(f"audio ends {1000 * (d - vdur):+.0f} ms vs video")
    if not audio:
        problems.append("no audio")
    d = subprocess.run([ffmpeg, "-v", "error", "-xerror", "-i", str(clip), "-map", "0", "-f", "null", "-"],
                       capture_output=True, text=True)
    if d.returncode != 0 or d.stderr.strip():
        problems.append(f"decode: {d.stderr.strip()[:200]}")
    return problems


if __name__ == "__main__":
    sys.exit(main())
