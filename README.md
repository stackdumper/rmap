# rmap

[![crates.io](https://img.shields.io/crates/v/rmap.svg)](https://crates.io/crates/rmap)
[![license](https://img.shields.io/crates/l/rmap.svg)](LICENSE)

Codebase map CLI. The first command to run on an unfamiliar repo. Compact, parsed, friendly to humans and LLM tooling.

> **Rust-aware today.** `tree` (no `--detail`) works on any repo. Item parsing, `refs`, `deps`, and `graph` are Rust-only — they use `syn` to read `.rs` files. Other languages render as bare file names.

Path enumeration is git-aware via `git ls-files` and respects `.gitignore`. Outside a git work tree, `rmap` falls back to a filesystem walk that skips `.git`, `target`, `node_modules`, `.DS_Store`, `tmp`, `dist`.

## Install

```sh
cargo install rmap
```

From source:

```sh
cargo install --git https://github.com/stackdumper/rmap
cargo install --path .                # local checkout
```

## Subcommands

| Command  | Purpose                                                          | Rust-only? |
|----------|------------------------------------------------------------------|------------|
| `tree`   | Brace map of files/dirs. `--detail` inlines parsed items.        | No (items: yes) |
| `module` | Focused index of a subtree or file, items always inlined.        | Items: yes |
| `refs`   | Find defs + uses of one or more identifiers.                     | Yes        |
| `deps`   | File-level dep graph from `use` and `mod` statements.            | Yes        |
| `graph`  | Reachability subgraph from an entry file (forward or reverse).   | Yes        |

## Quick start

Orient on an unfamiliar repo:

```sh
rmap tree --depth 2                # shape of the repo
rmap tree --detail                 # tree + Rust items per .rs file
rmap tree --detail --lines         # add :start-end ranges, :LOC per file
```

Drill into a subtree or file:

```sh
rmap module computer               # fuzzy: any unique path suffix
rmap module src/domain/hull        # exact directory
rmap module src/walk.rs            # single-file item index
```

Find an identifier:

```sh
rmap refs enumerate                # all defs + uses
rmap refs Filter --defs-only       # where defined?
rmap refs render_file --uses-only  # who calls?
```

Trace dependencies:

```sh
rmap deps                          # whole-repo forward graph
rmap deps --reverse                # who imports each file
rmap deps --ext                    # include external crate edges
rmap deps src/walk.rs              # focus: in/out/ext for one file
rmap graph                         # brace tree from crate root
rmap graph src/walk.rs --reverse   # who reaches walk.rs?
rmap graph --mermaid               # mermaid `graph TD` block
```

## Output samples

`rmap tree --detail`:

```
src {
  main.rs { struct Cli, enum Cmd, fn main, fn run_tree, fn run_module }
  parse.rs { struct ParseOptions, fn render_file, fn render_str, fn render_items }
  walk.rs {
    const SKIP_DIRS, enum Node { fn name }, struct Filter,
    fn enumerate, fn list_paths, fn git_ls_files
  }
}
```

`rmap tree --detail --lines`:

```
walk.rs:249 { const SKIP_DIRS:12-19, enum Node:21-32 { fn name:35-39 },
              struct Filter:42-50, fn enumerate:52-79, ... }
```

`rmap refs Filter`:

```
src/main.rs:396:24 use struct-lit Filter
src/refs.rs:28:25  use import     Filter
src/walk.rs:37:12  def struct     Filter
src/walk.rs:47:40  use type       Filter
```

`rmap refs Filtr` (typo → suggestion):

```
no hits for `Filtr`
did you mean: Filter, filter?
```

`rmap deps src/walk.rs`:

```
src/walk.rs
  out: -
  in:  src/refs.rs, src/render.rs
```

`rmap graph --depth 2`:

```
src/main { deps, graph { deps* }, parse, refs { walk }, render { parse*, walk* }, walk* }
```

Markers: `*` revisit, `~` back-edge, `{…}` depth limit hit.

## Non-Rust repos

`rmap` still works — with reduced fidelity:

- `tree` (no `--detail`) — full layout, language-agnostic.
- `tree --detail`, `module` — non-`.rs` files render as bare names.
- `refs`, `deps`, `graph` — Rust-only. Run on a Rust repo or scope with `--in`.

## Teach an AI agent

```sh
rmap agent >> CLAUDE.md            # or AGENTS.md, .cursorrules, ...
```

Prints a short markdown snippet telling Claude/Cursor/Codex/... to reach for `rmap` instead of `find`/`ls`/`tree` when locating code.

## Flags reference

Run `rmap --help` for the full surface. Highlights:

**`tree`**
- `--depth N`, `--detail`, `--lines`
- `--cap PREFIX=N` (repeatable, default `docs/sessions=10`), `--no-default-caps`
- `--exclude SUBSTR` (repeatable), `--ext rs,md`
- `--detail` implicitly caps depth at 4 unless `--depth` is given.

**`module`**
- `--depth N`, `--lines`, `--ext rs,md`
- Positional args: exact dir, exact file, or unique path suffix. Multiple args render in order, blank line between.

**`refs <NAME>...`**
- `--in PATH`, `--defs-only`, `--uses-only`
- Output: `file:line:col role kind name`. `role` ∈ `def|use`.
- `def` kinds: `fn, method, struct, enum, union, trait, const, static, type, macro`.
- `use` kinds: `call, method, type, struct-lit, path, macro, import, pat`.
- Matches by trailing path segment only — no name resolution. Scope with `--in PATH` to disambiguate.
- On zero hits, suggests similar identifiers (`did you mean: ...`) ranked by substring, snake/camelCase token overlap, longest common prefix, character-bigram Jaccard similarity, and edit distance. A signal gate suppresses short-edit-distance noise (e.g. `Bar` won't be suggested for `baner`).

**`deps [PATH]`**
- `--reverse`, `--ext`
- `PATH` selects scope: dir with `Cargo.toml` (whole repo), subdirectory (scoped subtree, repo auto-detected), or `.rs` file (focus mode: in/out/ext).
- Whole-repo / scoped output: `file -> dep1, dep2, ...`.
- Resolves `crate::`, `self::`, `super::` against the `mod` tree from `src/main.rs` or `src/lib.rs`. Single-crate only — workspaces not parsed.

**`graph [ENTRY...]`**
- `--reverse`, `--mermaid`, `--depth N`, `--ext`
- Zero or more entries (each renders its own subgraph; default = detected crate root).
- Default output: single-line brace tree. Reuses the `deps` graph and follows `mod x;` edges too.

## Conventions

- Output is plain text on stdout; errors go to stderr with non-zero exit.
- No colors, no panics on user input.
- Item parsing is Rust-only (via `syn`).

## License

MIT. See [LICENSE](LICENSE).
