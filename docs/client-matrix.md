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
| Skills | yes | yes | yes |
| Usage and cost | yes | yes | no |
| Attachments | yes | yes | yes |
| Resume / fork / rewind | yes | yes | resume, fork and rewind |
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

**Mobile, fork and rewind.** Closed. `apps/mobile/src/lib/runtime-context.tsx`
now exposes `forkSessionFromResponse` and `rewindSessionToMessage` with the same
signatures the web client uses, and the transcript rows carry the controls. Two
details are worth knowing before changing them:

- The affordances are per-message, not in the task menu, because both commands
  take a turn count. `responseForkTurnCount` and `userMessageRewindTurnCount` in
  `apps/mobile/src/lib/session-presentation.ts` decide eligibility, and a
  message that fails leaves the control out entirely rather than showing it
  disabled. Fork does *not* require a checkpoint ref — the daemon's
  `validate_response_fork` reports `checkpointWarning` instead of failing — but
  rewind does, because the restore needs the ref.
- **The confirmation differs between clients, on purpose.** Desktop
  (`src/app/components.rs`) and web (`apps/web/src/components/transcript.tsx`)
  both confirm a rewind by opening an edit bubble seeded with the original
  prompt: the submit *is* the confirmation, and there is no separate dialog.
  Mobile has no message-edit bubble yet, so it uses an `Alert` with a
  destructive button instead. When the bubble lands, delete the alert rather
  than keeping both. The alert body currently borrows `session.rewound`, whose
  wording reads as a completed rewind rather than a pending one, because
  `locales/*.yml` had no confirm-body key when this shipped.

Rewind also invalidates the `session-turn-refs` query after succeeding, since
that is the same ref set the controls are gated on; without it the buttons stay
stale for up to the query's `staleTime` and a destructive action can be
repeated against refs that no longer exist.

**Mobile, background work.** Still absent, and not planned for this phase.
Unlike fork and rewind this is not a request/response surface: the daemon pushes
`backgroundWork` runtime events, and the ~250-line decode-and-reduce state
machine (including `reconcileProcesses` / `reconcileLive`) lives inside
`apps/web/src/lib/runtime-context.tsx` rather than in `packages/waku-client`.
Porting it means moving that module to the shared package first, which touches
the web client — a change that needs its own authorisation and verification.

**Mobile, Skills.** Closed. `apps/mobile/src/app/skills.tsx` is the library,
`skills/[key].tsx` the detail pane, and `lib/skills-presentation.ts` the
grouping, search, labelling and route-key logic.

`crates/waku-core/src/skills.rs` hashes each install's directory **and its
`enabled` flag** into `row_key`, so a skill's key changes the moment it is
toggled. Any client that stores that key — a route parameter, a saved selection
— would lose its target on the next toggle. The desktop uses it only to build
element ids, which are rebuilt each frame, so it does not notice. Mobile routes
on `(project, name)` instead, which is the daemon's own grouping identity and
does not move.

Toggle and delete take every install location at once, because the list
aggregates one skill installed into several ecosystems into a single row, and
that is what the user means. The detail pane prints at most three paths and
counts the rest; the delete button states how many locations go. Toggling
writes through the catalogue cache immediately (the desktop answers the switch
before the rescan too) and then refetches. That refetch reorders the list,
because `rowKey` changes with the flag — expected, not a defect.

**Delete confirmation is a two-step button on all three clients, and that is
deliberately not how rewind works.** Rewind on mobile uses an `Alert` because
the desktop and web confirm it by opening an edit bubble mobile does not have
yet. Delete needs no bubble, so all three keep the same armed-button idiom
(`skills_delete_arming` / `deleteArmed`). Do not unify the two without first
giving mobile a message-edit bubble.

**Mobile, Usage.** Still absent: no hits for the usage commands anywhere under
`apps/mobile`, and no settings route for it. The i18n catalogue carries the
strings, which is what an unfinished surface looks like.

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
2. **Unfinished** — the code exists and nothing calls it (mobile Usage). Cheap
   to close, and the cheapest wins available.
3. **Not yet wired** — the protocol and one other client already do it (the
   desktop remote terminal). The work is a client change, not a protocol one.

Only the second and third kinds are on the roadmap.
