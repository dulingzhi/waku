# Provider fixtures

Golden payloads recorded from a real provider CLI, replayed into the driver so
a transport regression fails in CI instead of in a user's task.

This is the design for stage 2 of the [roadmap](roadmap.md). The recorder and
the fixture set do not exist yet; the format below is what both sides build
against, so recording and replaying cannot drift apart.

## Why

The drivers already normalize every provider into one event stream, and 151
tests assert against hand-built streams. What none of them can catch is an
upstream CLI changing its wire shape: OpenCode 2.0's rename, Cursor's
parameterized model picker, Pi's slower handshake, and Claude's model naming
were all found by a user, not by CI.

The 15 tests that would catch those drive a real binary, so they carry
`#[ignore]` and never run. Recording what a real CLI sent turns one manual run
into a fixture every future run replays.

## What a fixture is

One JSON file per scenario, holding the provider's outbound messages in order.
It is the provider's side of the conversation only: no events Waku invented, no
timestamps from the recording machine, nothing that depends on who recorded it.

```jsonc
{
  "provider": "codex",
  "scenario": "fork-preserves-history",
  "recorded_with": "codex 0.52.0",
  "recorded_at": "2026-11-15",
  "transport": "codex-app-server",
  // The handshake replies a replay must serve, in request order. Requests are
  // matched by method and by the id Waku assigned, never by timing. A method
  // that appears twice in a session must be recorded twice — the replayer
  // consumes these in order, so a merged entry replays the wrong reply.
  "exchanges": [
    {
      "on_request": "initialize",
      "reply": { "id": 0, "result": { "userAgent": "codex/0.52.0" } }
    },
    {
      "on_request": "thread/start",
      "reply": { "id": 1, "result": { "thread": { "id": "th_1", "turns": [] } } }
    }
  ],
  // Everything the provider pushed unprompted, in order. These are the
  // provider's own wire shapes, not Waku's summary of them: Codex reads
  // `params.turn.status` and `params.turn.id`, so a fixture that writes
  // `params.status` records something the driver never parses.
  "events": [
    { "method": "turn/started", "params": { "turn": { "id": "turn_1" } } },
    { "method": "item/agentMessage/delta", "params": { "delta": "Hello" } },
    { "method": "turn/completed", "params": { "turn": { "status": "completed" } } }
  ]
}
```

Rules the format enforces:

- **Order is meaning.** The transcript, the activity rows and the turn settling
  all depend on event order, so a fixture is a list, never a set.
- **No wall-clock times.** Anything time-shaped is either omitted or replaced
  with a placeholder the replayer substitutes at run time. A fixture recorded in
  March must still pass in December.
- **No absolute paths from the recording host.** Session files, workspaces and
  binary paths are placeholders, because the replayer runs on a different
  machine.
- **Provider errors are recorded, not avoided.** A turn the provider rejected is
  the most valuable fixture there is — that is the Kimi shape, where the
  protocol reports a clean `end_turn` and the real reason lives elsewhere.

## Recording a rejected turn

A rejection goes in `events` as the provider's own `error` notification, in the
position it actually arrived. It keeps that position because the failure is
usually interleaved with a turn that otherwise looks successful:

```jsonc
"events": [
  { "method": "turn/started", "params": { "turn": { "id": "turn_2" } } },
  // The rejection itself. `params.message` is required and must be non-empty:
  // the replayer refuses to load an error event it cannot show a user.
  { "method": "error",
    "params": { "message": "You've hit your usage limit" } },
  // Note the status: the wire reports a completed turn even though it failed.
  // That is the whole reason this notification has to be recorded.
  { "method": "turn/completed",
    "params": { "turn": { "id": "turn_2", "status": "completed" } } }
]
```

The `error` method with `params.message` is the shape a JSON-RPC driver's parser
already recognizes — Codex's `handle_codex_message` maps it straight to
`DriverEvent::Error`, so a rejection replays into the user-visible error with no
special casing. `Fixture::rejections()` lists the recorded ones, which lets a
test assert that one reached the user rather than only that the stream ended.

Two boundaries, stated so a recorder does not assume otherwise:

- **ACP providers (Cursor, Fx, Grok, Kimi) do not use this shape.** They are
  driven through the Agent Client Protocol, where a failure arrives as a
  protocol-level error or a `session/update`, not as a JSON-RPC `error` method.
  Record those in whatever shape that transport actually sent; the replayer
  replays events verbatim and does not reinterpret them.
- **This records what the provider said, not what Waku concluded.** A provider
  that fails silently produces a fixture with no `error` event, and that
  silence is the correct recording — the test then asserts the driver's own
  fallback, which is the Kimi case this section is titled after.

## Where they live

```
crates/waku-core/src/driver/fixtures/
  codex/
    fork-preserves-history.json
    models-listing.json
  opencode/
    service-restart.json
  ...
```

A fixture sits beside the driver that consumes it. `codex_fork.sh`, the one
fixture the repo has today, is a shell-script harness rather than a recording;
it stays as it is until its scenario is ported.

## How replay works

Replay feeds the driver's parser through the same event channel its reader
thread writes to. It does **not** stand in for the process:

1. The replayer answers `on_request` entries by method, re-stamping each reply
   with the id the driver used, then drains `events` in recorded order.
2. The test hands those messages to the driver's message handler — for Codex
   that is `handle_codex_message`, a free function the reader thread calls. The
   handler runs for real; only its input is fabricated.
3. The test asserts the resulting `DriverEvent` stream, exactly as today's 151
   tests do.

`test_event_channel()` returns the driver's event **outlet**, not an inlet for
requests, so a driver's `start()` still spawns a real binary in any test that
goes through it. That is the honest boundary of this approach: it covers
parsing and event mapping, and it does not cover process startup, the writer
thread, or anything the driver reads from disk.

Closing that gap would mean giving `start()` a transport seam, which is a
change to how all eleven drivers are built rather than a test-only addition.
It is not part of this plan; scenarios that genuinely need a process stay
`#[ignore]`d and go on the release checklist.

## Recording

A recorder is an `#[ignore]`d test that drives the real CLI with a fixed prompt
and writes the fixture. It runs when a provider changes, on a machine that has
the CLI installed and authenticated.

Rules:

- A fixed prompt, so the recording is reproducible.
- Strip absolute paths, timestamps and version strings that would make the
  fixture machine-specific. Keep the ones that document what recorded it.
- The recorder must redact anything the user would not want committed. A Claude
  or Codex transcript can carry a fragment of the user's own repository.
- Review a new fixture by hand before committing it. It is a transcript of a
  real session, and a careless one leaks.

## Which to record first

Ordered by how recently each broke, which is the best available predictor:

1. **OpenCode** — 2.0 moved from a per-workspace server to a shared service and
   took over the `opencode` command.
2. **Cursor** — the parameterized `configOptions` picker replaced a fixed model
   list.
3. **Pi** — the 30 s handshake exists because a slow extension load failed a
   session that was merely slow to start.
4. **Claude** — model naming gained a version suffix, and the resume cursor
   depends on a transcript uuid.

Then the rest, as their scenarios come up.

## What a fixture cannot cover

- **A real handshake.** Whether the binary actually accepts the flags Waku
  passes. That stays a manual check on the release checklist.
- **Timing.** A provider that answers in five seconds and one that hangs differ
  only by a timeout a fixture cannot reproduce.
- **Provider-side state.** OpenCode's shared service, Claude's transcript on
  disk. Fixtures cover the wire, not the disk.

The `#[ignore]`d tests that cover these stay ignored. The goal is to move 12 of
the 29 provider-related ignored tests into CI — the 30th `#[ignore]` in the
tree is a Markdown math performance measurement in `src/md/math.rs` and has
nothing to do with a provider — not to pretend the other 17 do not matter.
