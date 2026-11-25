//! `:TSInstall <lang>` — fetch a treesitter grammar's source, compile it to a
//! loadable parser, and drop the matching queries into the data dir, all
//! in-process (a prebuilt bemtvi needs no external `curl`/`git`/`tar`).
//!
//! Source of truth is **nvim-treesitter**, pinned to one commit ([`NVIM_TS_REF`])
//! so installs are reproducible: that commit's `parsers.lua` names every grammar
//! repo + the exact revision to build, and its `runtime/queries/<lang>/` holds the
//! queries matched to that revision. The flow for `install(data_dir, lang)`:
//!
//!   1. fetch `parsers.lua`, extract the `<lang>` `install_info` (url + revision),
//!   2. download the grammar repo tarball at that revision and unpack it,
//!   3. compile `src/parser.c` (+ a `scanner.{c,cc}` if present) into
//!      `<data>/parser/<lang>.so`, using a resolved C compiler — generating
//!      `parser.c` first, in-process, when the repo commits only `grammar.js`
//!      ([`crate::generate`]),
//!   4. copy the standard queries into `<data>/queries/<lang>/`.
//!
//! The compiler is resolved in [`resolve_compiler`]: an explicit `$BEMTVI_CC`
//! wins, else a **pinned Zig** is fetched and checksum-verified ([`ensure_zig`]) —
//! never a compiler found on PATH — so the user needs no toolchain at all.
//!
//! Everything fails *loud* (no silent stub ever leaves a half-installed grammar
//! that "loads but is wrong"). Tests avoid the network via `$BEMTVI_TS_MIRROR`,
//! which redirects every HTTP GET to a local directory tree (see [`fetch`]), and
//! `$BEMTVI_CC` to pin the compiler.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};

/// nvim-treesitter commit the install reads from — `parsers.lua` (grammar repos +
/// pinned revisions) and `runtime/queries/`. Pinning the ref pins every grammar
/// transitively. Override with `$BEMTVI_TS_REF` (a branch, tag, or sha).
const NVIM_TS_REF: &str = "52773f0d13a0c11d0c3ef33c14ab6ecc1c17148b";

/// nvim-treesitter-textobjects commit the install reads `textobjects.scm` from.
/// The textobjects queries live in this **separate** repo (nvim-treesitter core
/// ships none), under `queries/<lang>/textobjects.scm` — a different path than the
/// core repo's `runtime/queries/`. Pinned; override with `$BEMTVI_TS_TEXTOBJECTS_REF`.
const NVIM_TS_TEXTOBJECTS_REF: &str = "5c7b0263797dfd1bd6202f2b219f3b53a80b2187";

/// Zig version fetched to build every grammar (unless `$BEMTVI_CC` is set), as a
/// macro so the per-target archive names in [`zig_target`] are spelled from it.
/// Bump it together with the SHA-256s there (from `ziglang.org`'s
/// `download/index.json`), then run the live install test on each target.
macro_rules! zig_version {
    () => {
        "0.17.1"
    };
}

/// [`zig_version!`] as a constant — public so tests can find the unpacked toolchain.
pub const ZIG_VERSION: &str = zig_version!();

/// The standard nvim-treesitter query files, in the order we copy them. Missing
/// ones (a 404) are simply skipped — not every grammar ships every query.
const QUERY_FILES: &[&str] = &["highlights", "indents", "injections", "folds", "locals"];

/// What [`install`] resolved and produced — surfaced to the user by `:TSInstall`.
#[derive(Debug, Clone)]
pub struct InstallReport {
    pub lang: String,
    /// The grammar revision that was built: nvim-treesitter's pinned one, or for a
    /// `lang@ref` install the commit `ref` resolved to (the ref itself when the
    /// archive doesn't say).
    pub revision: String,
    /// For a `lang@ref` install, the ref it follows (`HEAD`, a branch, tag or sha):
    /// recorded so `:TSUpdate` builds it again. `None` for the pinned revision and
    /// for a repo install.
    pub follows: Option<String>,
    /// The compiled parser written under `<data>/parser/`.
    pub parser: PathBuf,
    /// Query file basenames copied into `<data>/queries/<lang>/` (e.g. `indents`).
    pub queries: Vec<String>,
    /// Inherited languages whose query sets were also fetched (query-only, no
    /// parser) by following `; inherits:` modelines — e.g. `["ecma", "jsx"]` for
    /// `javascript`. Empty for a self-contained grammar.
    pub inherited: Vec<String>,
    /// Human description of the compiler used (`$BEMTVI_CC`, `cc`, `zig (fetched)`…).
    pub compiler: String,
    /// File extensions the grammar declares it applies to (from a repo install's
    /// `tree-sitter.json` / `package.json` `grammars[].file-types`), e.g. `["rb"]`.
    /// Empty for a catalog install (the static `EXT_FILETYPE` table already covers
    /// those) or a repo that declares none. Phase 2 registers these for detection.
    pub file_types: Vec<String>,
}

/// The `install_info` we need out of `parsers.lua`.
struct ParserEntry {
    url: String,
    revision: String,
    /// Subdirectory of the repo holding `src/` (the `location` field), if any.
    location: Option<String>,
}

/// Install grammar `spec` into `data_dir` (`:TSInstall`). `spec` is one of:
///
/// - a catalog **language name** (`rust`), resolved through nvim-treesitter's
///   `parsers.lua` and built at its pinned revision;
/// - a catalog language **at a ref** (`rust@HEAD`, or a branch, tag or sha): the same
///   grammar repo built at `ref`, with the queries still from the pinned
///   nvim-treesitter. The ref is recorded, so [`update`] follows it;
/// - a GitHub **repo spec** (`owner/repo` or `owner/repo@ref`), built straight from
///   the repo. A spec containing `/` is treated as a repo.
///
/// A plain language name forgets a ref recorded by an earlier `lang@ref` install.
/// See the module docs for the flow.
pub fn install(data_dir: &Path, spec: &str) -> Result<InstallReport> {
    if let Some(repo) = parse_repo_spec(spec) {
        let turn = install_turn(&format!("{}/{}", repo.owner, repo.repo));
        let _held = turn.lock().unwrap_or_else(|e| e.into_inner());
        return install_from_repo(data_dir, &repo);
    }
    let (lang, git_ref) = parse_catalog_spec(spec)?;
    let turn = install_turn(lang);
    let _held = turn.lock().unwrap_or_else(|e| e.into_inner());
    install_from_catalog(data_dir, lang, git_ref)
}

/// The lock an install of `key` (a language, or `owner/repo`) holds while it runs.
/// `:TSInstall rust rust@HEAD` runs both installs on parallel workers, and two installs
/// of one grammar share its build dir, its parser and its record: each would delete the
/// other's unpacked source. Different grammars still install in parallel. A poisoned
/// lock (an install panicked) guards no state, so callers take it anyway.
fn install_turn(key: &str) -> std::sync::Arc<std::sync::Mutex<()>> {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};
    static TURNS: OnceLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();
    TURNS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(key.to_string())
        .or_default()
        .clone()
}

/// Reinstall grammar `spec` (`:TSUpdate`). Like [`install`], except that a plain
/// language name installed earlier as `lang@ref` is built at that ref again rather
/// than going back to the pinned revision.
pub fn update(data_dir: &Path, spec: &str) -> Result<InstallReport> {
    if parse_repo_spec(spec).is_none() && !spec.contains('@') {
        let turn = install_turn(spec);
        let held = turn.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(record) = read_follow_record(data_dir, spec)? {
            return install_from_catalog(data_dir, spec, Some(&record.git_ref));
        }
        // No record: a plain install, which takes the turn itself.
        drop(held);
    }
    install(data_dir, spec)
}

/// Split a catalog spec into the language and an optional ref: `lua` or `lua@HEAD`.
fn parse_catalog_spec(spec: &str) -> Result<(&str, Option<&str>)> {
    match spec.split_once('@') {
        None => Ok((spec, None)),
        Some((lang, git_ref)) => {
            if !valid_ref(git_ref) {
                bail!("invalid ref '{git_ref}' in '{spec}' (expected a branch, tag or sha)");
            }
            Ok((lang, Some(git_ref)))
        }
    }
}

/// Whether `git_ref` can name a branch, tag or sha. It goes into a URL path
/// (`archive/<ref>.tar.gz`), so it is limited to letters, digits and `-_.+/` — what
/// branch, tag and sha names use in practice — which keeps `?`, `#`, `%` and the like
/// from turning it into a query, a fragment or an escape. No `..`, no leading `/` or
/// `-`, no trailing `/` or `.`.
fn valid_ref(git_ref: &str) -> bool {
    !git_ref.is_empty()
        && git_ref
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+' | '/'))
        && !git_ref.contains("..")
        && !git_ref.contains("//")
        && !git_ref.starts_with(['/', '-'])
        && !git_ref.ends_with(['/', '.'])
}

/// What a `lang@ref` install records in `<data>/parser-info/<lang>.json`.
struct FollowRecord {
    git_ref: String,
}

fn follow_record_path(data_dir: &Path, lang: &str) -> PathBuf {
    data_dir.join("parser-info").join(format!("{lang}.json"))
}

/// The ref `lang` follows, from its record; `None` when it was installed at the
/// pinned revision (no record). A record that can't be read fails loud rather than
/// quietly falling back to the pinned revision.
fn read_follow_record(data_dir: &Path, lang: &str) -> Result<Option<FollowRecord>> {
    if !crate::loader::is_valid_language(lang) {
        return Ok(None);
    }
    let path = follow_record_path(data_dir, lang);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    let json: serde_json::Value =
        serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    let git_ref = json
        .get("ref")
        .and_then(|v| v.as_str())
        .filter(|r| valid_ref(r))
        .with_context(|| format!("{} has no valid \"ref\"", path.display()))?;
    Ok(Some(FollowRecord {
        git_ref: git_ref.to_string(),
    }))
}

/// The ref `lang` follows, for `:TSInstallInfo`: `(ref, commit)` from its record, or
/// `None` at the pinned revision (or with no readable record).
pub fn followed_ref(data_dir: &Path, lang: &str) -> Option<(String, Option<String>)> {
    let text = std::fs::read_to_string(follow_record_path(data_dir, lang)).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    let git_ref = json.get("ref")?.as_str()?.to_string();
    let commit = json
        .get("commit")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    Some((git_ref, commit))
}

/// Record (`Some`) or forget (`None`) the ref `lang` follows.
fn write_follow_record(
    data_dir: &Path,
    lang: &str,
    follow: Option<(&str, Option<&str>)>,
) -> Result<()> {
    let path = follow_record_path(data_dir, lang);
    match follow {
        None => match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("remove {}", path.display())),
        },
        Some((git_ref, commit)) => {
            let dir = path.parent().expect("record path has a parent");
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
            let json = serde_json::json!({ "ref": git_ref, "commit": commit });
            std::fs::write(&path, serde_json::to_string_pretty(&json)? + "\n")
                .with_context(|| format!("write {}", path.display()))
        }
    }
}

/// The commit a GitHub archive was made from. GitHub builds archives with
/// `git archive`, which writes the commit id into a pax global header (`comment`)
/// at the start of the tarball. `None` if there isn't one.
fn archive_commit(tarball: &[u8]) -> Option<String> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(tarball));
    let mut entries = archive.entries().ok()?;
    let mut first = entries.next()?.ok()?;
    if !first.header().entry_type().is_pax_global_extensions() {
        return None;
    }
    for ext in first.pax_extensions().ok()??.flatten() {
        if ext.key().ok()? == "comment" {
            let value = ext.value().ok()?.trim();
            if value.len() == 40 && value.chars().all(|c| c.is_ascii_hexdigit()) {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// Install a catalog language: resolve `parsers.lua`, build the grammar at the
/// pinned revision (or at `git_ref`), copy nvim-treesitter's matched query sets.
fn install_from_catalog(
    data_dir: &Path,
    lang: &str,
    git_ref: Option<&str>,
) -> Result<InstallReport> {
    if !crate::loader::is_valid_language(lang) {
        bail!("invalid language name '{lang}' (letters, digits, '_' and '-' only)");
    }

    let entry = resolve_entry(lang)?;
    let (owner, repo) = github_owner_repo(&entry.url)
        .with_context(|| format!("grammar url is not a GitHub repo: {}", entry.url))?;

    // 1. Download + unpack the grammar source at the pinned revision (or the ref).
    let build_dir = data_dir.join(".ts-build").join(lang);
    let _ = std::fs::remove_dir_all(&build_dir);
    std::fs::create_dir_all(&build_dir)
        .with_context(|| format!("create build dir {}", build_dir.display()))?;
    let wanted = git_ref.unwrap_or(&entry.revision);
    let tarball = fetch(&format!(
        "https://github.com/{owner}/{repo}/archive/{wanted}.tar.gz"
    ))
    .with_context(|| match git_ref {
        Some(r) => format!("download {owner}/{repo} at '{r}' — is the ref spelled right?"),
        None => format!("download {owner}/{repo} at {wanted}"),
    })?;
    let revision = match git_ref {
        Some(r) => archive_commit(&tarball).unwrap_or_else(|| r.to_string()),
        None => entry.revision.clone(),
    };
    unpack_tar_gz(&tarball, &build_dir).with_context(|| format!("unpack {repo} source tarball"))?;
    // The archive's single top-level directory — *discovered*, not computed.
    // GitHub names it `<repo>-<ref>` but mangles `<ref>`: a version-like tag
    // `v0.25.0` becomes `…-0.25.0` (leading `v` stripped), so `{repo}-{revision}`
    // would miss it. Reading the one extracted dir is robust to tag vs sha.
    let unpacked = single_subdir(&build_dir).with_context(|| {
        format!(
            "locating unpacked {repo} source under {}",
            build_dir.display()
        )
    })?;
    let src_root = match &entry.location {
        Some(loc) => unpacked.join(loc),
        None => unpacked.clone(),
    };
    // A repo that commits only `grammar.js` (nvim-treesitter's `generate = true`) is
    // generated here first.
    ensure_parser_c(&src_root, &unpacked, &format!("grammar '{lang}'"))?;
    let src_dir = src_root.join("src");
    let parser_c = src_dir.join("parser.c");

    // 2. Resolve a compiler and build the shared object.
    let (cc, compiler_desc) = resolve_compiler(data_dir)?;
    let parser_dir = data_dir.join("parser");
    std::fs::create_dir_all(&parser_dir)
        .with_context(|| format!("create {}", parser_dir.display()))?;
    let out = parser_dir.join(format!("{lang}.so"));
    compile(&cc, &src_dir, &parser_c, &out)?;

    // 3. Copy queries matched to this revision (plus inherited query sets).
    let (queries, inherited) = install_queries(data_dir, lang)?;

    // 4. Remember the ref (or forget an old one), so `:TSUpdate` builds the same.
    let commit = (git_ref.is_some() && revision.len() == 40).then_some(revision.as_str());
    write_follow_record(data_dir, lang, git_ref.map(|r| (r, commit)))?;

    // Best-effort cleanup of the unpacked source; the parser is self-contained.
    let _ = std::fs::remove_dir_all(&build_dir);

    Ok(InstallReport {
        lang: lang.to_string(),
        revision,
        follows: git_ref.map(str::to_string),
        parser: out,
        queries,
        inherited,
        compiler: compiler_desc,
        file_types: Vec::new(),
    })
}

// ---------------------------------------------------------------------------
// Repo install (`:TSInstall owner/repo[@ref]`)
// ---------------------------------------------------------------------------

/// A parsed `owner/repo[@ref]` GitHub grammar spec. `git_ref` defaults to `HEAD`
/// (GitHub's archive endpoint resolves it to the repo's default branch).
struct RepoSpec {
    owner: String,
    repo: String,
    git_ref: String,
}

/// Parse `owner/repo` or `owner/repo@ref` into a [`RepoSpec`], or `None` if `spec`
/// is not repo-shaped (no `/` → a catalog language name). `ref` may be a branch,
/// tag, or sha and can itself contain `/` (e.g. `feature/x`); only the part before
/// the first `@` is `owner/repo`. Owner/repo are restricted to GitHub's name
/// charset so a malformed spec fails here rather than as a confusing 404.
fn parse_repo_spec(spec: &str) -> Option<RepoSpec> {
    let (path, git_ref) = match spec.split_once('@') {
        Some((p, r)) => (p, r),
        None => (spec, "HEAD"),
    };
    let (owner, repo) = path.split_once('/')?;
    let name_ok = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    if !name_ok(owner) || !name_ok(repo) {
        return None;
    }
    if !valid_ref(git_ref) {
        return None;
    }
    Some(RepoSpec {
        owner: owner.to_string(),
        repo: repo.to_string(),
        git_ref: git_ref.to_string(),
    })
}

/// A grammar's own declared metadata, read from `tree-sitter.json` (modern) or a
/// `package.json` `tree-sitter` array (older). `path` is the grammar dir relative
/// to the repo root (`.` unless a monorepo places it in a subdir).
struct GrammarMeta {
    lang: String,
    file_types: Vec<String>,
    path: String,
}

/// Install a grammar straight from a GitHub repo: download + unpack the tarball at
/// `ref`, read the grammar's declared name / file-types / subdir, compile its
/// pre-generated `src/parser.c`, and copy the repo's own queries. Unlike the
/// catalog path this consults **no** nvim-treesitter data — the repo is the source
/// of truth — and it surfaces the declared `file_types` so detection can be wired.
fn install_from_repo(data_dir: &Path, spec: &RepoSpec) -> Result<InstallReport> {
    let RepoSpec {
        owner,
        repo,
        git_ref,
    } = spec;

    // 1. Download + unpack the repo source at `ref`.
    let build_dir = data_dir.join(".ts-build").join(format!("{owner}__{repo}"));
    let _ = std::fs::remove_dir_all(&build_dir);
    std::fs::create_dir_all(&build_dir)
        .with_context(|| format!("create build dir {}", build_dir.display()))?;
    let tarball = fetch(&format!(
        "https://github.com/{owner}/{repo}/archive/{git_ref}.tar.gz"
    ))
    .with_context(|| {
        format!("download {owner}/{repo}@{git_ref} — is the repo/ref spelled right?")
    })?;
    unpack_tar_gz(&tarball, &build_dir)
        .with_context(|| format!("unpack {owner}/{repo} source tarball"))?;
    let root = single_subdir(&build_dir)
        .with_context(|| format!("locating unpacked {owner}/{repo} source"))?;

    // 2. Read the grammar's declared metadata (name, file-types, subdir).
    let meta = read_grammar_metadata(&root, repo);
    if !crate::loader::is_valid_language(&meta.lang) {
        bail!(
            "derived language name '{}' is invalid (letters, digits, '_' and '-' only) — \
             the repo's tree-sitter.json `name` or repo name is unusable",
            meta.lang
        );
    }

    // The declared `path` is a subdir of the repo: an absolute or `..` one would build
    // (and generating would write) outside the unpacked source.
    if !Path::new(&meta.path).components().all(|c| {
        matches!(
            c,
            std::path::Component::Normal(_) | std::path::Component::CurDir
        )
    }) {
        bail!(
            "the grammar path {:?} declared by {owner}/{repo} is not a directory inside the repo",
            meta.path
        );
    }

    // 3. Compile the (pre-generated) parser.
    let grammar_root = if meta.path == "." {
        root.clone()
    } else {
        root.join(&meta.path)
    };
    ensure_parser_c(&grammar_root, &root, &format!("repo {owner}/{repo}"))?;
    let src_dir = grammar_root.join("src");
    let parser_c = src_dir.join("parser.c");
    let (cc, compiler_desc) = resolve_compiler(data_dir)?;
    let parser_dir = data_dir.join("parser");
    std::fs::create_dir_all(&parser_dir)
        .with_context(|| format!("create {}", parser_dir.display()))?;
    let out = parser_dir.join(format!("{}.so", meta.lang));
    compile(&cc, &src_dir, &parser_c, &out)?;

    // 4. Copy the repo's own queries (custom grammars aren't in nvim-treesitter).
    let queries = copy_repo_queries(data_dir, &meta.lang, &grammar_root)?;

    // This build replaces any catalog `lang@ref` one, so forget the ref it followed:
    // otherwise `:TSInstallInfo` keeps naming it and `:TSUpdate <lang>` rebuilds the
    // catalog grammar over this one.
    write_follow_record(data_dir, &meta.lang, None)?;

    let _ = std::fs::remove_dir_all(&build_dir);

    Ok(InstallReport {
        lang: meta.lang,
        revision: git_ref.clone(),
        follows: None,
        parser: out,
        queries,
        inherited: Vec::new(),
        compiler: compiler_desc,
        file_types: meta.file_types,
    })
}

/// Read a grammar's declared metadata from the repo `root`. Prefers a modern
/// `tree-sitter.json` (`{ grammars: [ { name, file-types, path } ] }`); falls back
/// to an older `package.json` `tree-sitter` array of the same shape. When neither
/// declares a name, the language is derived from the repo name (a `tree-sitter-`
/// prefix stripped, `-`→`_`), and `file_types` is left empty (no auto-detection —
/// Phase 2 fails loud on that, telling the user to `:set ft=`).
fn read_grammar_metadata(root: &Path, repo: &str) -> GrammarMeta {
    let from_entry = |g: &serde_json::Value| -> GrammarMeta {
        let lang = g
            .get("name")
            .and_then(|n| n.as_str())
            .map(sanitize_lang)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| lang_from_repo(repo));
        let file_types = g
            .get("file-types")
            .and_then(|f| f.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let path = g
            .get("path")
            .and_then(|p| p.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(".")
            .to_string();
        GrammarMeta {
            lang,
            file_types,
            path,
        }
    };

    // `tree-sitter.json` → `grammars: [ … ]`.
    if let Ok(bytes) = std::fs::read(root.join("tree-sitter.json")) {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) {
            if let Some(g) = v
                .get("grammars")
                .and_then(|g| g.as_array())
                .and_then(|a| a.first())
            {
                return from_entry(g);
            }
        }
    }
    // Older `package.json` → `tree-sitter: [ … ]`.
    if let Ok(bytes) = std::fs::read(root.join("package.json")) {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) {
            if let Some(g) = v
                .get("tree-sitter")
                .and_then(|g| g.as_array())
                .and_then(|a| a.first())
            {
                return from_entry(g);
            }
        }
    }

    GrammarMeta {
        lang: lang_from_repo(repo),
        file_types: Vec::new(),
        path: ".".to_string(),
    }
}

/// Derive a language name from a repo name: drop a leading `tree-sitter-` (or
/// `tree-sitter_`) prefix and map `-` → `_` (nvim's convention). `tree-sitter-c-sharp`
/// → `c_sharp`, `btv-lang` → `btv_lang`.
fn lang_from_repo(repo: &str) -> String {
    let stem = repo
        .strip_prefix("tree-sitter-")
        .or_else(|| repo.strip_prefix("tree-sitter_"))
        .unwrap_or(repo);
    sanitize_lang(stem)
}

/// Normalize a declared/derived grammar name into a language token: lowercase and
/// `-` → `_`. (Validity is checked separately by `is_valid_language`.)
fn sanitize_lang(name: &str) -> String {
    name.trim().to_ascii_lowercase().replace('-', "_")
}

/// Copy a repo's own `queries/<name>.scm` (the standard basenames + `textobjects`)
/// into `<data>/queries/<lang>/`, skipping any it doesn't ship. Returns the
/// basenames written.
fn copy_repo_queries(data_dir: &Path, lang: &str, grammar_root: &Path) -> Result<Vec<String>> {
    let src = grammar_root.join("queries");
    let dst = data_dir.join("queries").join(lang);
    std::fs::create_dir_all(&dst).with_context(|| format!("create {}", dst.display()))?;
    let mut written = Vec::new();
    for name in QUERY_FILES.iter().chain(std::iter::once(&"textobjects")) {
        let from = src.join(format!("{name}.scm"));
        if from.exists() {
            std::fs::copy(&from, dst.join(format!("{name}.scm")))
                .with_context(|| format!("copy {name}.scm"))?;
            written.push((*name).to_string());
        }
    }
    Ok(written)
}

/// Fetch nvim-treesitter's `parsers.lua` and pull out `lang`'s `install_info`.
fn resolve_entry(lang: &str) -> Result<ParserEntry> {
    let parsers = fetch(&format!(
        "https://raw.githubusercontent.com/nvim-treesitter/nvim-treesitter/{}/lua/nvim-treesitter/parsers.lua",
        nvim_ts_ref()
    ))?;
    let text = String::from_utf8(parsers).context("parsers.lua is not UTF-8")?;
    parse_entry(&text, lang)
}

/// Extract one grammar's `install_info` from `parsers.lua`. The file is a plain
/// Lua table literal of the shape:
///
/// ```lua
///   rust = {
///     install_info = {
///       revision = '77a3…',
///       url = 'https://github.com/tree-sitter/tree-sitter-rust',
///       location = 'sub/dir',   -- optional
///     },
///     …
///   },
/// ```
///
/// We scan the 2-space-indented `<lang> = {` block (closed by a 2-space `},`) and
/// read the quoted `revision` / `url` / `location` fields. Reordered or extra
/// fields don't matter; a missing required field or a missing entry fails loud.
fn parse_entry(parsers_lua: &str, lang: &str) -> Result<ParserEntry> {
    let header = format!("  {lang} = {{");
    let lines: Vec<&str> = parsers_lua.lines().collect();
    let start = lines
        .iter()
        .position(|l| l.trim_end() == header)
        .with_context(|| format!("nvim-treesitter has no parser named '{lang}'"))?;
    // Block ends at the next 2-space-indented `},`.
    let end = lines[start + 1..]
        .iter()
        .position(|l| l.trim_end() == "  },")
        .map(|i| start + 1 + i)
        .unwrap_or(lines.len());
    let block = &lines[start..end];

    let field = |key: &str| -> Option<String> { block.iter().find_map(|l| extract_quoted(l, key)) };
    let url = field("url").with_context(|| format!("parser '{lang}' has no install_info.url"))?;
    let revision = field("revision")
        .with_context(|| format!("parser '{lang}' has no install_info.revision"))?;
    Ok(ParserEntry {
        url,
        revision,
        location: field("location"),
    })
}

/// If `line` is `key = 'value'` (any indent), return `value`. Single-quoted only,
/// matching nvim-treesitter's style.
fn extract_quoted(line: &str, key: &str) -> Option<String> {
    let t = line.trim_start();
    let rest = t
        .strip_prefix(key)?
        .trim_start()
        .strip_prefix('=')?
        .trim_start();
    let rest = rest.strip_prefix('\'')?;
    let end = rest.find('\'')?;
    Some(rest[..end].to_string())
}

/// Copy the standard queries for `lang` into `<data>/queries/<lang>/` and, following
/// `; inherits:` modelines, the query sets of every inherited language (e.g.
/// `javascript` → `ecma`,`jsx`). The native engine reads one file per language and
/// the query bridge ([`crate::engine::Engine::set_query_overlay`]) merges the inherit
/// chain at runtime, so the inherited files must be on disk too — without them, base
/// highlighting of an inherits-based grammar is missing the parent's patterns.
/// Inherited languages are fetched **query-only** (no parser is built; `ecma` ships
/// none). Returns `(basenames written for lang, inherited languages fetched)`.
fn install_queries(data_dir: &Path, lang: &str) -> Result<(Vec<String>, Vec<String>)> {
    let mut visited = std::collections::HashSet::from([lang.to_string()]);
    let (primary, mut pending) = fetch_query_set(data_dir, lang)?;
    let mut inherited = Vec::new();
    while let Some(l) = pending.pop() {
        if !visited.insert(l.clone()) {
            continue; // already fetched via another inherit edge
        }
        let (written, more) = fetch_query_set(data_dir, &l)?;
        if !written.is_empty() {
            inherited.push(l.clone()); // only report a lang we actually got queries for
        }
        pending.extend(more);
    }
    inherited.sort();
    Ok((primary, inherited))
}

/// Fetch one language's standard query files from the pinned nvim-treesitter ref into
/// `<data>/queries/<lang>/`, skipping any the ref doesn't ship. Returns the basenames
/// written and the languages named in any `; inherits:` modeline across them.
fn fetch_query_set(data_dir: &Path, lang: &str) -> Result<(Vec<String>, Vec<String>)> {
    let dir = data_dir.join("queries").join(lang);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let mut written = Vec::new();
    let mut inherits = Vec::new();
    let mut fetch_one = |url: &str, name: &str| -> Result<()> {
        if let Some(bytes) = fetch_opt(url)? {
            if let Ok(text) = std::str::from_utf8(&bytes) {
                for l in parse_inherits_modeline(text) {
                    if !inherits.contains(&l) {
                        inherits.push(l);
                    }
                }
            }
            std::fs::write(dir.join(format!("{name}.scm")), &bytes)
                .with_context(|| format!("write {name}.scm"))?;
            written.push(name.to_string());
        }
        Ok(())
    };
    for name in QUERY_FILES {
        let url = format!(
            "https://raw.githubusercontent.com/nvim-treesitter/nvim-treesitter/{}/runtime/queries/{lang}/{name}.scm",
            nvim_ts_ref()
        );
        fetch_one(&url, name)?;
    }
    // `textobjects.scm` is not in nvim-treesitter core — fetch it from the separate
    // nvim-treesitter-textobjects repo (different repo *and* path), writing it beside
    // the core queries so the engine reads it off the same `<data>/queries/<lang>/`.
    let to_url = format!(
        "https://raw.githubusercontent.com/nvim-treesitter/nvim-treesitter-textobjects/{}/queries/{lang}/textobjects.scm",
        nvim_ts_textobjects_ref()
    );
    fetch_one(&to_url, "textobjects")?;
    Ok((written, inherits))
}

/// Languages from a query file's leading `; inherits: a,b` modeline(s). Mirrors the
/// server-side resolver's `parse_inherits` so the install fetches exactly the set the
/// bridge merges. Only the leading comment block is scanned (the modeline is
/// conventionally the first line).
fn parse_inherits_modeline(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if !line.starts_with(';') {
            break; // past the leading comment block
        }
        if let Some(rest) = line
            .trim_start_matches(';')
            .trim()
            .strip_prefix("inherits:")
        {
            out.extend(
                rest.split(',')
                    .map(str::trim)
                    .filter(|s| crate::loader::is_valid_language(s))
                    .map(String::from),
            );
        }
    }
    out
}

/// Compile `parser.c` (+ a sibling `scanner.{c,cc,cpp,cxx}` if present) at `src`
/// into the shared object `out`, using compiler argv prefix `cc` (e.g. `["cc"]`
/// or `["zig", "cc"]`).
fn compile(cc: &[String], src_dir: &Path, parser_c: &Path, out: &Path) -> Result<()> {
    let scanner = ["scanner.c", "scanner.cc", "scanner.cpp", "scanner.cxx"]
        .into_iter()
        .map(|f| src_dir.join(f))
        .find(|p| p.exists());
    let cpp_scanner = scanner
        .as_ref()
        .is_some_and(|p| p.extension().is_some_and(|e| e != "c"));

    let (prog, prefix) = cc.split_first().context("empty compiler command")?;
    // We drive Zig as `<zig> cc …` (a `cc` subcommand) — true whether it is the
    // fetched binary (`prog` is a path / `zig.exe`) or a `$BEMTVI_CC="zig cc"`,
    // so key off the subcommand, not the program name.
    let is_zig = prefix.first().is_some_and(|a| a == "cc");
    let mut cmd = Command::new(prog);
    bemtvi_core::process::no_console_window(&mut cmd)
        .args(prefix)
        .args(["-shared", "-fPIC", "-O2"]);
    // On macOS, Zig names the target itself instead of detecting the host: Zig 0.15's
    // detection doesn't know macOS 26, and with it a `-shared` build links no libc, so
    // every grammar whose scanner calls `calloc`/`free` fails with undefined symbols.
    // An explicit target links Zig's bundled libSystem stubs, whatever the SDK.
    if is_zig && cfg!(target_os = "macos") {
        let arch = if cfg!(target_arch = "aarch64") {
            "aarch64"
        } else {
            "x86_64"
        };
        cmd.args(["-target", &format!("{arch}-macos")]);
    }
    cmd.arg("-I").arg(src_dir).arg(parser_c);
    if let Some(s) = &scanner {
        cmd.arg(s);
    }
    cmd.arg("-o").arg(out);
    // A C++ scanner built by a non-zig `cc` needs the C++ runtime linked; `zig cc`
    // links it itself, so only add the flag otherwise.
    if cpp_scanner && !is_zig {
        cmd.arg(if cfg!(target_os = "macos") {
            "-lc++"
        } else {
            "-lstdc++"
        });
    }

    let output = cmd
        .output()
        .with_context(|| format!("run compiler {prog}"))?;
    if !output.status.success() {
        bail!(
            "compiling grammar failed ({prog}):\n{}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Compiler resolution
// ---------------------------------------------------------------------------

/// Resolve a C compiler as an argv prefix, plus a human label: `$BEMTVI_CC` (split
/// on whitespace) when explicitly set, else the pinned Zig (downloaded on demand).
/// PATH is never probed — whatever `cc`/`gcc`/`zig` happens to be installed is
/// unknown (a MinGW `gcc` can't build a loadable MSVC-ABI parser, a stale `zig`
/// differs in flags), so every build uses the one toolchain we pinned and verified.
fn resolve_compiler(data_dir: &Path) -> Result<(Vec<String>, String)> {
    if let Ok(cc) = std::env::var("BEMTVI_CC") {
        if !cc.trim().is_empty() {
            let argv: Vec<String> = cc.split_whitespace().map(str::to_string).collect();
            return Ok((argv, format!("$BEMTVI_CC ({cc})")));
        }
    }
    let zig = ensure_zig(data_dir)?;
    Ok((
        vec![zig.to_string_lossy().into_owned(), "cc".into()],
        "zig (fetched)".to_string(),
    ))
}

/// Make sure `grammar_root/src/parser.c` exists. A committed one is used as is.
/// Otherwise it is generated: by the `tree-sitter` CLI named in
/// `$BEMTVI_TREE_SITTER_CLI` when that is set, else in-process
/// ([`crate::generate`]: `src/grammar.json` if committed, else `grammar.js`
/// evaluated in an embedded QuickJS). `root` is the unpacked source tree, which
/// bounds what `grammar.js` may `require`.
fn ensure_parser_c(grammar_root: &Path, root: &Path, what: &str) -> Result<()> {
    let src_dir = grammar_root.join("src");
    if src_dir.join("parser.c").exists() {
        return Ok(());
    }
    if !grammar_root.join("grammar.js").exists() && !src_dir.join("grammar.json").exists() {
        bail!(
            "{what} ships neither src/parser.c, src/grammar.json nor grammar.js at {} — \
             nothing to build",
            grammar_root.display()
        );
    }
    let Some(cli) = tree_sitter_cli() else {
        return crate::generate::generate(grammar_root, root).with_context(|| {
            format!("{what} ships no generated src/parser.c; generating it failed")
        });
    };
    let mut cmd = Command::new(&cli);
    cmd.arg("generate");
    // Without an argument the CLI reads `grammar.js`; a repo that commits only
    // `src/grammar.json` has to be named.
    if !grammar_root.join("grammar.js").exists() {
        cmd.arg(src_dir.join("grammar.json"));
    }
    let output = bemtvi_core::process::no_console_window(&mut cmd)
        .current_dir(grammar_root)
        .output()
        .with_context(|| format!("run `{cli} generate` in {}", grammar_root.display()))?;
    if !output.status.success() {
        bail!(
            "`{cli} generate` ($BEMTVI_TREE_SITTER_CLI) failed in {}:\n{}",
            grammar_root.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    if !src_dir.join("parser.c").exists() {
        bail!(
            "`{cli} generate` ran but produced no src/parser.c in {}",
            grammar_root.display()
        );
    }
    Ok(())
}

/// The `tree-sitter` CLI `$BEMTVI_TREE_SITTER_CLI` names (a path or program name), or
/// `None` to generate in-process. A CLI on `PATH` is not picked up on its own: the
/// in-process generator is the default everywhere, so an install doesn't depend on
/// what happens to be installed.
fn tree_sitter_cli() -> Option<String> {
    std::env::var("BEMTVI_TREE_SITTER_CLI")
        .ok()
        .filter(|p| !p.trim().is_empty())
}

// ---------------------------------------------------------------------------
// Zig toolchain fetch
// ---------------------------------------------------------------------------

/// Pinned Zig archive name + SHA-256 for the host target, or `Err` on an
/// unsupported platform. macOS/Linux ship a `.tar.xz`, Windows a `.zip`;
/// [`ensure_zig`] dispatches the extractor on the extension. Bump the shas
/// together with [`zig_version!`] (they come from `ziglang.org`'s
/// `download/index.json`).
fn zig_target() -> Result<(&'static str, &'static str)> {
    let t = match (std::env::consts::ARCH, std::env::consts::OS) {
        ("aarch64", "macos") => (
            concat!("zig-aarch64-macos-", zig_version!(), ".tar.xz"),
            "75f1bc7a1e9852f17ec9a0c1895f767c4ec56d46bc8a834920eece1d0359dd8d",
        ),
        ("x86_64", "macos") => (
            concat!("zig-x86_64-macos-", zig_version!(), ".tar.xz"),
            "9af07b20319fbd0c92ee46ad23e81d6a782b1aaea2cfb8dcbff35f1d2c7ddbb7",
        ),
        ("x86_64", "linux") => (
            concat!("zig-x86_64-linux-", zig_version!(), ".tar.xz"),
            "3581863e3cbcfd0fce0efc682ce7e9e93da96668a977791d356a26d2a7e9b17a",
        ),
        ("aarch64", "linux") => (
            concat!("zig-aarch64-linux-", zig_version!(), ".tar.xz"),
            "a1546d5646eb184437918d6635ca6b762a0e889eac09cef720ee7a1ed3fe7795",
        ),
        ("x86_64", "windows") => (
            concat!("zig-x86_64-windows-", zig_version!(), ".zip"),
            "b03f477a1943673e02aca0f570e8030dffd536ce1f45cbc4d3326ec31159562e",
        ),
        ("aarch64", "windows") => (
            concat!("zig-aarch64-windows-", zig_version!(), ".zip"),
            "4789895bdb1e312a2ea73b2a7c0a0cc8d2ab62ba6976e834a7c54cf443377c15",
        ),
        (arch, os) => bail!(
            "no pinned Zig for {arch}-{os}; set $BEMTVI_CC to a C compiler \
             (e.g. `cc` or `zig cc`)"
        ),
    };
    Ok(t)
}

/// Ensure a pinned Zig is unpacked under `<data>/zig/<version>/` and return the
/// path to its `zig` binary, downloading + checksum-verifying it on first use.
/// Both archive shapes unpack to a single `<stem>/` dir holding `zig`
/// (`zig.exe` on Windows).
///
/// Public so the test suites can build their grammar fixtures with the same
/// pinned toolchain on hosts with no system `cc` (Windows).
pub fn ensure_zig(data_dir: &Path) -> Result<PathBuf> {
    let (archive, sha) = zig_target()?;
    let is_zip = archive.ends_with(".zip");
    let stem = archive
        .strip_suffix(".tar.xz")
        .or_else(|| archive.strip_suffix(".zip"))
        .context("zig archive name")?;
    let root = data_dir.join("zig").join(ZIG_VERSION);
    let bin = root
        .join(stem)
        .join(format!("zig{}", std::env::consts::EXE_SUFFIX));
    if bin.exists() {
        return Ok(bin);
    }

    let url = format!("https://ziglang.org/download/{ZIG_VERSION}/{archive}");
    let bytes = http_get(&url).with_context(|| format!("download {url}"))?;
    verify_sha256(&bytes, sha).context("Zig download failed checksum verification")?;

    std::fs::create_dir_all(&root).with_context(|| format!("create {}", root.display()))?;
    if is_zip {
        unpack_zig_zip(&bytes, &root)
    } else {
        let xz = xz2::read::XzDecoder::new(&bytes[..]);
        tar::Archive::new(xz).unpack(&root).map_err(Into::into)
    }
    .with_context(|| format!("unpack Zig into {}", root.display()))?;
    if !bin.exists() {
        bail!(
            "Zig archive did not contain the expected binary at {}",
            bin.display()
        );
    }
    Ok(bin)
}

/// Unpack the Windows Zig `.zip` into `dest`, preserving the archive's directory
/// layout and re-applying each entry's Unix mode (the `zig.exe` bit is moot on
/// Windows but we honor it for parity with the tar path). Entry names are checked
/// to stay under `dest` (no `..`/absolute escape) before anything is written.
fn unpack_zig_zip(bytes: &[u8], dest: &Path) -> Result<()> {
    use std::io::Cursor;
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).context("read Zig zip")?;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).with_context(|| format!("zip entry {i}"))?;
        let rel = entry.enclosed_name().with_context(|| {
            let name = entry
                .name()
                .map_or_else(|e| format!("<{e}>"), |n| n.into_owned());
            format!("unsafe path in Zig zip: {name}")
        })?;
        let out = dest.join(rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&out).with_context(|| format!("mkdir {}", out.display()))?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("mkdir {}", parent.display()))?;
        }
        let mut file =
            std::fs::File::create(&out).with_context(|| format!("create {}", out.display()))?;
        std::io::copy(&mut entry, &mut file).with_context(|| format!("write {}", out.display()))?;
        #[cfg(unix)]
        if let Some(mode) = entry.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&out, std::fs::Permissions::from_mode(mode))
                .with_context(|| format!("chmod {}", out.display()))?;
        }
    }
    Ok(())
}

/// Verify `bytes` hash to the hex `expected` SHA-256, else fail loud.
fn verify_sha256(bytes: &[u8], expected: &str) -> Result<()> {
    use sha2::{Digest, Sha256};
    let got = Sha256::digest(bytes);
    let got_hex = got.iter().map(|b| format!("{b:02x}")).collect::<String>();
    if got_hex != expected {
        bail!("sha256 mismatch: expected {expected}, got {got_hex}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// HTTP + archive helpers
// ---------------------------------------------------------------------------

/// nvim-treesitter ref to read (pinned [`NVIM_TS_REF`], overridable for testing /
/// pinning a different snapshot via `$BEMTVI_TS_REF`).
fn nvim_ts_ref() -> String {
    std::env::var("BEMTVI_TS_REF").unwrap_or_else(|_| NVIM_TS_REF.to_string())
}

/// nvim-treesitter-textobjects ref to read `textobjects.scm` from (pinned
/// [`NVIM_TS_TEXTOBJECTS_REF`], overridable via `$BEMTVI_TS_TEXTOBJECTS_REF`).
fn nvim_ts_textobjects_ref() -> String {
    std::env::var("BEMTVI_TS_TEXTOBJECTS_REF")
        .unwrap_or_else(|_| NVIM_TS_TEXTOBJECTS_REF.to_string())
}

/// GET `url` into bytes, failing on any non-success status. With `$BEMTVI_TS_MIRROR`
/// set, the URL is served from `<mirror>/<host>/<path…>` on disk instead — the
/// seam that lets the black-box tests run the whole install offline.
fn fetch(url: &str) -> Result<Vec<u8>> {
    fetch_opt(url)?.with_context(|| format!("not found: {url}"))
}

/// Like [`fetch`] but `Ok(None)` when the resource is absent (HTTP 404, or a
/// missing mirror file) — used for optional query files.
fn fetch_opt(url: &str) -> Result<Option<Vec<u8>>> {
    if let Ok(mirror) = std::env::var("BEMTVI_TS_MIRROR") {
        let rel = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
        let path = Path::new(&mirror).join(rel);
        return match std::fs::read(&path) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("mirror read {}", path.display())),
        };
    }
    match ureq::get(url).call() {
        Ok(resp) => {
            let mut buf = Vec::new();
            resp.into_body()
                .into_reader()
                .read_to_end(&mut buf)
                .with_context(|| format!("read body of {url}"))?;
            Ok(Some(buf))
        }
        Err(ureq::Error::StatusCode(404)) => Ok(None),
        Err(e) => Err(anyhow!("GET {url}: {e}")),
    }
}

/// Real-network GET (no mirror), used for the Zig download.
fn http_get(url: &str) -> Result<Vec<u8>> {
    let resp = ureq::get(url)
        .call()
        .map_err(|e| anyhow!("GET {url}: {e}"))?;
    let mut buf = Vec::new();
    resp.into_body().into_reader().read_to_end(&mut buf)?;
    Ok(buf)
}

/// The single sub-directory of `dir` (a freshly-unpacked GitHub source archive
/// always has exactly one top-level dir). Errors if there are zero or several, so
/// a malformed archive fails loud rather than building from the wrong place.
fn single_subdir(dir: &Path) -> Result<PathBuf> {
    let mut dirs = std::fs::read_dir(dir)
        .with_context(|| format!("read {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir());
    let first = dirs.next().context("archive contained no directory")?;
    if dirs.next().is_some() {
        bail!("archive had more than one top-level directory");
    }
    Ok(first)
}

/// Unpack a gzip-compressed tarball into `dest`.
fn unpack_tar_gz(bytes: &[u8], dest: &Path) -> Result<()> {
    let gz = flate2::read::GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(gz);
    let entries = archive
        .entries()
        .with_context(|| format!("read tar.gz for {}", dest.display()))?;
    for entry in entries {
        let mut entry = entry.context("read a tar entry")?;
        // `tar` keeps hardlinks and entry paths inside `dest`, but creates a symlink to
        // any target. A grammar's source is untrusted, and the build writes into it
        // (a generated `src/parser.c`) and reads from it: a `src` symlink to a directory
        // elsewhere would have both happen there. So a symlink must resolve inside the
        // archive.
        if entry.header().entry_type().is_symlink() {
            let path = entry
                .path()
                .context("read a tar entry's path")?
                .into_owned();
            let target = entry
                .link_name()
                .context("read a symlink's target")?
                .with_context(|| format!("symlink {} has no target", path.display()))?
                .into_owned();
            if !symlink_stays_inside(&path, &target) {
                bail!(
                    "the archive's symlink {} -> {} points outside it; refusing to unpack",
                    path.display(),
                    target.display()
                );
            }
        }
        entry
            .unpack_in(dest)
            .with_context(|| format!("unpack into {}", dest.display()))?;
    }
    // The check above reads each link's target as text, which can't see through
    // another link: `a/l -> ..` is fine alone, but `src -> a/l/../x` climbs one
    // level further on disk than it does lexically. So once everything is on disk,
    // resolve every symlink for real and refuse one that lands outside `dest`.
    let root =
        std::fs::canonicalize(dest).with_context(|| format!("resolve {}", dest.display()))?;
    check_symlinks_inside(dest, &root)
}

/// Walk `dir` (not following symlinks) and fail on any symlink that resolves outside
/// `root`. A dangling one is removed: it can't be read, and writing through it (a
/// generated `src/parser.c`) would create its target, wherever that is.
fn check_symlinks_inside(dir: &Path, root: &Path) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let path = entry?.path();
        let meta =
            std::fs::symlink_metadata(&path).with_context(|| format!("stat {}", path.display()))?;
        if meta.file_type().is_symlink() {
            match std::fs::canonicalize(&path) {
                Ok(real) if real.starts_with(root) => {}
                Ok(real) => bail!(
                    "the archive's symlink {} resolves to {}, outside it; refusing to unpack",
                    path.display(),
                    real.display()
                ),
                Err(_) => std::fs::remove_file(&path)
                    .with_context(|| format!("remove dangling symlink {}", path.display()))?,
            }
        } else if meta.is_dir() {
            check_symlinks_inside(&path, root)?;
        }
    }
    Ok(())
}

/// Whether a symlink at archive path `path` pointing at `target` resolves inside the
/// archive: `target` is relative, and walking it from `path`'s directory never climbs
/// above the archive root.
fn symlink_stays_inside(path: &Path, target: &Path) -> bool {
    use std::path::Component;
    let mut depth: Vec<&std::ffi::OsStr> = Vec::new();
    let parent = path.parent().unwrap_or(Path::new(""));
    for c in parent.components().chain(target.components()) {
        match c {
            Component::Normal(name) => depth.push(name),
            Component::CurDir => {}
            Component::ParentDir => {
                if depth.pop().is_none() {
                    return false;
                }
            }
            Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    true
}

/// Split `https://github.com/<owner>/<repo>` into `(owner, repo)`, tolerating a
/// trailing `.git` or `/`.
fn github_owner_repo(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("https://github.com/")?;
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let mut it = rest.splitn(2, '/');
    let owner = it.next()?.to_string();
    let repo = it.next()?.to_string();
    if owner.is_empty() || repo.is_empty() || repo.contains('/') {
        return None;
    }
    Some((owner, repo))
}
