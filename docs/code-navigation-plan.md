# Sidebar browsing and code navigation: the plan

Two asks: the sidebar's browsing needs to be stronger, and the file editor needs
the basics a reader of code expects — jump to a definition, find who references
it. This document says what exists, what is missing, and in what order to build
it. It is written against the code as it stands at `dabb370`, not against what
the documents claim.

The two asks look unrelated but share one constraint, and it decides most of the
design: **the desktop is an RPC client of a daemon that may be on another
machine.** Nothing in this plan reads the filesystem from the UI thread, and
nothing assumes the workspace is local.

## What exists today

### Sidebar

| Part | Where | State |
| --- | --- | --- |
| Grouping: Project / Updated | `src/app/sidebar.rs:92` | shipped, two values |
| Ordering: Newest / Oldest | `src/app/sidebar.rs:99` | shipped, two values |
| Collapse a group | `sidebar.rs:1657` | shipped, in-memory only |
| Older sessions behind "Show more" | `sidebar.rs:1650` | shipped, batches of 30, in-memory |
| Row virtualization | `sidebar.rs:1049` | shipped (`list()`) |
| Search row | `sidebar.rs:776` | opens the window command palette, not a sidebar filter |
| Session row keyboard | `sidebar.rs:1996` | `enter`/`space` selects, `shift+f10` opens its menu |

Persistence (`crates/waku-client/src/persistence.rs:368-371`) stores
`sidebar_grouping` and `sidebar_ordering` only. **Collapsed groups and revealed
batches are lost on restart.**

### Files and the editor

| Part | Where | State |
| --- | --- | --- |
| Working tree | `right_panel.rs:2722` | mouse-only, no keyboard, virtualized by hand |
| Open a file | `right_panel.rs:2015` | shipped |
| Read a file | `right_panel.rs:3047` | background pass, epoch-guarded |
| Find and replace | `src/app/file_search.rs` | shipped, one file at a time |
| Gutter line numbers | `right_panel.rs:3179` | painted by a canvas, **not clickable** |
| Project file list | `waku-core/src/composer_complete.rs` | `git ls-files` with a walk fallback, cap 50 000 |
| Cross-workspace text search | — | **does not exist** |
| Symbol index, go to definition, find references | — | **does not exist** |
| LSP, tree-sitter, or any parser | — | **no dependency for any of them** |

Two facts worth naming now because they set the scope:

1. **There is no language-aware infrastructure at all.** The only code
   understanding in the tree is `src/md/highlight.rs`, a single-pass line lexer
   with no syntax tree, and it does not even emit plain identifiers as tokens
   (see C3). "Jump to definition" therefore either brings a parser in or is
   built on pattern matching over text.
2. **A file's location suffix is already parsed and then thrown away.** Codex
   emits `path:line` and `path#LlineCcolumn` links; `right_panel.rs:46` strips
   that location and a comment at `:43-45` says it "is not yet consumed by
   Waku's compact editor". Clicking such a link opens the file at line 1. That
   is a bug in the current build, not a missing feature.

## The constraint that shapes everything

`Command::Workspace` (`crates/waku-core/src/daemon.rs:723`) runs
`crate::workspace::execute` **synchronously on the daemon**. Adding a
workspace-wide search or a symbol scan as a new `WorkspaceOperation` would block
every other request — session updates included — for as long as the scan runs.
That is acceptable for `ReadTextFile` and not acceptable for anything that reads
thousands of files.

So the plan puts search in two places by design:

- **The daemon owns the index.** It is the only party that can see the
  filesystem, it knows the workspace root, and it survives client restarts.
- **The client owns the query box and the results UI**, and asks the daemon for
  results asynchronously, debounced, through a path that does not stall the
  request loop.

## What the workspace browsing should become

Ordered by how much each item actually costs the user today.

### B1. Keyboard navigation in the sidebar (no protocol change)

The sidebar is a `list()` of focusable rows, so `↑`/`↓` currently move focus
between rows but every row needs its own tab stop.

What already works, and must not be re-described as missing: a **group header**
handles `←`/`→` to collapse and expand itself (`sidebar.rs:1555-1571`) and
`enter`/`space` to toggle. That is the right behaviour and it stays.

What is actually missing is movement *between* rows. What a reader expects
instead is a single tab stop on the list with arrow keys moving the selection:

- `↑`/`↓` move to the previous/next **selectable** row, skipping spacers, and
  clamp at the ends rather than wrapping.
- `←`/`→` keep their existing meaning on a header, and on a session row they
  move to its group header — so the collapse keys are reachable from a row
  without a detour.
- `home`/`end` jump to the first/last selectable row.
- The list keeps `tab_index(0)` on the rows for tab users, but the arrow keys are
  handled at the list level through a `FocusHandle` on the scroll container.

Cost: one file, no protocol, no persistence. This is the cheapest real win and
it is a prerequisite for B2 feeling right.

### B2. Persist what the user arranged (no protocol change)

- `sidebar_collapsed_groups` and `sidebar_project_reveal_counts` join
  `AppSettings` next to `sidebar_grouping`. Both are small and both are the kind
  of state a user is annoyed to lose.
- Stored as stable keys, not indices: `project:<uuid>` and `updated:<n>`, so a
  new project appearing does not shift what is collapsed.

Cost: one persistence field pair in each of the two settings structs
(`waku-client` and `waku-core`, which keep separate copies) plus the legacy
default. Note the `#[serde(default)]` trap recorded during the theme work: an
added field must not silently inherit a default that its absence cannot be told
apart from.

### B3. A filter, not a search (no protocol change)

The sidebar's "Search" row opens the window palette, which searches every
session's messages. That is the right thing for "where did I discuss X" and the
wrong thing for "show me only the Acme project". Add a local filter:

- Typing in the search row narrows the **already loaded** session list by title,
  project name and branch label, without leaving the sidebar.
- `escape` clears the filter; the row keeps working as a palette launcher when
  the query is empty.
- Filtering happens over `state.sessions`, not per row: `sidebar_rows` gains a
  filter stage before grouping, and the viewport keeps its position.

Cost: `sidebar.rs` only. It must not turn into a filesystem or message search —
that is what B4 and the palette are for.

### B4. Session activity: which task needs me (needs one protocol field)

`AgentSession` (`waku-protocol/src/model.rs:973`) carries `status`, but the
sidebar has no way to show a task that has been sitting in `Waiting` for an hour
apart from one that just changed. Two cheap additions:

- Sort key: a "needs attention" tier above recency, opt-in through the existing
  ordering menu (Newest / Oldest / Needs attention).
- The row already renders `hourglass`, `alert`, `x` icons (`sidebar.rs:1932-1951`);
  what is missing is the elapsed time on a waiting row, which the row already
  computes for its timestamp.

Whether "needs attention" needs a protocol field at all depends on one question:
can `status` alone express it? It can for `Waiting` and `Failed`. It cannot for
"waiting for longer than N", which is a client-side timer over `updated_at`. So:
**client-side only, no protocol change**, unless the user wants the notion to
travel to web and mobile, in which case it becomes a `SessionStatus` question and
a three-client release.

### B5. Drag to reorder projects (deferred, deliberately)

Manual project ordering is the one item here that needs a persisted ordering key
per project and a protocol field to carry it, because project order is shared
state in the daemon's view. It is listed as deferred: it is the only browsing
feature that cannot be built without touching the wire contract, and the value
is lower than B1–B4.

## What code navigation should become

### C1. Consume the location that is already parsed (no protocol change)

Before anything new: `open_right_panel_file` takes a path and the location is
dropped at `right_panel.rs:46`. Split it into `open_right_panel_file_at(path,
line, column)`:

- The existing editor entity gets `select_range` over the line's byte range and
  the scroll handle reveals it — the same two calls `file_search.rs:500-508`
  already makes, so there is a working precedent.
- Every `path:line` and `#LlineCcolumn` link in the transcript goes through it.

This is small, it fixes something that is currently wrong, and the mechanism C3
and C4 need is exactly this one.

### C2. Gutter: clickable line numbers and a line selection

- Clicking a gutter number selects that line; `shift+click` extends.
- The gutter is a `canvas` today (`right_panel.rs:3179`), and a canvas cannot be
  clicked per number. Either the gutter becomes a column of real elements (one
  per visible line, cheap only because the editor already knows which lines are
  visible) or the canvas gains a hit-test against the same `heights` vector it
  paints from. **Prefer the hit-test**: it keeps one paint path and adds no
  elements, and the canvas already receives the same `bounds` it would need.
- Value: `path:line` links in transcripts become copyable and shareable, which is
  what people actually use line numbers for.

### C3. Go to definition — the honest options

There is no parser, so there are three real routes and one fake one.

| Route | What it buys | What it costs | Verdict |
| --- | --- | --- | --- |
| **A. LSP client** | True definitions, references, rename, diagnostics; works for every language a server exists for | A server per language the user has installed; process lifecycle, JSON-RPC, capability negotiation; and the daemon is the side that must host it because only it sees the files | **Right long term, wrong first step.** It is a subsystem, not a feature |
| **B. In-house indexer** | Definitions and references for the languages Waku already lexes (`md/highlight.rs` `Lang`), no process to manage | A real declaration parser per language, and the lexer is a line scanner with no tree — this is writing a mini tree-sitter | Viable for a small, named language set only |
| **C. Pattern scan** | "Jump to the definition of this identifier" for the common declaration shapes | Everything else, and every false positive is visible | **The right first step.** Ships value now, honest about its limits |
| **D. A shell-out to `rg`/`git grep`** | Nothing a pattern scan cannot do | A process per query on a host that may not have the tool | Rejected |

The recommendation is **C now, A later, and B only if a specific language needs
it**. Concretely for C:

- Index is built **on the daemon**, in a background thread, over the file list
  `list_project_files` already produces (git-listed when available).
- It stores, per file: identifier name → byte ranges of the lines matching a
  declaration pattern for its extension. Pure text, no AST.
- Declaration patterns are a small table keyed by extension, not a language
  framework: `fn|struct|enum|trait|impl|const|static|mod|type` for Rust,
  `function|class|const|let|var` for JS/TS, `def|class` for Python, and so on.
  Keeping it a table is what makes the false-positive rate predictable and the
  scope reviewable.
- The identifier under the cursor is resolved by **a small word scanner over the
  line**, not by the lexer. This is a correction to an earlier draft of this
  plan: `highlight::tokenize_line` *does not* emit plain identifiers. At
  `md/highlight.rs:963-969` an identifier that is not a keyword, literal, known
  type, call, or capitalized type produces `None` and is pushed to no token list
  at all. So the lexer cannot answer "what word is under the cursor", and
  assuming it could would have produced an empty result for every ordinary
  variable name.
  What the lexer *is* good for is telling us whether the cursor sits in a
  comment or a string — span lookup over the same `tokenize_line` output — which
  is exactly the check that keeps go-to-definition from firing inside a comment.
- Results: exactly one hit jumps; several open a picker in the right panel. The
  picker is the same component the command palette uses for results.

Limits to state in the UI, not hide: a pattern scan finds declarations it has a
pattern for, cannot follow a trait method to its implementations, and will pick
the wrong `impl` for a repeated method name. The picker makes that visible
because it shows every candidate with its file and line.

### C4. Find references

Same index, opposite query: every occurrence of the identifier as a whole word
across the indexed file set, grouped by file with counts, opened in the right
panel's existing list. This is `rg` semantics built in Rust over the same
`regex` dependency `file_search.rs` already uses, run once per query on the
daemon, and capped with an honest "+N more".

The distinction that has to survive into the UI: **references are not
definitions.** A text occurrence includes comments, strings, and unrelated
same-named identifiers. Grouping by file, with the line text shown, is what makes
that legible rather than misleading.

### C5. Workspace text search (a find bar that is not per-file)

`file_search.rs` searches one open file. There is no way to ask "where is this
string in the project". This is the third query over the same index, and the one
most people will use daily:

- `secondary-shift-f` opens a workspace search bar in the right panel. That
  combination is free: `src/lib.rs:297` binds only `secondary-alt-shift-f` (the
  FPS counter, excluded from the reference) and `secondary-f` is per-file find.
  Whatever lands here must also be added to `src/app/shortcuts.rs`, whose drift
  tests read `lib.rs` in both directions and will fail a binding that is
  undocumented.
- Results grouped by file, each line clickable into the editor at that line —
  which needs C1, so C1 comes first.
- Case/whole-word/regex toggles reuse `file_search.rs`'s existing flag model.

### C6. Symbol outline for the open file

With C3's index already built, the per-file outline (functions, types, members in
order) is a filtered view of the same data. It belongs in the right panel, above
or beside the working tree, and it is what makes navigation work on a file too
big to scroll.

## Order of work

Each stage is independently shippable and independently testable.

| Stage | Content | Wire change | Why this order |
| --- | --- | --- | --- |
| 1 | C1 consume the parsed location | none | Fixes a live bug; builds the reveal primitive everything else needs |
| 2 | B1 keyboard navigation, C2 clickable gutter | none | Pure `src/app`, cheap, immediately felt |
| 3 | B2 persistence, B3 filter | none | Pure client, removes daily annoyances |
| 4 | The daemon-side index + C3 go to definition | new `WorkspaceOperation` | First thing that needs the daemon to work; see the concurrency rule below |
| 5 | C4 find references, C5 workspace search | reuses stage 4 | Same index, same request shape |
| 6 | C6 outline, B4 attention ordering | none (client-side) | Presentation over data that now exists |
| 7 | LSP (route A) — only if stage 4's limits prove real | new op, process hosting | Deliberately last |

## The concurrency rule for stage 4

The indexer must not run on the request loop. Concretely:

- A dedicated worker thread in the daemon owns the index, guarded by a
  `parking_lot::Mutex` that the **request path never takes**: requests ask for a
  snapshot (`Arc<Index>`) and answer from it.
- Refresh is incremental and triggered by a cheap signal — file count and
  modification times, not content hashes — debounced the way
  `command_palette.rs:628-644` debounces message search.
- A query that arrives while the first index is still building gets an explicit
  "indexing" state, not an empty result. An empty result must mean "not found".
- Every request carries a generation number, so a snapshot from a superseded
  build cannot answer a newer query. This is the same epoch pattern
  `read_right_panel_file_into_editor` already uses (`right_panel.rs:3073`).

## What this plan deliberately refuses

- **A full LSP for every language as step one.** It is the correct end state and
  the wrong first step: it is a process supervisor, a JSON-RPC client, and a
  capability negotiator, and every one of those needs the daemon to be the host
  before any of it is useful.
- **Background indexing inside the render path.** `docs/performance.md` and
  `AGENTS.md` forbid I/O from a frame; there are existing tests
  (`right_panel.rs:1383`, `:1412`) that assert the render paths do zero
  filesystem work. Anything here keeps that property.
- **A search that silently truncates.** Every capped query reports its cap.
- **Pretending text search is symbol search.** The UI says "matches", not
  "references", wherever the answer came from a pattern.

## Verification

Per stage, the same bar the last four rounds held:

- Unit tests for every pure piece: the identifier-under-cursor scanner, the
  line-range computation for C1, the filter predicate for B3, the declaration
  table for C3.
- **A destructive edit for each new test that could pass vacuously.** The
  incremental-cache trap recorded in earlier work means a "passing" test after a
  restore is re-run after `cargo clean -p <crate>` before it is trusted.
- The indexer's concurrency guard gets a test that a stale generation cannot
  answer.
- `clippy -D warnings`, `fmt`, `i18n:check`, `protocol:check` per stage.
- Any new `WorkspaceOperation` variant bumps `PROTOCOL_VERSION` and regenerates
  the TS types (`bun run protocol:generate`), because a daemon without the
  operation would otherwise answer with an error the client mistakes for "no
  results".
