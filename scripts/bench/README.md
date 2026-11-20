# Search-cost probes

Three standalone Rust programs that answer "what does searching a workspace
actually cost on this machine". They exist because `docs/code-navigation-plan.md`
originally claimed incremental refreshing was cheap, and measuring it showed the
claim was wrong. The numbers in that document come from these probes.

They are deliberately **not** part of the workspace, not in `[workspace.members]`,
and have no dependencies: compile one with `rustc -O` and run it. A benchmark that
needs a dependency build is a benchmark nobody re-runs.

```powershell
rustc -O -o search.exe scripts/bench/search_bench.rs
.\search.exe "D:\Projects\some-repo" 8
```

| Probe | Question |
| --- | --- |
| `search_bench.rs` | How long does a full read-and-scan take, and does threading help? |
| `refresh_bench.rs` | After an index exists, what does one freshness tick cost? |
| `query_bench.rs` | With no index at all, how long does one identifier query take? |

## What they measure, precisely

Each probe walks the tree itself, skipping `.git`, `target`, `node_modules` and
`dist`, and reads only text-shaped files by extension. That extension list is the
one thing to keep current: an earlier run of `search_bench` silently skipped
`.cpp`, `.ui` and `.lua` and reported 1 848 files for a tree that has 4 054, which
made every number it printed wrong. **Check the reported file count against the
tree before trusting a timing.**

`refresh_bench` also carries a warning worth keeping: its first version compared
file lists with `Vec::contains`, which is O(n²), and reported a 18.5-second tick
that was entirely its own fault. It now uses set membership and reports ~1.2 s on
the same tree. If a number here looks catastrophic, suspect the probe first.

## Measured results

Machine: Windows, 32 logical cores, SSD. Trees: Waku itself (624 text files,
8.7 MiB), a C++/Qt client at `up5client\Client\Platform` (4 054 files, 43 MiB),
and the whole `up5client\Client` tree (19 202 files, 211 MiB).

| What | 43 MiB | 211 MiB |
| --- | --- | --- |
| Walk | 17–37 ms | 179–346 ms |
| Read every file, 1 thread, cold | 595 ms | 6 083 ms |
| Read every file, 1 thread, warm | 147–156 ms | 1 872–1 894 ms |
| Read + 5 whole-word scans, 1 thread | 463 ms | 5 822 ms |
| Read + 5 whole-word scans, 8 threads | **95 ms** | **860 ms** |
| Stat every file (freshness poll) | 112–118 ms | **496–990 ms** |
| Stat + walk (a real freshness tick) | 537–545 ms | **1 209–1 276 ms** |
| One identifier query, no index, 8 threads | ~105–175 ms | ~400–520 ms |

For reference, `rg` on the 211 MiB tree took 2.3–3.1 s, and `git status` took
2.9 s on the 43 MiB tree — both slower than the naive probe, which is why neither
is the escape hatch it looks like.
