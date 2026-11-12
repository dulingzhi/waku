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
 *   - `Waku-<version>-Setup.exe` needs Inno Setup 6.
 *
 * Usage:
 *   bun scripts/package-local.ts
 */
import { copyFile, mkdir, rm, stat } from "node:fs/promises";
import { join, resolve } from "node:path";

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
await mkdir(join(staging, "resources"), { recursive: true });

// The app resolves the daemon next to itself, so both stay side by side.
const binaries = ["waku.exe", "waku-daemon.exe", "waku_js_repl.exe", "waku_computer_use.exe"];
for (const binary of binaries) {
  await copyFile(join(release, binary), join(staging, binary));
}
await copyFile(join(projectRoot, "LICENSE"), join(staging, "LICENSE"));

await rm(archive, { force: true });
await mkdir(join(projectRoot, "dist"), { recursive: true });
// Windows ships bsdtar, which writes a zip when the output name says so.
await Bun.$`tar -a -c -f ${archive} -C ${join(projectRoot, "dist", "staging")} ${directoryName}`;

const bytes = (await stat(archive)).size;
console.log(`Created ${archive} (${(bytes / 1024 / 1024).toFixed(1)} MiB)`);
console.log(`Staged ${staging}`);
