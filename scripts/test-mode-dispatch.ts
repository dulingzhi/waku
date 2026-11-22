#!/usr/bin/env bun
// Mode-dispatch smoke test. No network, no SDK download, no window system.
//
// A package ships one `waku` executable that is the GUI, the daemon, the
// JavaScript REPL and the Cua Driver host. That only works if the mode word
// selects the right job, and the failure is silent: a helper whose flag was lost
// starts the window system and answers nothing. These checks run each mode as a
// real child process and assert what it did, so a broken flag fails CI instead
// of a user's agent session.
//
// Usage: bun scripts/test-mode-dispatch.ts [<waku>]
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { basename, join, resolve } from "node:path";

const suffix = process.platform === "win32" ? ".exe" : "";
const [program = `target/debug/waku${suffix}`] = process.argv.slice(2);
assert(program, "Pass the Waku executable");
const executable = resolve(program);
// A merged build selects a mode with a flag; a dedicated helper takes none.
const merged = basename(executable).startsWith("waku.");

/** Run the executable and collect its output, with a deadline. */
function run(
  args: string[],
  input = "",
): Promise<{ code: number | null; stdout: string; stderr: string }> {
  return new Promise((resolveRun, rejectRun) => {
    const child = spawn(executable, args, { windowsHide: true });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (chunk) => (stdout += chunk));
    child.stderr.on("data", (chunk) => (stderr += chunk));
    child.on("error", rejectRun);
    const timer = setTimeout(() => {
      child.kill();
      rejectRun(new Error(`${args.join(" ")} did not exit within 20s`));
    }, 20_000);
    child.on("exit", (code) => {
      clearTimeout(timer);
      resolveRun({ code, stdout, stderr });
    });
    child.stdin.end(input);
  });
}

/** Read a daemon's readiness line out of a stream of JSON. */
function jsonLines(stdout: string): any[] {
  return stdout
    .split("\n")
    .filter((line) => line.trim().startsWith("{"))
    .flatMap((line) => {
      try {
        return [JSON.parse(line)];
      } catch {
        return [];
      }
    });
}

/// Run a process until it prints what we are waiting for, then stop it.
///
/// A daemon is a long-lived server: it never exits on its own, so waiting for an
/// exit would hang CI. `until` decides when the expected output has arrived.
function runUntil(options: {
  args: string[];
  env?: Record<string, string>;
  until(stdout: string, stderr: string): boolean;
  timeoutMs?: number;
}): Promise<{
  stdout: string;
  stderr: string;
  timedOut: boolean;
  code: number | null;
}> {
  const timeoutMs = options.timeoutMs ?? 30_000;
  return new Promise((resolveRun, rejectRun) => {
    const child = spawn(executable, options.args, {
      windowsHide: true,
      env: { ...process.env, ...options.env },
    });
    let stdout = "";
    let stderr = "";
    let settled = false;
    const finish = (timedOut: boolean) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      const outcome = { stdout, stderr, timedOut, code: child.exitCode };
      if (child.exitCode === null) child.kill();
      resolveRun(outcome);
    };
    const check = () => {
      if (options.until(stdout, stderr)) finish(false);
    };
    child.stdout.on("data", (chunk) => {
      stdout += chunk;
      check();
    });
    child.stderr.on("data", (chunk) => {
      stderr += chunk;
      check();
    });
    child.on("error", (error) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      rejectRun(error);
    });
    child.on("exit", () => finish(false));
    const timer = setTimeout(() => finish(true), timeoutMs);
    child.stdin.end();
  });
}

// 1. The REPL answers MCP and actually runs JavaScript.
//
// This is the check that a lost `--js-repl` would fail: the GUI does not speak
// JSON-RPC, so the handshake never arrives.
const replMode = merged ? ["--js-repl"] : [];
const replInput = [
  {
    jsonrpc: "2.0",
    id: 1,
    method: "initialize",
    params: {
      protocolVersion: "2025-06-18",
      capabilities: {},
      clientInfo: { name: "waku-dispatch-test", version: "1" },
    },
  },
  {
    jsonrpc: "2.0",
    id: 2,
    method: "tools/call",
    params: {
      name: "js",
      arguments: { code: "jsRepl.write('dispatch=' + (6 * 7));" },
    },
  },
]
  .map((request) => JSON.stringify(request))
  .join("\n")
  .concat("\n");
const repl = await run(replMode, replInput);
const replMessages = jsonLines(repl.stdout);
const initialized = replMessages.find((message) => message.id === 1);
assert.ok(
  initialized?.result?.serverInfo,
  `--js-repl did not answer initialize (exit ${repl.code}, stderr: ${repl.stderr.slice(0, 400)})`,
);
assert.equal(initialized.result.serverInfo.name, "waku_js_repl");
// The kernel really ran: a GUI that never started would leave this empty.
const evaluated = replMessages.find((message) => message.id === 2);
assert.equal(
  evaluated?.result?.content?.[0]?.text,
  "dispatch=42",
  `--js-repl did not evaluate JavaScript: ${repl.stdout.slice(0, 400)}`,
);
assert.notEqual(evaluated?.result?.isError, true);

// 2. The Cua Driver host is reached, not the GUI.
//
// With the SDK absent this fails on the SDK load, which is the proof that the
// helper ran at all; with the SDK present it prints the tool catalog. Either way
// the process must exit instead of opening a window.
if (merged) {
  const helper = await run(["--computer-use", "list-tools"]);
  const enteredHelper = /Cua Driver SDK is missing|Waku Computer Use/.test(
    helper.stderr,
  );
  assert.ok(
    enteredHelper,
    `--computer-use did not reach the helper host (exit ${helper.code}, stderr: ${helper.stderr.slice(0, 400)})`,
  );
  // A tool catalog means the SDK loaded; validate its shape.
  if (helper.code === 0) {
    const catalog = JSON.parse(helper.stdout);
    assert.ok(Array.isArray(catalog.tools) && catalog.tools.length > 0);
  }

  // 3. The flag without a mode still reaches the helper rather than the GUI.
  const modeLess = await run(["--computer-use"]);
  assert.ok(
    /Cua Driver SDK is missing|expected mcp or list-tools/.test(modeLess.stderr),
    `--computer-use without a mode fell through to the GUI (exit ${modeLess.code}, stderr: ${modeLess.stderr.slice(0, 400)})`,
  );
}

// 4. The daemon mode is dispatched before the window system.
//
// A ready daemon prints one readiness line and then waits for clients forever,
// so this checks for that line and stops the process instead of waiting for an
// exit. The line is the evidence that `--daemon` chose the daemon job; the GUI
// never prints it. The token is supplied here because a daemon refuses to start
// without one, and that refusal is a different outcome from "never dispatched".
const daemon = await runUntil({
  args: ["--daemon"],
  env: { WAKU_DAEMON_TOKEN: "mode-dispatch-smoke-token" },
  until: (stdout) => jsonLines(stdout).some((line) => line.address),
});
assert.equal(
  daemon.timedOut,
  false,
  `--daemon never reported readiness (stderr: ${daemon.stderr.slice(0, 400)})`,
);
const ready = jsonLines(daemon.stdout).find((line) => line.address);
assert.ok(
  ready?.address,
  `--daemon did not report readiness (exit ${daemon.code}, stderr: ${daemon.stderr.slice(0, 400)})`,
);
assert.ok(ready.pid !== undefined);

console.log(
  `Mode dispatch smoke passed (${basename(executable)}: REPL evaluates, ${merged ? "helper and daemon dispatch" : "daemon dispatch"}).`,
);
