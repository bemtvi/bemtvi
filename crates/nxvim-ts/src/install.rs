//! `:TSInstall <lang>` — fetch a treesitter grammar's source, compile it to a
//! loadable parser, and drop the matching queries into the data dir, all
//! in-process (a prebuilt nxvim needs no external `curl`/`git`/`tar`).
//!
//! Source of truth is **nvim-treesitter**, pinned to one commit ([`NVIM_TS_REF`])
//! so installs are reproducible: that commit's `parsers.lua` names every grammar
//! repo + the exact revision to build, and its `runtime/queries/<lang>/` holds the
//! queries matched to that revision. The flow for `install(data_dir, lang)`:
//!
//!   1. fetch `parsers.lua`, extract the `<lang>` `install_info` (url + revision),
//!   2. download the grammar repo tarball at that revision and unpack it,
//!   3. compile `src/parser.c` (+ a `scanner.{c,cc}` if present) into
//!      `<data>/parser/<lang>.so`, using a resolved C compiler,
//!   4. copy the standard queries into `<data>/queries/<lang>/`.
//!
//! The compiler is resolved in [`resolve_compiler`]: `$NXVIM_CC` wins, else a
//! system `cc`/`clang`/`gcc`/`zig`, else a **pinned Zig** is fetched and
//! checksum-verified ([`ensure_zig`]) so the user needs no toolchain at all.
//!
//! Everything fails *loud* (no silent stub ever leaves a half-installed grammar
//! that "loads but is wrong"). Tests avoid the network via `$NXVIM_TS_MIRROR`,
//! which redirects every HTTP GET to a local directory tree (see [`fetch`]), and
//! `$NXVIM_CC` to pin the compiler.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};

/// nvim-treesitter commit the install reads from — `parsers.lua` (grammar repos +
/// pinned revisions) and `runtime/queries/`. Pinning the ref pins every grammar
/// transitively. Override with `$NXVIM_TS_REF` (a branch, tag, or sha).
const NVIM_TS_REF: &str = "4916d6592ede8c07973490d9322f187e07dfefac";

/// nvim-treesitter-textobjects commit the install reads `textobjects.scm` from.
/// The textobjects queries live in this **separate** repo (nvim-treesitter core
/// ships none), under `queries/<lang>/textobjects.scm` — a different path than the
/// core repo's `runtime/queries/`. Pinned; override with `$NXVIM_TS_TEXTOBJECTS_REF`.
const NVIM_TS_TEXTOBJECTS_REF: &str = "898ee307df58f854d11cd7edd06472574d48014e";

/// Zig version fetched when no system compiler is found. The per-target archive
/// + SHA-256 are pinned in [`zig_target`]; bump all three together.
const ZIG_VERSION: &str = "0.15.2";

/// The standard nvim-treesitter query files, in the order we copy them. Missing
/// ones (a 404) are simply skipped — not every grammar ships every query.
const QUERY_FILES: &[&str] = &["highlights", "indents", "injections", "folds", "locals"];

/// What [`install`] resolved and produced — surfaced to the user by `:TSInstall`.
#[derive(Debug, Clone)]
pub struct InstallReport {
    pub lang: String,
    /// The grammar revision (from nvim-treesitter's `parsers.lua`) that was built.
    pub revision: String,
    /// The compiled parser written under `<data>/parser/`.
    pub parser: PathBuf,
    /// Query file basenames copied into `<data>/queries/<lang>/` (e.g. `indents`).
    pub queries: Vec<String>,
    /// Inherited languages whose query sets were also fetched (query-only, no
    /// parser) by following `; inherits:` modelines — e.g. `["ecma", "jsx"]` for
    /// `javascript`. Empty for a self-contained grammar.
    pub inherited: Vec<String>,
    /// Human description of the compiler used (`$NXVIM_CC`, `cc`, `zig (fetched)`…).
    pub compiler: String,
}

/// The `install_info` we need out of `parsers.lua`.
struct ParserEntry {
    url: String,
    revision: String,
    /// Subdirectory of the repo holding `src/` (the `location` field), if any.
    location: Option<String>,
}

/// Install grammar `lang` into `data_dir`. See the module docs for the flow.
pub fn install(data_dir: &Path, lang: &str) -> Result<InstallReport> {
    if !crate::loader::is_valid_language(lang) {
        bail!("invalid language name '{lang}' (letters, digits, '_' and '-' only)");
    }

    let entry = resolve_entry(lang)?;
    let (owner, repo) = github_owner_repo(&entry.url)
        .with_context(|| format!("grammar url is not a GitHub repo: {}", entry.url))?;

    // 1. Download + unpack the grammar source at the pinned revision.
    let build_dir = data_dir.join(".ts-build").join(lang);
    let _ = std::fs::remove_dir_all(&build_dir);
    std::fs::create_dir_all(&build_dir)
        .with_context(|| format!("create build dir {}", build_dir.display()))?;
    let tarball = fetch(&format!(
        "https://github.com/{owner}/{repo}/archive/{}.tar.gz",
        entry.revision
    ))?;
    unpack_tar_gz(&tarball, &build_dir).with_context(|| format!("unpack {repo} source tarball"))?;
    // The archive's single top-level directory — *discovered*, not computed.
    // GitHub names it `<repo>-<ref>` but mangles `<ref>`: a version-like tag
    // `v0.25.0` becomes `…-0.25.0` (leading `v` stripped), so `{repo}-{revision}`
    // would miss it. Reading the one extracted dir is robust to tag vs sha.
    let mut src_root = single_subdir(&build_dir).with_context(|| {
        format!(
            "locating unpacked {repo} source under {}",
            build_dir.display()
        )
    })?;
    if let Some(loc) = &entry.location {
        src_root = src_root.join(loc);
    }
    let src_dir = src_root.join("src");
    let parser_c = src_dir.join("parser.c");
    if !parser_c.exists() {
        bail!(
            "grammar '{lang}' has no src/parser.c at {} — it likely needs \
             `tree-sitter generate`, which nxvim can't do; install it with the \
             nvim-treesitter CLI instead",
            src_dir.display()
        );
    }

    // 2. Resolve a compiler and build the shared object.
    let (cc, compiler_desc) = resolve_compiler(data_dir)?;
    let parser_dir = data_dir.join("parser");
    std::fs::create_dir_all(&parser_dir)
        .with_context(|| format!("create {}", parser_dir.display()))?;
    let out = parser_dir.join(format!("{lang}.so"));
    compile(&cc, &src_dir, &parser_c, &out)?;

    // 3. Copy queries matched to this revision (plus inherited query sets).
    let (queries, inherited) = install_queries(data_dir, lang)?;

    // Best-effort cleanup of the unpacked source; the parser is self-contained.
    let _ = std::fs::remove_dir_all(&build_dir);

    Ok(InstallReport {
        lang: lang.to_string(),
        revision: entry.revision,
        parser: out,
        queries,
        inherited,
        compiler: compiler_desc,
    })
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
    for name in QUERY_FILES {
        let url = format!(
            "https://raw.githubusercontent.com/nvim-treesitter/nvim-treesitter/{}/runtime/queries/{lang}/{name}.scm",
            nvim_ts_ref()
        );
        if let Some(bytes) = fetch_opt(&url)? {
            if let Ok(text) = std::str::from_utf8(&bytes) {
                for l in parse_inherits_modeline(text) {
                    if !inherits.contains(&l) {
                        inherits.push(l);
                    }
                }
            }
            std::fs::write(dir.join(format!("{name}.scm")), &bytes)
                .with_context(|| format!("write {name}.scm"))?;
            written.push((*name).to_string());
        }
    }
    // `textobjects.scm` is not in nvim-treesitter core — fetch it from the separate
    // nvim-treesitter-textobjects repo (different repo *and* path), writing it beside
    // the core queries so the engine reads it off the same `<data>/queries/<lang>/`.
    let to_url = format!(
        "https://raw.githubusercontent.com/nvim-treesitter/nvim-treesitter-textobjects/{}/queries/{lang}/textobjects.scm",
        nvim_ts_textobjects_ref()
    );
    if let Some(bytes) = fetch_opt(&to_url)? {
        if let Ok(text) = std::str::from_utf8(&bytes) {
            for l in parse_inherits_modeline(text) {
                if !inherits.contains(&l) {
                    inherits.push(l);
                }
            }
        }
        std::fs::write(dir.join("textobjects.scm"), &bytes)
            .with_context(|| "write textobjects.scm".to_string())?;
        written.push("textobjects".to_string());
    }
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
                    .filter(|s| !s.is_empty())
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
    // We drive Zig as `<zig> cc …` (a `cc` subcommand) — true whether Zig is on
    // PATH (`prog == "zig"`) or a fetched binary (`prog` is a path / `zig.exe`),
    // so key off the subcommand, not the program name.
    let is_zig = prefix.first().is_some_and(|a| a == "cc");
    let mut cmd = Command::new(prog);
    cmd.args(prefix)
        .args(["-shared", "-fPIC", "-O2"])
        .arg("-I")
        .arg(src_dir)
        .arg(parser_c);
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

/// Resolve a C compiler as an argv prefix, plus a human label. Order: `$NXVIM_CC`
/// (split on whitespace), then a system `cc`/`clang`/`gcc`, then a system `zig`,
/// then a fetched pinned Zig (downloaded on demand) so the user needs nothing.
fn resolve_compiler(data_dir: &Path) -> Result<(Vec<String>, String)> {
    if let Ok(cc) = std::env::var("NXVIM_CC") {
        if !cc.trim().is_empty() {
            let argv: Vec<String> = cc.split_whitespace().map(str::to_string).collect();
            return Ok((argv, format!("$NXVIM_CC ({cc})")));
        }
    }
    for c in ["cc", "clang", "gcc"] {
        if program_exists(c, &[]) {
            return Ok((vec![c.to_string()], c.to_string()));
        }
    }
    if program_exists("zig", &["version"]) {
        return Ok((vec!["zig".into(), "cc".into()], "zig (system)".to_string()));
    }
    let zig = ensure_zig(data_dir)?;
    Ok((
        vec![zig.to_string_lossy().into_owned(), "cc".into()],
        "zig (fetched)".to_string(),
    ))
}

/// Whether running `name <probe…>` succeeds (the compiler/toolchain is present).
/// `--version` for the cc family, `version` for zig.
fn program_exists(name: &str, args: &[&str]) -> bool {
    let probe = if args.is_empty() {
        &["--version"][..]
    } else {
        args
    };
    Command::new(name)
        .args(probe)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Zig toolchain fetch
// ---------------------------------------------------------------------------

/// Pinned Zig archive name + SHA-256 for the host target, or `Err` on an
/// unsupported platform. macOS/Linux ship a `.tar.xz`, Windows a `.zip`;
/// [`ensure_zig`] dispatches the extractor on the extension. Bump all the names +
/// shas together with [`ZIG_VERSION`] (they come from `ziglang.org`'s
/// `download/index.json`).
fn zig_target() -> Result<(&'static str, &'static str)> {
    let t = match (std::env::consts::ARCH, std::env::consts::OS) {
        ("aarch64", "macos") => (
            "zig-aarch64-macos-0.15.2.tar.xz",
            "3cc2bab367e185cdfb27501c4b30b1b0653c28d9f73df8dc91488e66ece5fa6b",
        ),
        ("x86_64", "macos") => (
            "zig-x86_64-macos-0.15.2.tar.xz",
            "375b6909fc1495d16fc2c7db9538f707456bfc3373b14ee83fdd3e22b3d43f7f",
        ),
        ("x86_64", "linux") => (
            "zig-x86_64-linux-0.15.2.tar.xz",
            "02aa270f183da276e5b5920b1dac44a63f1a49e55050ebde3aecc9eb82f93239",
        ),
        ("aarch64", "linux") => (
            "zig-aarch64-linux-0.15.2.tar.xz",
            "958ed7d1e00d0ea76590d27666efbf7a932281b3d7ba0c6b01b0ff26498f667f",
        ),
        ("x86_64", "windows") => (
            "zig-x86_64-windows-0.15.2.zip",
            "3a0ed1e8799a2f8ce2a6e6290a9ff22e6906f8227865911fb7ddedc3cc14cb0c",
        ),
        ("aarch64", "windows") => (
            "zig-aarch64-windows-0.15.2.zip",
            "b926465f8872bf983422257cd9ec248bb2b270996fbe8d57872cca13b56fc370",
        ),
        (arch, os) => bail!(
            "no pinned Zig for {arch}-{os}; install a C compiler (cc/clang/gcc) \
             or set $NXVIM_CC"
        ),
    };
    Ok(t)
}

/// Ensure a pinned Zig is unpacked under `<data>/zig/<version>/` and return the
/// path to its `zig` binary, downloading + checksum-verifying it on first use.
/// Both archive shapes unpack to a single `<stem>/` dir holding `zig`
/// (`zig.exe` on Windows).
fn ensure_zig(data_dir: &Path) -> Result<PathBuf> {
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
        let rel = entry
            .enclosed_name()
            .with_context(|| format!("unsafe path in Zig zip: {}", entry.name()))?;
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
/// pinning a different snapshot via `$NXVIM_TS_REF`).
fn nvim_ts_ref() -> String {
    std::env::var("NXVIM_TS_REF").unwrap_or_else(|_| NVIM_TS_REF.to_string())
}

/// nvim-treesitter-textobjects ref to read `textobjects.scm` from (pinned
/// [`NVIM_TS_TEXTOBJECTS_REF`], overridable via `$NXVIM_TS_TEXTOBJECTS_REF`).
fn nvim_ts_textobjects_ref() -> String {
    std::env::var("NXVIM_TS_TEXTOBJECTS_REF")
        .unwrap_or_else(|_| NVIM_TS_TEXTOBJECTS_REF.to_string())
}

/// GET `url` into bytes, failing on any non-success status. With `$NXVIM_TS_MIRROR`
/// set, the URL is served from `<mirror>/<host>/<path…>` on disk instead — the
/// seam that lets the black-box tests run the whole install offline.
fn fetch(url: &str) -> Result<Vec<u8>> {
    fetch_opt(url)?.with_context(|| format!("not found: {url}"))
}

/// Like [`fetch`] but `Ok(None)` when the resource is absent (HTTP 404, or a
/// missing mirror file) — used for optional query files.
fn fetch_opt(url: &str) -> Result<Option<Vec<u8>>> {
    if let Ok(mirror) = std::env::var("NXVIM_TS_MIRROR") {
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
            resp.into_reader()
                .read_to_end(&mut buf)
                .with_context(|| format!("read body of {url}"))?;
            Ok(Some(buf))
        }
        Err(ureq::Error::Status(404, _)) => Ok(None),
        Err(e) => Err(anyhow!("GET {url}: {e}")),
    }
}

/// Real-network GET (no mirror), used for the Zig download.
fn http_get(url: &str) -> Result<Vec<u8>> {
    let resp = ureq::get(url)
        .call()
        .map_err(|e| anyhow!("GET {url}: {e}"))?;
    let mut buf = Vec::new();
    resp.into_reader().read_to_end(&mut buf)?;
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
    tar::Archive::new(gz)
        .unpack(dest)
        .with_context(|| format!("unpack tar.gz into {}", dest.display()))?;
    Ok(())
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
