# Client capability matrix

What each of Waku's three clients can do, taken from the code rather than from
the README. `crates/waku-protocol/src/protocol.rs` lists every command the
daemon serves; this table is which client issues which.

Read this as a map of differences, not of missing features. Plenty of the gaps
below are deliberate — a phone should not host a browser surface — and the
point, which the roadmap lists first, is that a reader can tell the two apart.

## Matrix

| Capability | Desktop | Web | Mobile |
| --- | --- | --- | --- |
| Terminal (local daemon, unix) | yes | yes | yes (native) |
| Terminal (remote daemon, unix) | **no** | yes | yes |
| Terminal (web build of mobile) | — | no | no |
| Terminal (Windows daemon) | no | no | no |
| Browser surface | yes (macOS, Windows) | no | no |
| File tree | yes | yes | yes |
| File editing | yes | yes | no |
| Diff / review | yes | yes | yes |
| Task create, send, steer, queue | yes | yes | yes |
| Model, effort, service tier | yes | yes | yes |
| Permission approvals | yes | yes | yes |
| User input questions | yes | yes | yes |
| Goals | yes | yes | yes |
| Skills | yes | yes | no |
| Usage and cost | yes | yes | no |
| Attachments | yes | yes | yes |
| Resume / fork / rewind | yes | yes | no |
| Background work | yes | yes | no |
| Computer Use | yes (Debug builds) | **no, hardcoded off** | no |
| Multiple daemons | yes | **one connection** | yes |
| Theme and language | yes | yes | language only |

Every "no" and every "limited" cell has a code reference in the section below.
A cell without one would be a guess, so there are none.

## Where each gap lives

**Desktop, remote daemon.** Ten `is_remote()` branches, which split into three
kinds:

- *Graceful degradation* — adding a project browses the daemon's filesystem
  instead of opening a native path prompt (`src/app/sessions.rs`). Not a gap.
- *Local filesystem operations that cannot work remotely* — revealing a file in
  Finder, opening a skill's folder (`src/app/right_panel.rs`,
  `src/app/skills_page.rs`, `src/app/transcript_view.rs`,
  `src/app/composer.rs`, `src/app/background_work.rs`). Correct by nature; the
  path belongs to the daemon host.
- *Functional gaps* — the terminal, the Daemon settings page, and daemon
  exposure configuration (`src/app/right_panel.rs`, `src/app/settings.rs`).

The terminal is the interesting one: the protocol has served
`OpenTerminal`/`WriteTerminal`/`ResizeTerminal`/`CloseTerminal` since the web
client needed them, the web client uses them, and the desktop does not. The
comment at `src/app/right_panel.rs` still says it waits for the protocol to
grow a daemon terminal, which it already has.

**Windows daemon terminal.** `crates/waku-core/src/terminal.rs` implements the
daemon side behind `#[cfg(unix)]` and bails on every other platform. The
`alacritty_terminal` crate this builds on does ship a ConPTY backend
(`src/tty/windows/`), so the gap is the PTY's API shape rather than a missing
capability: the unix `Pty` exposes `file()` and `child()`, the Windows one
exposes `child_watcher()` and the `EventedReadWrite` reader and writer.

**Mobile, fork, rewind and background work.** Absent, not half-built.
`apps/mobile/src/lib/runtime-context.tsx` exposes attach, prompt, steer, create,
cancel, respond, user input, options, goal, rename, delete and queued-message
removal — and nothing else. No fork, no rewind, no background work, while
`packages/waku-client/src/generated/Command.ts` carries all four. The i18n
catalogue still holds `errors.rollback_*` strings no code references, which is
what a planned surface that never landed looks like.

Closing these means starting at the runtime layer — issuing the request,
handling the forked session, handling `checkpointWarning` — rather than adding
a button.

**Mobile, Skills and Usage.** No hits for the skills commands anywhere under
`apps/mobile`, and no settings route for usage. The i18n catalogue still carries
the strings, which is what an unfinished surface looks like.

**Web, Computer Use.** `apps/web/src/lib/runtime-context.tsx` passes
`computerUseEnabled: false` at both session-start call sites. Whether that is a
gap or a decision is not something the code says: Computer Use drives whichever
desktop the daemon runs on, so a browser reaching a daemon on another machine is
a different proposition from a native client on the same one. Recorded here as a
difference; the intent needs the product's answer.

**Web, multiple daemons.** `apps/web/src/lib/connection.ts` stores one
`ConnectionConfig` — an address, a token and a remember flag — under one of two
keys. The parser reads a single object, not a list, so this is a single
connection in the storage model rather than a list the UI declines to show.
Mobile, by contrast, manages profiles and has a `/daemons` route.

## Using this table

A row that differs between clients is only a defect when the difference is not
the point. Before closing one, decide which kind it is:

1. **Deliberate** — a browser surface on a phone, or a native path prompt for a
   remote machine. Write the reason down and leave the cell alone.
2. **Unfinished** — the code exists and nothing calls it (mobile fork and
   rewind). Cheap to close, and the cheapest wins available.
3. **Not yet wired** — the protocol and one other client already do it (the
   desktop remote terminal). The work is a client change, not a protocol one.

Only the second and third kinds are on the roadmap.
