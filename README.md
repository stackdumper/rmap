# rmap

Codebase map CLI. Three lenses on a repo:

- **`tree`** — brace map of files and directories, optionally inlining parsed Rust items.
- **`module`** — focused index of one subtree (or one file), Rust items always inlined.
- **`refs`** — find references to a Rust identifier (defs + uses) across the repo.
- **`deps`** — file-level dependency graph from `use` and `mod` statements.
- **`graph`** — reachability subgraph from an entry file (forward or reverse). Compact brace tree by default; `--mermaid` for diagram.

Path enumeration is git-aware via `git ls-files` and respects `.gitignore`. Outside a git work tree, `rmap` falls back to a filesystem walk that skips `.git`, `target`, `node_modules`, `.DS_Store`, `tmp`, `dist`.

`rmap` is designed to be the first command you run on an unfamiliar repo, and to be friendly to both humans and LLM tooling that needs a compact, parsed view of a codebase.

## Install

```sh
cargo install --git https://github.com/stackdumper/rmap
```

From a local checkout:

```sh
cargo install --path .
```

## Quick start

```sh
rmap tree --depth 2                # shape of the repo
rmap tree --detail                 # tree + Rust items per .rs file
rmap tree --detail --lines         # add :start-end ranges per symbol, :LOC per file
rmap module computer               # fuzzy: resolves any unique path suffix
rmap module src/domain/hull        # exact directory path
rmap module src/walk.rs            # single-file item index
rmap refs enumerate                # all defs + uses of `enumerate`
rmap refs Filter --defs-only       # where is `Filter` defined?
rmap refs render_file --uses-only  # who calls `render_file`?
rmap deps                          # whole-repo dependency graph
rmap deps --reverse                # who imports each file
rmap deps --ext                    # include external crate edges
rmap deps src/walk.rs              # focus: in/out/ext for one file
rmap graph                         # brace tree from crate root
rmap graph src/walk.rs --reverse   # who reaches walk.rs?
rmap graph --depth 2               # depth-bounded
rmap graph --mermaid               # mermaid `graph TD` block
```

## Output

`tree --detail`:

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

`tree --detail --lines`:

```
walk.rs:249 { const SKIP_DIRS:12-19, enum Node:21-32 { fn name:35-39 },
              struct Filter:42-50, fn enumerate:52-79, ... }
```

`module src/walk.rs --lines`:

```
walk.rs:249 { const SKIP_DIRS:12-19, enum Node:21-32 { fn name:35-39 }, ... }
```

## Teach an AI agent to use rmap

```sh
rmap agent >> CLAUDE.md        # or AGENTS.md, .cursorrules, etc.
```

Prints a short markdown snippet telling Claude/Cursor/Codex/... to reach for `rmap` instead of `find`/`ls`/`tree` when locating code.

## Subcommands and flags

Run `rmap --help` for the full surface, including a recommended workflow and per-subcommand examples. Highlights:

- `tree`: `--depth N`, `--detail`, `--lines`, `--cap PREFIX=N` (repeatable, default `docs/sessions=10`), `--no-default-caps`, `--exclude SUBSTR` (repeatable), `--ext rs,md`.
- `module`: `--depth N`, `--lines`, `--ext rs,md`. Accepts one or more positional args, each an exact directory path, an exact file path, or a unique path suffix. Multiple args render in order with a blank line between.
- `refs <NAME>...`: one or more identifiers. `--in PATH` to scope the search, `--defs-only`, `--uses-only`. Output `file:line:col role kind name` per hit; multiple names render in groups with a blank line between. `role` ∈ `def|use`. `def` kinds: `fn, method, struct, enum, union, trait, const, static, type, macro`. `use` kinds: `call, method, type, struct-lit, path, macro, import, pat`. Matches by trailing path segment only — no name resolution. Scope with `PATH` to disambiguate.
- `deps [PATH]`: `--reverse`, `--ext`. PATH selects what to render:
  dir with `Cargo.toml` (whole-repo), subdirectory (scoped subtree, repo
  auto-detected by walking up), or `.rs` file (focus mode: in/out/ext
  breakdown). Whole-repo / scope output is one line per file
  (`file -> dep1, dep2, ...`). Resolves `crate::`, `self::`, `super::`
  against the `mod` tree from `src/main.rs` or `src/lib.rs`. Single-crate
  only (workspaces not parsed).
- `graph [ENTRY...]`: zero or more entries (each renders its own subgraph; default = detected crate root). `--reverse`, `--mermaid`, `--depth N`, `--ext`. Reachability subgraph from `ENTRY` (default: detected crate root). Default output is a single-line brace tree (`main { deps, refs { walk }, ... }`). Markers: `*` = revisit, `~` = back-edge, `{…}` = depth limit hit. Reuses the `deps` graph and follows `mod x;` edges too.

`--detail` implicitly caps depth at 4 unless `--depth` is given, to prevent wall-of-text on large trees.

## Conventions

- Output is plain text on stdout; errors go to stderr with non-zero exit.
- No colors, no panics on user input.
- Item parsing is Rust-only (via `syn`). Other languages render as bare file names.

## License

MIT. See [LICENSE](LICENSE).
