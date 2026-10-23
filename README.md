# aiusage

Local coding-agent usage and estimated cost reports, powered by Rust.
Forked from [NazmusSayad/mytokens](https://github.com/NazmusSayad/mytokens).

## Run

Once published:

```sh
npx aiusage
```

Or install globally with `npm install -g aiusage`.
Node.js 22.13+ is required. npm installs only the matching parser binary from
`@shanthropic`; users do not need Rust or a compiler. PNG/image support also
installs the matching `@resvg/resvg-js` binary.

Supported platforms: Windows x64 (MSVC), macOS Intel/Apple Silicon, and Linux
x64/ARM64 with glibc 2.35+. Alpine/musl, Windows ARM64, and 32-bit systems are
not supported. CI validates on the documented GitHub-hosted runners; older OS
compatibility is not guaranteed.

## Usage

```sh
aiusage --help
aiusage --last 30
aiusage --screen models-by-tokens --month
aiusage export --format png --output overview.png
aiusage cache --help
```

Reads local history from 21 coding-agent sources, including Claude Code, Codex,
and OpenCode. Model pricing comes from [models.dev](https://models.dev).
Reports are estimates, not billing statements. Network access is needed to
refresh pricing and model groups.

Configuration and caches remain in `~/.mytokens` for compatibility with the
original CLI. `--fresh` bypasses the parse cache; `--no-cache` refreshes remote
data. No history-directory migration is performed.

## Development and releases

Rust parsers and the bundled SQLite cache live in `crates/core`. The adapter in
`src/native.ts` loads NAPI-generated bindings from `native/`; JS output lives
in `dist/`. Toolchain versions and dependency locks are committed.

All builds, existing tests, package validation, and size measurements run in
GitHub Actions. See [RELEASING.md](RELEASING.md) for bootstrap, OIDC, and recovery.

Branch flow is `main` → `rust-core` → `rust`: generic Rust work stays on
`rust-core`, fork branding and npm publication stay on `rust`.

Publication is blocked until redistribution permission and license notices
are supplied. No license is inferred from the upstream repository.
