// Build the shim and stage it as a Tauri sidecar (externalBin).
// Tauri expects src-tauri/binaries/pingmybell-shim-<target-triple>[.exe] and
// places it next to the app binary (plain name) in dev and in bundles.
import { mkdirSync, copyFileSync } from "node:fs";
import { join } from "node:path";

const root = join(import.meta.dir, "..");

const rustc = Bun.spawnSync(["rustc", "-vV"]);
const triple = rustc.stdout
  .toString()
  .split("\n")
  .find((l) => l.startsWith("host:"))
  ?.slice("host:".length)
  .trim();
if (!triple) {
  console.error("could not determine host target triple from rustc -vV");
  process.exit(1);
}

const build = Bun.spawnSync(
  ["cargo", "build", "--release", "-p", "pingmybell-shim"],
  { cwd: root, stdout: "inherit", stderr: "inherit" },
);
if (build.exitCode !== 0) process.exit(build.exitCode ?? 1);

const exe = process.platform === "win32" ? ".exe" : "";
const src = join(root, "target", "release", `pingmybell-shim${exe}`);
const destDir = join(root, "src-tauri", "binaries");
mkdirSync(destDir, { recursive: true });
const dest = join(destDir, `pingmybell-shim-${triple}${exe}`);
copyFileSync(src, dest);
console.log(`sidecar staged: ${dest}`);
