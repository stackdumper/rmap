# rmap

Codebase map CLI. Two lenses: `tree`, `module`. Rust-aware (parses items via `syn`). Git-aware enumeration; falls back to fs walk outside repo.

## Use rmap to explore rmap

Eat own dogfood. Before reading files, orient with:

```
rmap tree --depth 2            # shape of repo
rmap tree --detail --lines     # items + per-symbol line ranges
rmap module src                # focused index
rmap module src/parse.rs       # items in one file
```

Don't `find`/`ls`/`tree` for layout — `rmap` is faster, parsed, and the thing under test.

## Build / install

- `cargo build --release` — local build.
- `cargo install --path .` — install to `~/.cargo/bin/rmap`. Re-run after edits.
- `cargo run -- <args>` — iterate without install.

## Layout

- `src/main.rs` — clap CLI, subcommand dispatch.
- `src/walk.rs` — git-aware path enumeration + filtering.
- `src/parse.rs` — syn-based Rust item extraction.
- `src/render.rs` — tree / module output.
- `src/deps.rs` — file-level dep graph (`use` + `mod` resolution).
- `src/graph.rs` — reachability subgraph (brace + mermaid).
- `src/refs.rs` — identifier search (defs + uses via syn visitor).

## Conventions

- No project-specific assumptions — tool runs anywhere.
- Keep deps minimal: `clap`, `clap_complete`, `syn`, `proc-macro2`. Justify additions.
- Output is plain text on stdout. No colors unless explicitly added with opt-in flag.
- Default caps (e.g. `docs/sessions=10`) live in `main.rs` constants.

## Rules

- Don't add language-specific parsing beyond Rust without subcommand gate.
- Don't break existing flag names; add new flags rather than rename.
- Errors → stderr, non-zero exit. No panics on user input.
