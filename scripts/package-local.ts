#!/usr/bin/env bun
/**
 * Local-only portable package without network or Inno Setup.
 *
 * `scripts/bundle-windows.ts` downloads the Cua Driver SDK from GitHub and runs
 * Inno Setup, so it cannot build on a host without internet access or admin
 * rights. This stages the same versioned directory the official zip uses, with
 * the binaries that were already compiled, so the desktop app and its daemon
 * can be exercised in this environment.
 *
 * What it deliberately omits, and why:
 *   - `cua_driver_sdk.dll` / `cua-driver-uia.exe` need the GitHub SDK download.
 *     Computer use therefore starts but has no driver to talk to; every other
 *     feature works. The files it reads from `resources/` are staged.
 *   - `Waku-<version>-Setup.exe` needs Inno Setup 6.
 *
 * Usage:
 *   bun scripts/package-local.ts
 */
import { copyFile, mkdir, rm, stat } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";

const projectRoot = resolve(import.meta.dir, "..");
const packageName = "waku";

const metadata = JSON.parse(
  await Bun.$`cargo metadata --no-deps --format-version 1`.quiet().text(),
) as { packages: { name: string; version: string }[] };
const version = metadata.packages.find(
  (candidate) => candidate.name === packageName,
)?.version;
if (!version) throw new Error(`Cargo package "${packageName}" was not found.`);

const host = (await Bun.$`rustc -vV`.quiet().text())
  .split("\n")
  .find((line) => line.startsWith("host: "))
  ?.slice("host: ".length)
  .trim();
if (!host) throw new Error("could not read the host target triple");

const release = join(projectRoot, "target", "release");
const directoryName = `waku-${version}-${host}`;
const staging = join(projectRoot, "dist", "staging", directoryName);
const archive = join(projectRoot, "dist", `${directoryName}.zip`);

await rm(join(projectRoot, "dist", "staging"), { recursive: true, force: true });
await mkdir(staging, { recursive: true });

// `waku.exe` is both the GUI and the daemon: the desktop spawns itself with
// `--daemon`, so no `waku-daemon.exe` ships any more. The other two are genuine
// helper processes — the JS REPL host and the CUA driver host — launched as
// their own binaries by design.
const binaries = ["waku.exe", "waku_js_repl.exe", "waku_computer_use.exe"];
for (const binary of binaries) {
  await copyFile(join(release, binary), join(staging, binary));
}
await copyFile(join(projectRoot, "LICENSE"), join(staging, "LICENSE"));

// The computer-use files the app reads at runtime. They live in the repository,
// so staging them costs no download — and leaving them out is not a cosmetic
// gap: `packaged_file` bails, so a package without them reports the feature as
// missing from the build rather than as unavailable.
const resources = join(staging, "resources");
for (const [source, relative] of [
  ["resources/computer-use/pi-extension.ts", "computer-use/pi-extension.ts"],
  ["resources/computer-use/CUA-LICENSE", "computer-use/CUA-LICENSE"],
  ["resources/computer-use/SKILL.md", "skills/waku-computer-use/SKILL.md"],
]) {
  const destination = join(resources, relative);
  await mkdir(dirname(destination), { recursive: true });
  await copyFile(join(projectRoot, source), destination);
}

await rm(archive, { force: true });
await mkdir(join(projectRoot, "dist"), { recursive: true });
// Windows ships bsdtar, which writes a zip when the output name says so.
await Bun.$`tar -a -c -f ${archive} -C ${join(projectRoot, "dist", "staging")} ${directoryName}`;

const bytes = (await stat(archive)).size;
console.log(`Created ${archive} (${(bytes / 1024 / 1024).toFixed(1)} MiB)`);
console.log(`Staged ${staging}`);
