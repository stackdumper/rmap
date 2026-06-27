//! `rmap` — codebase map CLI.
//!
//! Six lenses on the repo, each with progressive disclosure:
//!   - `tree`    Brace map of files and directories.
//!   - `module`  Focused index of one subtree, with parsed Rust items.
//!   - `refs`    Definitions + uses of a Rust identifier.
//!   - `deps`    File-level dependency graph.
//!   - `graph`   Reachability subgraph from an entry file.
//!   - `body`    Full source body of a Rust item by name.
//!
//! The global `--crate <name|name@version>` flag rebases any lens onto a
//! vendored crate's source in the local cargo registry (see `krate`).
//!
//! `rmap --help` for top-level usage; `rmap <subcommand> --help` for
//! per-subcommand flags.

mod body;
mod deps;
mod graph;
mod krate;
mod parse;
mod refs;
mod render;
mod walk;

use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, CommandFactory, Parser, Subcommand};
use clap_complete::Shell;

const DEFAULT_SESSIONS_CAP: (&str, usize) = ("docs/sessions", 10);
/// Soft depth cap when `--detail` is set without an explicit `--depth`.
/// Prevents wall-of-text on large trees while still surfacing the structure.
const DETAIL_DEFAULT_DEPTH: usize = 4;

const ROOT_AFTER_HELP: &str = "\
RECOMMENDED WORKFLOW (orient an unfamiliar repo):
  1. rmap tree --depth 2                  # shape of repo, no item detail
  2. rmap tree --detail                   # full Rust item map
  3. rmap module <suffix>                 # drill into one subtree
  4. rmap module path/to/file.rs          # items in one file
  5. rmap refs <Name>                     # find defs + uses of a symbol
  6. rmap deps                            # file-level dep graph (architecture)
  7. rmap graph [ENTRY]                   # reachability subgraph from entry
  8. rmap body <Name>                     # print full source body of a symbol

EXAMPLES:
  rmap tree --depth 2
  rmap tree --detail --lines              # items + line ranges per symbol
  rmap module computer                    # fuzzy: resolves unique suffix
  rmap module src/domain/hull             # exact path
  rmap module src/walk.rs                 # single-file item index
  rmap refs enumerate                     # all defs + uses
  rmap refs Filter --defs-only            # just where it's defined
  rmap refs render_file --uses-only       # just who calls it
  rmap deps                               # who imports who
  rmap deps src/walk.rs                   # focused: in/out/ext for one file
  rmap graph                              # forward tree from crate root
  rmap graph src/walk.rs --reverse        # who reaches walk.rs?
  rmap body run_refs                      # print fn body verbatim
  rmap body Foo::bar                      # impl method body

EXPLORE A DEPENDENCY (--crate, global flag on every lens):
  rmap tree --crate serde --depth 2       # shape of a vendored crate
  rmap module --crate tokio runtime       # focused index inside a crate
  rmap body Deserialize --crate serde     # read a symbol's source
  rmap refs spawn --crate tokio           # find defs/uses in a crate
  rmap body Foo --crate serde@1.0.210     # pin an exact version

NOTES FOR TOOLING / LLMS:
  - `module` accepts a unique path suffix; on ambiguity it lists candidates and exits non-zero.
  - `--detail` implies `--depth 4` if no `--depth` is given (prevents wall-of-text).
  - `--lines` adds `:start-end` to every parsed symbol and `:LOC` to files.
  - `--crate SPEC` rebases all paths onto a crate's source in the local cargo
    registry (offline; must be `cargo fetch`ed). SPEC is `name` or `name@version`;
    bare name uses the nearest Cargo.lock pin, else the highest vendored version.
  - Output is plain text on stdout; errors go to stderr with non-zero exit.
";

#[derive(Parser)]
#[command(
    name = "rmap",
    version,
    about = "Codebase map CLI — two lenses for planning and implementation.",
    long_about = "rmap — codebase map CLI.\n\n\
        Configurable lenses on the repo, designed so nothing slips through the cracks:\n\n  \
        tree    Brace map of files and directories. Optional Rust item detail.\n  \
        module  Focused index of one subtree (Rust items always inlined).\n  \
        refs    Find references to a Rust identifier (defs + uses).\n  \
        deps    File-level dep graph from `use` and `mod` statements.\n  \
        graph   Reachability subgraph from an entry file (forward / reverse).\n  \
        body    Print the full source body of a Rust item.\n\n\
        Path enumeration is git-aware: respects .gitignore via `git ls-files`. \
        Outside a git repo, walks the fs and skips .git/target/node_modules/.DS_Store/tmp/dist.",
    arg_required_else_help = true,
    after_long_help = ROOT_AFTER_HELP,
)]
struct Cli {
    /// Resolve all paths against an external crate's source in the local cargo
    /// registry instead of the current directory. SPEC is `name` or
    /// `name@version` (e.g. `serde`, `serde@1.0.210`). Bare name uses the
    /// version pinned by the nearest Cargo.lock, else the highest vendored.
    /// OFFLINE: the crate must already be fetched (`cargo fetch`/`build`).
    /// Positional paths and `--in` become relative to the crate root.
    #[arg(long = "crate", value_name = "SPEC", global = true)]
    krate: Option<String>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Brace map of files and directories.
    Tree(TreeArgs),
    /// Focused index of one subtree (Rust items inlined).
    Module(ModuleArgs),
    /// Find references to a Rust identifier (defs + uses).
    Refs(RefsArgs),
    /// File-level dependency graph from `use` and `mod` statements.
    Deps(DepsArgs),
    /// Reachability subgraph from an entry file (forward or reverse).
    Graph(GraphArgs),
    /// Print the full source body of a Rust item by name.
    Body(BodyArgs),
    /// Print a markdown snippet teaching AI agents (Claude/Cursor/Codex/...) to
    /// reach for `rmap` instead of `find`/`ls`/`tree`. Paste into CLAUDE.md,
    /// AGENTS.md, .cursorrules, or any system-prompt file your agent reads.
    Agent,
    /// Emit a shell completion script to stdout (bash, zsh, fish, ...).
    Completions {
        #[arg(value_enum)]
        shell: Shell,
    },
}

#[derive(Args)]
#[command(after_long_help = TREE_AFTER_HELP)]
struct TreeArgs {
    /// Root path(s) to render. Defaults to current directory. Multiple
    /// paths render in order, blank line between.
    #[arg(value_name = "PATH", num_args = 0..)]
    paths: Vec<PathBuf>,

    /// Maximum nesting depth. Beyond this, dirs collapse to
    /// `{ ... N subdirs, M files (depth cap) }`.
    #[arg(long, value_name = "N")]
    depth: Option<usize>,

    /// Inline parsed Rust items (structs, enums, traits, impls, fns,
    /// consts, statics, type aliases, macros) for each `.rs` file.
    /// IMPLICITLY sets `--depth 4` if `--depth` is not given (prevents
    /// wall-of-text on large trees).
    #[arg(long)]
    detail: bool,

    /// Annotate every parsed item with `:start-end` line ranges, and files
    /// with `:LOC`. Useful with `--detail` for jumping straight to a symbol.
    #[arg(long)]
    lines: bool,

    /// Cap entries in a dir whose relative path EQUALS PREFIX. Repeatable.
    /// Truncates the head, keeps the tail (newest-by-name wins for sorted
    /// dirs). DEFAULT CAP applied unless `--no-default-caps`: docs/sessions=10.
    #[arg(long, value_name = "PREFIX=N", value_parser = parse_cap)]
    cap: Vec<(String, usize)>,

    /// Disable the built-in `docs/sessions=10` default cap. Use when you
    /// explicitly want every session file rendered.
    #[arg(long)]
    no_default_caps: bool,

    /// Skip paths whose relative path CONTAINS this substring. Repeatable.
    /// Substring match, not glob.
    #[arg(long, value_name = "SUBSTR")]
    exclude: Vec<String>,

    /// Inclusive allowlist: only include files with these extensions
    /// (without leading dot). Comma-separated or repeated. e.g.
    /// `--ext rs,md`. Files outside the allowlist are dropped — including
    /// Cargo.toml, README, etc.
    #[arg(long, value_name = "EXT", value_delimiter = ',')]
    ext: Vec<String>,
}

const TREE_AFTER_HELP: &str = "\
EXAMPLES:
  rmap tree                               # full repo tree, no item detail
  rmap tree --depth 2                     # top two levels only
  rmap tree --detail                      # Rust items inlined, depth auto-capped at 4
  rmap tree --detail --lines              # items + per-symbol line ranges + per-file LOC
  rmap tree --ext rs                      # Rust files only
  rmap tree --exclude tests --exclude bench
  rmap tree --cap docs/sessions=3         # override the default cap
";

#[derive(Args)]
#[command(after_long_help = MODULE_AFTER_HELP)]
struct ModuleArgs {
    /// What to index. Accepts one or more of: exact directory path (e.g.
    /// `src/domain/computer`), exact file path (e.g. `src/walk.rs`,
    /// renders that file's items only), or a UNIQUE PATH SUFFIX (e.g.
    /// `computer` resolves to `src/domain/computer`). On ambiguous suffix,
    /// candidates are listed and exit code is non-zero. Multiple paths
    /// render in the order given, blank line between.
    #[arg(value_name = "PATH_OR_SUFFIX", num_args = 1.., required = true)]
    paths: Vec<PathBuf>,

    /// Maximum nesting depth within the subtree.
    #[arg(long, value_name = "N")]
    depth: Option<usize>,

    /// Annotate every parsed item with `:start-end` line ranges, and files
    /// with `:LOC`.
    #[arg(long)]
    lines: bool,

    /// Inclusive allowlist of file extensions (without leading dot).
    /// Comma-separated or repeated. e.g. `--ext rs,md`.
    #[arg(long, value_name = "EXT", value_delimiter = ',')]
    ext: Vec<String>,
}

#[derive(Args)]
#[command(after_long_help = REFS_AFTER_HELP)]
struct RefsArgs {
    /// Identifier(s) to search for. Matches the LAST path segment (so
    /// `foo` matches `a::b::foo` and bare `foo`). Case-sensitive.
    /// Multiple names render in groups, blank line between.
    #[arg(value_name = "NAME", num_args = 1.., required = true)]
    names: Vec<String>,

    /// Root path(s) to scan. Repeatable, and comma-delimited values are
    /// accepted (`--in a --in b` or `--in a,b`). Defaults to current
    /// directory when omitted. Note: each `--in` consumes exactly one
    /// path; bare positionals after the flag are treated as identifier
    /// NAMEs, not additional scopes.
    #[arg(
        long = "in",
        value_name = "PATH",
        action = clap::ArgAction::Append,
        value_delimiter = ',',
    )]
    paths: Vec<PathBuf>,

    /// Only emit definitions, skip uses.
    #[arg(long, conflicts_with = "uses_only")]
    defs_only: bool,

    /// Only emit uses, skip definitions.
    #[arg(long, conflicts_with = "defs_only")]
    uses_only: bool,

    /// Print source excerpt with ±N lines of context around each hit.
    /// Use `--excerpt 0` for hit line only. Hit line is marked with `>`.
    #[arg(long, value_name = "N")]
    excerpt: Option<usize>,
}

const REFS_AFTER_HELP: &str = "\
EXAMPLES:
  rmap refs enumerate                     # all defs + uses of `enumerate`
  rmap refs Filter --defs-only            # where is `Filter` defined?
  rmap refs render_file --uses-only       # who calls `render_file`?
  rmap refs Foo Bar Baz                   # multiple names (groups, blank line between)
  rmap refs Foo --in src/domain           # scope search to a subtree
  rmap refs Foo --in src/a --in src/b     # multiple scopes (repeat --in)
  rmap refs Foo --in src/a,src/b          # comma-delimited form
  rmap refs collect_idents --excerpt 2    # show ±2 lines of source around each hit

OUTPUT:
  Each hit on its own line:
    <file>:<line>:<col> def <kind> <name>
    <file>:<line>:<col> use <kind> <name>
  def kinds: fn, method, struct, enum, union, trait, const, static, type, macro
  use kinds: call, method, type, struct-lit, path, macro, import, pat

  When no hits, prints `no hits for `<name>`` and, if any similar
  identifiers exist in scope, a `did you mean: ...` line ranked by
  substring, snake/camelCase token overlap, longest common prefix,
  character-bigram Jaccard similarity, and edit distance.

LIMITATIONS:
  - Matches by trailing identifier only (no name resolution). A hit on
    `foo` matches every `foo` regardless of which `foo` it resolves to.
    Scope with PATH or read the surrounding file to disambiguate.
  - Rust only. Non-`.rs` files are ignored.
";

#[derive(Args)]
#[command(after_long_help = DEPS_AFTER_HELP)]
struct DepsArgs {
    /// What to render. Default: current directory (whole-repo graph).
    /// Accepts:
    ///   - dir containing Cargo.toml → whole-repo graph
    ///   - subdirectory               → graph scoped to files under that dir
    ///                                  (repo root auto-detected by walking up)
    ///   - `.rs` file                 → focus mode (in / out / ext for one file)
    #[arg(default_value = ".", value_name = "PATH")]
    path: PathBuf,

    /// Show reverse edges (who imports me) instead of forward (what I import).
    /// Ignored in focus mode (focus always shows both).
    #[arg(long)]
    reverse: bool,

    /// Include external crate edges (std, third-party). Off by default to
    /// keep the internal architecture readable.
    #[arg(long)]
    ext: bool,
}

const DEPS_AFTER_HELP: &str = "\
EXAMPLES:
  rmap deps                               # whole-repo forward graph
  rmap deps --reverse                     # who imports each file
  rmap deps --ext                         # include external crate edges
  rmap deps src/walk.rs                   # focus: in + out + ext for one file
  rmap deps src/domain                    # scope to a subtree (rows for files under)
  rmap deps src/domain --reverse          # who imports files under src/domain

OUTPUT:
  Whole-repo (forward):  `<file> -> <dep1>, <dep2>, ...`
  Whole-repo (reverse):  `<file> <- <caller1>, <caller2>, ...`
  Focus mode:
    <file>
      out: <internal imports>
      in:  <internal callers>
      ext: <external crates>   (only with --ext)

LIMITATIONS:
  - Single-crate only. Looks for src/main.rs then src/lib.rs.
    Workspaces and `[[bin]]` overrides not parsed (Tier 1).
  - Cfg-gated `mod` / `use` items followed unconditionally.
  - `use a::b::*;` resolves to module file (one edge, no symbol detail).
";

#[derive(Args)]
#[command(after_long_help = GRAPH_AFTER_HELP)]
struct GraphArgs {
    /// Entry file(s) to start the walk from. Defaults to detected crate
    /// root (`src/main.rs` then `src/lib.rs`). Multiple entries render
    /// as separate subgraphs, blank line between.
    #[arg(value_name = "ENTRY", num_args = 0..)]
    entries: Vec<PathBuf>,

    /// Repo root to scan. Defaults to current directory.
    #[arg(long, default_value = ".", value_name = "PATH")]
    repo: PathBuf,

    /// Walk reverse edges (who can reach ENTRY) instead of forward.
    #[arg(long)]
    reverse: bool,

    /// Emit a Mermaid `graph TD` diagram instead of the brace tree.
    #[arg(long)]
    mermaid: bool,

    /// Group mermaid nodes into `subgraph` clusters by top-level dir under
    /// `src/` (e.g. `domain`, `engine`, `ui`). Implies `--mermaid`.
    #[arg(long, requires = "mermaid")]
    mermaid_cluster: bool,

    /// Number of dir levels under `src/` used as the cluster key. Default 1
    /// (e.g. `domain`). 2 splits sub-clusters (`domain/econ`,
    /// `domain/fleet`). Useful when a single top-level dir has too many
    /// nodes to render compactly. Requires `--mermaid-cluster`.
    #[arg(long, value_name = "N", default_value_t = 1, requires = "mermaid_cluster")]
    cluster_depth: usize,

    /// Maximum BFS depth from entry. Default: unlimited.
    #[arg(long, value_name = "N")]
    depth: Option<usize>,

    /// Include external crate edges in the walk (forward only).
    #[arg(long)]
    ext: bool,
}

const GRAPH_AFTER_HELP: &str = "\
EXAMPLES:
  rmap graph                              # brace tree from crate root
  rmap graph src/refs.rs                  # brace tree from refs.rs
  rmap graph src/walk.rs --reverse        # who reaches walk.rs?
  rmap graph --depth 2                    # depth-bounded
  rmap graph --mermaid > g.md             # mermaid diagram
  rmap graph --mermaid --mermaid-cluster                  # group by top-level dir
  rmap graph --mermaid --mermaid-cluster --cluster-depth 2 # split into sub-dirs

OUTPUT (brace tree, default):
  Single line per entry, e.g.:
    main { deps, graph { deps* }, refs { walk }, render { parse*, walk* }, walk* }
  Markers:
    *     revisit (already shown elsewhere; children elided)
    ~     back-edge (cycle in current DFS path)
    {…}   depth limit reached but children exist
  File names are stems (`src/walk.rs` → `walk`); `mod.rs` / `lib.rs` /
  `main.rs` are prefixed with their parent dir to disambiguate.

OUTPUT (--mermaid):
  `graph TD` block, each node declared once, then `a --> b` edges. Node
  labels are file stems; when stems collide across the rendered graph,
  the parent dir is prefixed (e.g. two `balance` nodes become
  `econ/balance` and `fleet/balance`).
  With `--mermaid-cluster`, nodes are wrapped in `subgraph <dir>` blocks
  by top-level dir under `src/`. Files directly in `src/` and external
  crates render outside any subgraph.

NOTES:
  - Reuses the same dep graph as `rmap deps`. Same single-crate limit.
  - `--ext` is ignored in reverse mode (external crates have no inbound
    edges in this model).
";

#[derive(Args)]
#[command(after_long_help = BODY_AFTER_HELP)]
struct BodyArgs {
    /// Symbol(s) to print. Accepts plain `name` or `Type::method`.
    /// Multiple names render in groups, blank line between.
    #[arg(value_name = "NAME", num_args = 1.., required = true)]
    names: Vec<String>,

    /// Root path(s) to scan. Repeatable, comma-delimited values accepted
    /// (`--in a --in b` or `--in a,b`). Defaults to current directory.
    #[arg(
        long = "in",
        value_name = "PATH",
        action = clap::ArgAction::Append,
        value_delimiter = ',',
    )]
    paths: Vec<PathBuf>,

    /// Filter by item kind. One of: fn, method, struct, enum, union,
    /// trait, impl, const, static, type, macro.
    #[arg(long, value_name = "KIND")]
    kind: Option<String>,
}

const BODY_AFTER_HELP: &str = "\
EXAMPLES:
  rmap body run_refs                      # print fn body
  rmap body Foo::bar                      # impl method on Foo
  rmap body Filter                        # struct/enum/trait body
  rmap body Foo --kind impl               # the `impl Foo { ... }` block
  rmap body run_body --in src/main.rs     # scoped lookup

OUTPUT:
  Each match prefixed with a header:
    // <file>:<start>-<end> <kind> <name>
    <verbatim source lines>
  Multiple matches separated by a blank line.

LIMITATIONS:
  - Matches by trailing identifier (no name resolution). Use `Type::name`
    or `--in PATH` / `--kind` to disambiguate.
  - Rust only. Non-`.rs` files ignored.
  - Span is reported via `syn`; doc comments above an item are included
    when they are attached as attributes (the usual case).
";

const MODULE_AFTER_HELP: &str = "\
EXAMPLES:
  rmap module computer                    # fuzzy: resolves to unique src/.../computer
  rmap module src/domain/hull             # exact directory path
  rmap module src/walk.rs                 # single-file item index
  rmap module computer --lines            # add per-symbol line ranges
  rmap module src/a.rs src/b.rs src/c.rs  # multiple files (blank line between)

NOTES:
  - Item detail is ALWAYS on for `module` (that's the point of the subcommand).
  - Suffix resolution searches the whole repo; ambiguous suffixes are rejected.
";

fn main() -> ExitCode {
    let cli = Cli::parse();

    // `agent` / `completions` take no paths, so `--crate` is irrelevant to them
    // — handle them before resolving (a bad SPEC must not block `rmap agent`).
    match cli.cmd {
        Cmd::Agent => return run_agent(),
        Cmd::Completions { shell } => return run_completions(shell),
        _ => {}
    }

    // Resolve `--crate SPEC` once to a source root used as the base for all
    // relative paths. `None` means "current directory" (the default behavior).
    let base: Option<PathBuf> = match cli.krate.as_deref() {
        Some(spec) => match krate::resolve(spec) {
            Ok(p) => Some(p),
            Err(e) => {
                eprintln!("{e}");
                return ExitCode::from(1);
            }
        },
        None => None,
    };
    let base = base.as_deref();
    match cli.cmd {
        Cmd::Tree(args) => run_tree(args, base),
        Cmd::Module(args) => run_module(args, base),
        Cmd::Refs(args) => run_refs(args, base),
        Cmd::Deps(args) => run_deps(args, base),
        Cmd::Graph(args) => run_graph(args, base),
        Cmd::Body(args) => run_body(args, base),
        Cmd::Agent | Cmd::Completions { .. } => unreachable!("handled above"),
    }
}

/// Resolve a user-supplied path against an optional crate base. With a base,
/// relative paths join onto it; without, they pass through unchanged.
fn rebase(base: Option<&Path>, p: &Path) -> PathBuf {
    match base {
        Some(b) => b.join(p),
        None => p.to_path_buf(),
    }
}

/// The default root to scan when no positional path is given: the crate root
/// under `--crate`, else the current directory.
fn default_root(base: Option<&Path>) -> PathBuf {
    base.map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."))
}

fn run_tree(args: TreeArgs, base: Option<&Path>) -> ExitCode {
    let mut caps = args.cap;
    if !args.no_default_caps && caps.is_empty() {
        caps.push((DEFAULT_SESSIONS_CAP.0.to_string(), DEFAULT_SESSIONS_CAP.1));
    }
    let depth = match (args.detail, args.depth) {
        (true, None) => Some(DETAIL_DEFAULT_DEPTH),
        (_, d) => d,
    };
    let filter = walk::Filter {
        exclude: args.exclude,
        ext: args.ext,
    };
    let opts = render::TreeOptions {
        depth,
        detail: args.detail,
        lines: args.lines,
        caps,
    };
    let paths = if args.paths.is_empty() {
        vec![default_root(base)]
    } else {
        args.paths.iter().map(|p| rebase(base, p)).collect()
    };
    let mut exit = ExitCode::SUCCESS;
    for (i, path) in paths.iter().enumerate() {
        if i > 0 {
            println!();
        }
        let tree = match walk::enumerate(path, &filter) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("{e}");
                exit = ExitCode::from(1);
                continue;
            }
        };
        print!("{}", render::tree(&tree, &opts));
    }
    exit
}

fn run_module(args: ModuleArgs, base: Option<&Path>) -> ExitCode {
    let mut exit = ExitCode::SUCCESS;
    for (i, path) in args.paths.iter().enumerate() {
        if i > 0 {
            println!();
        }
        let resolved = match resolve_module_path(path, base) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("{e}");
                exit = ExitCode::from(1);
                continue;
            }
        };
        if resolved.is_file() {
            print!("{}", render::file_module(&resolved, args.lines));
            continue;
        }
        let filter = walk::Filter {
            exclude: Vec::new(),
            ext: args.ext.clone(),
        };
        let opts = render::TreeOptions {
            depth: args.depth,
            detail: true,
            lines: args.lines,
            caps: Vec::new(),
        };
        let tree = match walk::enumerate(&resolved, &filter) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("{e}");
                exit = ExitCode::from(1);
                continue;
            }
        };
        print!("{}", render::tree(&tree, &opts));
    }
    exit
}

fn run_refs(args: RefsArgs, base: Option<&Path>) -> ExitCode {
    // Guard: catch the most common mis-invocation —
    //   `rmap refs Foo --in src/a src/b`
    // clap consumes only `src/a` as `--in`; `src/b` lands in `names`.
    // Reject path-shaped names with a hint instead of silently searching
    // for a "src/b" identifier.
    for n in &args.names {
        if n.contains('/') || n.ends_with(".rs") {
            eprintln!(
                "error: `{n}` looks like a path, not an identifier. \
                 Did you mean `--in {n}`? Each `--in` flag takes exactly \
                 one path; repeat the flag (or use `--in a,b`) for \
                 multiple roots."
            );
            return ExitCode::from(2);
        }
    }

    let mode = match (args.defs_only, args.uses_only) {
        (true, _) => refs::Mode::DefsOnly,
        (_, true) => refs::Mode::UsesOnly,
        _ => refs::Mode::Both,
    };
    let roots: Vec<PathBuf> = if args.paths.is_empty() {
        vec![default_root(base)]
    } else {
        args.paths.iter().map(|p| rebase(base, p)).collect()
    };
    for (i, name) in args.names.iter().enumerate() {
        if i > 0 {
            println!();
        }
        let opts = refs::RefsOptions {
            name: name.clone(),
            mode,
            excerpt: args.excerpt,
        };
        print!("{}", refs::run(&roots, &opts));
    }
    ExitCode::SUCCESS
}

fn run_deps(args: DepsArgs, base: Option<&Path>) -> ExitCode {
    let target = rebase(base, &args.path);
    let (repo, focus, scope) = resolve_deps_target(&target);
    let mode = if args.reverse {
        deps::Mode::Reverse
    } else {
        deps::Mode::Forward
    };
    let opts = deps::DepsOptions {
        focus,
        mode,
        include_external: args.ext,
        scope,
    };
    print!("{}", deps::run(&repo, &opts));
    ExitCode::SUCCESS
}

/// Map a positional `deps` PATH to (repo_root, focus, scope_prefix):
///   - file path                       → focus mode (repo = walked-up Cargo.toml dir)
///   - dir containing Cargo.toml       → repo mode, no scope
///   - dir without Cargo.toml          → walk up to find Cargo.toml,
///                                       use that as repo, scope to the dir's
///                                       repo-relative path
fn resolve_deps_target(path: &Path) -> (PathBuf, Option<PathBuf>, Option<String>) {
    if path.is_file() {
        let parent = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let repo = walk_up_for_cargo(&parent);
        return (repo, Some(path.to_path_buf()), None);
    }
    // Directory case.
    let abs = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if abs.join("Cargo.toml").is_file() {
        return (path.to_path_buf(), None, None);
    }
    let repo = walk_up_for_cargo(&abs);
    let canon_repo = std::fs::canonicalize(&repo).unwrap_or_else(|_| repo.clone());
    let scope = abs
        .strip_prefix(&canon_repo)
        .ok()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .filter(|s| !s.is_empty());
    (repo, None, scope)
}

fn walk_up_for_cargo(start: &Path) -> PathBuf {
    let abs = std::fs::canonicalize(start).unwrap_or_else(|_| start.to_path_buf());
    let mut cur: &Path = abs.as_path();
    loop {
        if cur.join("Cargo.toml").is_file() {
            return cur.to_path_buf();
        }
        match cur.parent() {
            Some(p) => cur = p,
            None => return cur.to_path_buf(),
        }
    }
}

fn run_graph(args: GraphArgs, base: Option<&Path>) -> ExitCode {
    let direction = if args.reverse {
        graph::Direction::Reverse
    } else {
        graph::Direction::Forward
    };
    let repo = rebase(base, &args.repo);
    let entries: Vec<Option<PathBuf>> = if args.entries.is_empty() {
        vec![None]
    } else {
        args.entries.iter().map(|e| Some(rebase(base, e))).collect()
    };
    for (i, entry) in entries.into_iter().enumerate() {
        if i > 0 {
            println!();
        }
        let opts = graph::GraphOptions {
            entry,
            direction,
            mermaid: args.mermaid,
            mermaid_cluster: args.mermaid_cluster,
            cluster_depth: args.cluster_depth,
            depth: args.depth,
            include_external: args.ext,
        };
        print!("{}", graph::run(&repo, &opts));
    }
    ExitCode::SUCCESS
}

fn run_body(args: BodyArgs, base: Option<&Path>) -> ExitCode {
    for n in &args.names {
        if n.contains('/') || n.ends_with(".rs") {
            eprintln!(
                "error: `{n}` looks like a path, not a symbol. \
                 Did you mean `--in {n}`?"
            );
            return ExitCode::from(2);
        }
    }
    let roots: Vec<PathBuf> = if args.paths.is_empty() {
        vec![default_root(base)]
    } else {
        args.paths.iter().map(|p| rebase(base, p)).collect()
    };
    for (i, name) in args.names.iter().enumerate() {
        if i > 0 {
            println!();
        }
        let opts = body::BodyOptions {
            name: name.clone(),
            kind: args.kind.clone(),
        };
        print!("{}", body::run(&roots, &opts));
    }
    ExitCode::SUCCESS
}

fn run_agent() -> ExitCode {
    print!("{AGENT_SNIPPET}");
    ExitCode::SUCCESS
}

const AGENT_SNIPPET: &str = "\
## `rmap` — use this for codebase layout

Prefer `rmap` over `find`/`ls`/`tree`/`grep -rl`. Git-aware, parses Rust
items via `syn`, brace output on stdout.

- `rmap tree` — file/dir layout from a root. Add `--detail` to inline
  Rust items per `.rs` file.
- `rmap module <path-or-suffix>...` — focused index of one or more
  subtrees or files, items always inlined. Suffix resolves if exactly
  one dir matches. Multiple args render in order, blank line between.
  Add `--lines` for per-symbol line ranges (replaces blind `sed -n A,Bp`).
- `rmap refs <Name>...` — find defs + uses of one or more Rust
  identifiers. `--defs-only` / `--uses-only` to filter, `--in PATH`
  to scope, `--excerpt N` to inline ±N source lines per hit (hit line
  marked `>`, replaces `sed -n` after `grep -n`). Matches by trailing
  path segment. Parsed via `syn` — no false hits in comments/strings.
- `rmap deps [PATH]` — file-level dep graph from `use` + `mod`. Default
  whole-repo forward (`file -> deps`). `--reverse` for callers,
  `--ext` for external crates. Pass a `.rs` file to focus on one file.
- `rmap graph [ENTRY...]` — reachability subgraph from one or more
  entry files (default: detected crate root). Compact brace tree by default
  (`main { deps, refs { walk }, ... }`). Markers: `*` revisit,
  `~` cycle, `{…}` depth limit. `--reverse`, `--depth N`, `--ext`,
  `--mermaid` for diagram output.
- `rmap body <Name>...` — print the full source body of a Rust item by
  name. Accepts `Type::method` for impl methods. `--in PATH` scopes,
  `--kind KIND` filters (fn|method|struct|enum|trait|impl|const|...).
  Replaces `rmap module --lines` + `sed -n A,Bp`.
- `--crate <name|name@version>` — global flag on every lens. Rebases all
  paths onto a dependency's source in the local cargo registry, so you can
  read a crate you depend on instead of guessing its API. Offline (must be
  `cargo fetch`ed); bare name uses the Cargo.lock pin. E.g.
  `rmap body Deserialize --crate serde`, `rmap tree --crate tokio --depth 2`.

Sample (`rmap module src/walk.rs`):

```
walk.rs { const SKIP_DIRS, enum Node { fn name }, struct Filter, fn enumerate, ... }
```

Anti-patterns — replace these combos:

- `grep -n Foo path/to/file.rs && sed -n 'A,Bp' path/to/file.rs`
  → `rmap refs Foo --in path/to/file.rs --excerpt 3`
- `grep -rn 'fn foo\\|pub fn foo' src/`
  → `rmap refs foo --defs-only`
- `head -40 file.rs` (to learn file shape)
  → `rmap module file.rs --lines`
- `rmap module file.rs --lines && sed -n 'A,Bp' file.rs` (to read one fn)
  → `rmap body fn_name`  (or `rmap body Type::method`)
- `find src -name '*.rs' | xargs grep -l Bar`
  → `rmap refs Bar` (lists hits per file, parsed not regex)

Run `rmap --help` for flags (`--depth`, `--lines`, `--ext`, ...).
";

fn run_completions(shell: Shell) -> ExitCode {
    let mut cmd = Cli::command();
    let name = cmd.get_name().to_string();
    clap_complete::generate(shell, &mut cmd, name, &mut io::stdout());
    ExitCode::SUCCESS
}

fn parse_cap(s: &str) -> Result<(String, usize), String> {
    let (prefix, n) = s
        .rsplit_once('=')
        .ok_or_else(|| format!("expected `PREFIX=N`, got `{s}`"))?;
    let n: usize = n
        .parse()
        .map_err(|_| format!("N must be a non-negative integer, got `{n}`"))?;
    Ok((prefix.to_string(), n))
}

/// Resolve a module path. If the input exists, return it. Otherwise search
/// the repo for directories whose path ends in the input as a path-aligned
/// suffix; succeed iff exactly one candidate matches.
fn resolve_module_path(input: &Path, base: Option<&Path>) -> Result<PathBuf, String> {
    let candidate = rebase(base, input);
    if candidate.exists() {
        return Ok(candidate);
    }
    let needle = input.to_string_lossy().replace('\\', "/");
    let needle = needle.trim_matches('/');
    if needle.is_empty() {
        return Err(format!("error: `{}` does not exist", input.display()));
    }
    let search_root = default_root(base);
    let candidates = walk::find_dirs_matching_suffix(&search_root, needle);
    match candidates.len() {
        0 => Err(format!(
            "error: `{}` does not exist and no directory ending in `{}` was found",
            input.display(),
            needle,
        )),
        1 => Ok(candidates.into_iter().next().unwrap()),
        _ => {
            let mut msg = format!(
                "error: `{}` is ambiguous — {} candidates:\n",
                needle,
                candidates.len()
            );
            for c in &candidates {
                msg.push_str(&format!("  {}\n", c.display()));
            }
            msg.push_str("Use the full path to disambiguate.");
            Err(msg)
        }
    }
}
