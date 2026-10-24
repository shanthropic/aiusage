# Benchmark: `tokenstat` vs `mytokens`

Local head-to-head on this machine's real history. Not run in CI: the numbers
are only meaningful on the machine and data they came from.

```sh
pnpm bench                 # 3 workloads, 7 rounds each, ~10 minutes
pnpm bench --self-check    # prewarm + correctness gate only
```

Reports land in `bench/results/<timestamp>.json` and `.md` (gitignored) and a
markdown table is printed.

## What runs

| workload | command |
|---|---|
| `help` | `npx --yes --offline <pkg> --help` |
| `default` | `npx --yes --offline <pkg> --fresh` |
| `all` | `npx --yes --offline <pkg> --fresh --all` |

Pinned variants: `tokenstat@0.1.1` (Rust parsers) and `mytokens@1.3.4`
(TypeScript version of the same codebase, the fork point).

`--fresh` disables the parse cache, so each run parses the history from scratch.
That is the work being compared. `--no-cache` is never used: it refetches
pricing/model data from the network, which adds noise and is not parser work.

## Method

1. **Prewarm, once per variant, online:** installs the package and fills the
   shared `~/.mytokens` remote-data cache. Every measured run afterwards uses
   `npx --offline`, so no network is involved.
2. **Correctness gate:** both variants render the default report; output is
   normalized (ANSI stripped, whitespace collapsed, `tokenstat`/`mytokens`
   replaced by `CLI`) and compared. Humanized token magnitudes are compared at
   two significant digits because the Rust port prints slightly different
   precision (`26.9M` vs `27M`) on the same data. If the reports differ, the
   benchmark aborts and writes both outputs to `bench/results/`. Comparing
   different work is meaningless.
3. **Rounds:** 1 discarded warmup + 7 measured per workload. A/B order
   alternates each round so machine drift does not favor one variant.
4. **Wall time** is measured around the child process inside
   `bench/measure.ps1`; it includes `npx`/npm startup, as a user feels it.
   PowerShell's own startup is excluded.
5. **Memory** is sampled every 20 ms from `Get-Process` for every `node`
   process started by that run (npm/npx plus the CLI):
   - `nodeSumPeakMB` — highest combined working set (both processes at once),
   - `nodeMaxPeakMB` — highest single process, usually the CLI with the native
     addon loaded.

   Windows has no reliable peak-RSS counter, so this is a sampled maximum. It
   is the same method for both variants; compare the two columns to each other,
   not to Task Manager. Sampling costs a little CPU on the same machine, which
   affects both variants equally.
6. **Data fingerprint:** file count, total bytes and a SHA-256 over
   path+size+mtime of every file in the 21 source directories the CLIs read.
   It is computed before and after the run. Results are comparable only between
   runs with the same fingerprint; the report prints deltas against the
   previous run only when the fingerprint matches.

## Reading the result

- `mytokens/tokenstat` ratio on time > 1 means tokenstat is faster.
- Ratio on memory > 1 means tokenstat uses less memory.
- The `help` workload is the startup floor: Node, npx and CLI module loading
  without parsing. The difference between `default` and `help` is roughly the
  parsing work.
- A variant that fails a workload (for example `mytokens --all` exceeding the
  V8 heap limit on a large history) is reported as `failed (exit N)` instead of
  a time, and the other variant is still timed. No ratio is shown for that row.
- Medians are the headline. On a desktop with background load, treat
  differences under ~5 % as noise; `min` and `max` are printed to show spread.

## History changes

Running the benchmark while your agents are active changes the fingerprint and
marks the run unstable. If you start it from inside one of the agents it
measures, that agent keeps writing its own history, so expect the run to be
flagged. Re-run from an idle machine for a comparable result. The benchmark
itself writes only the remote-data cache (once, during prewarm); with `--fresh`
neither CLI reads or writes the parse cache.
