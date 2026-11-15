# Waku roadmap

Where the app is going over the next four months, what each stage has to prove
before it counts as done, and what is deliberately left out. Written as a
planning reference: a stage that fails its check does not roll into the next
one, it goes back.

## Where the project stands

The architecture is not the problem. The desktop is an RPC client of a
standalone daemon, the wire contract lives in `waku-protocol`, and the browser
and mobile clients generate their types from it. That shape is worth keeping
and nothing here proposes changing it.

What the project actually carries:

| Strength | Where it shows |
| --- | --- |
| Broadest provider coverage among comparable clients | 11 providers over 7 transports, every one a long-lived session (`docs/providers.md`) |
| Provider-agnostic transcript | one `ActivityItem` normalization every driver feeds |
| Native performance discipline | frame budgets and the counter-based measurement method in `docs/performance.md` |
| Quality baseline | `cargo clippy --all-targets` at zero warnings, run on every pull request; releases gated on the test suite |

What it does not carry yet:

| Gap | Evidence | Why it matters |
| --- | --- | --- |
| Provider regressions have no automated net | 30 `#[ignore]`d tests drive real CLIs; CI runs none of them, and provider fixes dominate `CHANGELOG.md` | an upstream release breaks a transport and a human finds out |
| The three clients drift | `apps/mobile` is the busiest tree in the last 100 commits; `apps/web` sees far less | one product, three behaviours |
| Remote daemons are incomplete | daemon terminals `bail!` on every non-unix host (`crates/waku-core/src/terminal.rs`) | a Windows daemon can never serve a remote terminal |
| Documentation contradicts the code | `README.md` and `CONTRIBUTING.md` call the browser macOS-only while `docs/windows.md` devotes a section to its WebView2 host | a stale claim costs more than a missing one |
| Risk concentrates in few files | `right_panel.rs` 4786 lines, `composer.rs` 3850, `runtime.rs` 3638 — the three most-edited files, all without tests | changes land where protection is thinnest |

## Stage 1 — Make the documentation true (about one week)

The cheapest credibility win available, and it has to come first: every later
stage is planned against what these documents claim.

1. Rewrite the browser's platform section in `README.md` and `CONTRIBUTING.md`
   from what `docs/windows.md` and `src/browser.rs` actually implement.
2. Re-check the "At a glance" table in `docs/providers.md` against all 11
   `ProviderKind` variants.
3. Record the required checks — `fmt`, `clippy`, `test`, `protocol:check`,
   per-package suites — and the "all green" rule in `CONTRIBUTING.md`.
4. Record the current test counts as a regression baseline.

**Done when** no document makes a claim the code contradicts; every platform
statement can be traced to an implementation or is marked unimplemented.

## Stage 2 — A net for provider regressions (about three weeks)

The highest-value stage, because provider coverage is both the strongest
feature and the largest risk.

1. Golden fixtures per provider, recorded from real payloads. The repo has one
   fixture today (`codex_fork.sh`).
2. Cover the four that broke most recently first: OpenCode 2.0's rename, Cursor
   model discovery, Pi's startup handshake, Claude's model naming.
3. Combination cases — fork after steering, rewind at a turn boundary. The
   0.1.20 "forked at the wrong turn" bug was a combination nobody tested.
4. Failure semantics per provider, in Kimi's shape: record what the protocol
   says when the turn cannot run at all, since Kimi reports a clean `end_turn`.
5. Split the 30 `#[ignore]`d tests into CI-runnable (recorded or mocked) and
   genuinely hardware-bound. The first group joins CI; the second stays manual
   and moves onto the release checklist.

**Done when** at least 12 of the ignored tests run in CI, a deliberately broken
event mapping fails the suite, and every provider has a recorded failure
semantics note.

## Stage 3 — Align the clients and finish remote (about four weeks)

1. A capability matrix: what each of desktop, web and mobile supports, with a
   code reference or an explicit gap per cell.
2. Close the obvious drops — mobile's terminal is unavailable on web
   (`apps/mobile/src/components/task-surface-sheet.tsx`), and the web terminal
   is limited against a remote daemon.
3. Remote terminals. The protocol already carries `OpenTerminal`,
   `WriteTerminal`, `ResizeTerminal` and `CloseTerminal`, but
   `crates/waku-core/src/terminal.rs` refuses every non-unix host.
4. Desktop remote terminal wiring. `src/app/right_panel.rs` returns early for a
   remote daemon behind a comment that says it awaits a daemon-owned terminal —
   a comment that is now stale, since the protocol has one.

**Done when** a desktop client against a unix remote daemon opens a terminal
and runs a command, and every remaining three-client difference has a written
reason.

**Caution** Windows needs a real ConPTY implementation, which is a separate
piece of work. Land unix first; schedule Windows on its own.

## Stage 4 — Maintainability, then new work (about four weeks)

1. Split the giant files, starting with `composer.rs` — 3850 lines and no
   tests. One cohesive unit at a time, with tests and a performance check after
   each.
2. Pull pure functions out of `composer.rs`, `transcript.rs` and
   `streaming.rs` — roughly 6000 lines between them with no unit tests.
3. Only then, explore parallel task orchestration: several agents working one
   problem with the results merged.

**Done when** `composer.rs` is three files of at most 1500 lines each, the
streaming CPU figures in `docs/performance.md` do not regress, and the
extracted functions carry tests.

## Stage 5 — Remote access over a relay, end to end encrypted (unscheduled)

Detailed in `docs/remote-access.md`. It depends on stage 3's remote groundwork
and on a security design settling before any code is written, which is why it
is a separate document rather than a bullet here.

## What this plan deliberately does not do

- **No new providers.** Eleven already means eleven maintenance surfaces. A
  provider is added when a user needs it, not to grow a list.
- **No account system or cloud sync.** "No Waku account or remote service
  required" is the product's position, not an unfinished feature.
- **No orchestration before the foundation holds.** Stages 1 to 3 come first.
- **No splitting files for their own sake.** Only the most-edited, least-tested
  ones.

## How a stage is judged

Each stage closes with evidence, not a summary: command output, test results,
measured numbers. A stage that misses its check twice gets re-evaluated rather
than pushed through.

Two standing rules:

- Nothing relaxes a gate — a shorter test matrix, a skipped check, a softer
  release condition — without the user's decision, and the reason is recorded
  so it can be re-judged later.
- A stage is not done because its work items are checked off. It is done when
  its check passes.
