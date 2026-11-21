// Builds the capture engine (release) and places it where Tauri bundles it as a sidecar:
// app/src-tauri/binaries/bettr-engine-<target-triple>.exe. Run before `tauri dev/build`.
import { execFileSync } from "node:child_process";
import { copyFileSync, existsSync, mkdirSync, readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
execFileSync("cargo", ["build", "--release", "-p", "bettr-engine"], { cwd: root, stdio: "inherit" });
const triple = /host: (\S+)/.exec(execFileSync("rustc", ["-vV"]).toString())[1];
const src = join(root, "target", "release", "bettr-engine.exe");
const dir = join(root, "app", "src-tauri", "binaries");
const dst = join(dir, `bettr-engine-${triple}.exe`);
mkdirSync(dir, { recursive: true });
if (!existsSync(dst) || !readFileSync(src).equals(readFileSync(dst))) {
  copyFileSync(src, dst);
  console.log(`engine -> ${dst}`);
}
