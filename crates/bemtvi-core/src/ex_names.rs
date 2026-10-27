//! Every built-in ex command, by its canonical name, with the spellings it answers to.
//!
//! The one list of command spellings. The core's dispatch (`Editor::execute_ex`) and the
//! server's (`resolve_command`) both match on the canonical name [`canonical`] gives them,
//! so a spelling that isn't here doesn't dispatch at all: anything keyed on a command
//! (the disabled set an embedding app gives the editor) can't be got around with another
//! spelling of it. Commands that behave the same share a canonical name (`:tabnew` and
//! `:tabedit`, `:bdelete` and `:bwipeout`). A name not here (a user command, the `Lsp*`
//! and `TS*` commands, which have one spelling each) is its own canonical name.
//!
//! It also has how a file name is spelled as a command's argument ([`escape_file_arg`]),
//! for a command line built from a path rather than typed.

use std::collections::HashMap;
use std::sync::OnceLock;

/// `(canonical name, every spelling of it)`. The core's commands, then the server's, then
/// the ones the core recognizes before splitting a name off (`:normal`, `:&`, `:=`, `:s`).
const COMMANDS: &[(&str, &[&str])] = &[
    // The core (`editor/ex.rs`).
    ("write", &["w", "write"]),
    ("quit", &["q", "quit"]),
    ("wq", &["wq", "x", "xit", "exit"]),
    ("quitall", &["qa", "qall", "quita", "quitall"]),
    ("wall", &["wa", "wall"]),
    ("xall", &["wqa", "xa", "xall"]),
    ("edit", &["e", "edit"]),
    ("enew", &["ene", "enew"]),
    ("split", &["sp", "spl", "split"]),
    ("vsplit", &["vs", "vsp", "vsplit", "vspl"]),
    ("new", &["new"]),
    ("vnew", &["vne", "vnew"]),
    ("close", &["clo", "close"]),
    ("hide", &["hid", "hide"]),
    ("only", &["on", "only"]),
    (
        "terminal",
        &["ter", "term", "termi", "termin", "termina", "terminal"],
    ),
    ("helix", &["helix"]),
    ("tabedit", &["tabnew", "tabe", "tabed", "tabedit"]),
    ("tabclose", &["tabc", "tabclo", "tabclose"]),
    ("tabonly", &["tabo", "tabonly"]),
    ("tabmove", &["tabm", "tabmo", "tabmove"]),
    ("tabnext", &["tabn", "tabnext"]),
    ("tabprevious", &["tabp", "tabN", "tabprevious", "tabNext"]),
    ("tabfirst", &["tabfir", "tabfirst", "tabr", "tabrewind"]),
    ("tablast", &["tabl", "tablast"]),
    ("tab", &["tab"]),
    ("drop", &["dr", "dro", "drop"]),
    ("resize", &["res", "resize"]),
    ("vertical", &["vert", "vertical", "ver"]),
    ("checktime", &["checkt", "checkti", "checktim", "checktime"]),
    ("buffers", &["ls", "buffers", "files"]),
    ("lspanels", &["lspanels", "panels"]),
    ("buffer", &["b", "bu", "buf", "buffer"]),
    ("bnext", &["bn", "bnext"]),
    ("bprevious", &["bp", "bN", "bprev", "bprevious", "bNext"]),
    ("bfirst", &["bf", "bfirst", "br", "brewind"]),
    ("blast", &["bl", "blast"]),
    (
        "bdelete",
        &["bd", "bdel", "bdelete", "bw", "bwipe", "bwipeout"],
    ),
    (
        "cbuffer",
        &[
            "cb",
            "cbu",
            "cbuf",
            "cbuff",
            "cbuffe",
            "cbuffer",
            "cgetb",
            "cgetbu",
            "cgetbuf",
            "cgetbuff",
            "cgetbuffe",
            "cgetbuffer",
        ],
    ),
    (
        "caddbuffer",
        &[
            "cad",
            "cadd",
            "caddb",
            "caddbu",
            "caddbuf",
            "caddbuff",
            "caddbuffe",
            "caddbuffer",
        ],
    ),
    (
        "lbuffer",
        &[
            "lb",
            "lbu",
            "lbuf",
            "lbuff",
            "lbuffe",
            "lbuffer",
            "lgetb",
            "lgetbu",
            "lgetbuf",
            "lgetbuff",
            "lgetbuffe",
            "lgetbuffer",
        ],
    ),
    (
        "laddbuffer",
        &[
            "lad",
            "ladd",
            "laddb",
            "laddbu",
            "laddbuf",
            "laddbuff",
            "laddbuffe",
            "laddbuffer",
        ],
    ),
    ("cfile", &["cf", "cfi", "cfil", "cfile"]),
    ("cgetfile", &["cgetf", "cgetfi", "cgetfil", "cgetfile"]),
    ("caddfile", &["caddf", "caddfi", "caddfil", "caddfile"]),
    ("lfile", &["lf", "lfi", "lfil", "lfile"]),
    ("lgetfile", &["lgetf", "lgetfi", "lgetfil", "lgetfile"]),
    ("laddfile", &["laddf", "laddfi", "laddfil", "laddfile"]),
    ("copen", &["cope", "copen"]),
    ("cclose", &["ccl", "cclo", "cclos", "cclose"]),
    ("cwindow", &["cw", "cwin", "cwindow"]),
    ("cc", &["cc"]),
    ("cnext", &["cn", "cne", "cnex", "cnext"]),
    (
        "cprevious",
        &["cp", "cpr", "cprev", "cprevious", "cN", "cNext"],
    ),
    ("cfirst", &["cfir", "cfirst", "cr", "crewind"]),
    ("clast", &["cla", "clast"]),
    ("cnfile", &["cnf", "cnfi", "cnfil", "cnfile"]),
    ("cpfile", &["cpf", "cpfi", "cpfil", "cpfile"]),
    ("colder", &["col", "cold", "colde", "colder"]),
    ("cnewer", &["cnew", "cnewe", "cnewer"]),
    ("lopen", &["lop", "lope", "lopen"]),
    ("lclose", &["lcl", "lclo", "lclos", "lclose"]),
    ("lwindow", &["lw", "lwin", "lwindow"]),
    ("ll", &["ll"]),
    ("lnext", &["lne", "lnex", "lnext"]),
    (
        "lprevious",
        &["lp", "lpr", "lprev", "lprevious", "lN", "lNext"],
    ),
    ("lfirst", &["lfir", "lfirst", "lr", "lrewind"]),
    ("llast", &["lla", "llast"]),
    ("lnfile", &["lnf", "lnfi", "lnfil", "lnfile"]),
    ("lpfile", &["lpf", "lpfi", "lpfil", "lpfile"]),
    ("lolder", &["lol", "lold", "lolde", "lolder"]),
    ("lnewer", &["lnew", "lnewe", "lnewer"]),
    ("vimgrep", &["vim", "vimg", "vimgr", "vimgre", "vimgrep"]),
    ("vimgrepadd", &["vimgrepa", "vimgrepad", "vimgrepadd"]),
    (
        "lvimgrep",
        &["lvim", "lvimg", "lvimgr", "lvimgre", "lvimgrep"],
    ),
    ("lvimgrepadd", &["lvimgrepa", "lvimgrepad", "lvimgrepadd"]),
    ("lua", &["lua"]),
    ("sleep", &["sleep", "sl"]),
    ("messages", &["mes", "messages", "message"]),
    ("echo", &["ec", "ech", "echo"]),
    ("echomsg", &["echom", "echoms", "echomsg"]),
    ("echoerr", &["echoe", "echoer", "echoerr"]),
    ("registers", &["reg", "registers", "di", "dis", "display"]),
    ("marks", &["marks"]),
    ("jumps", &["ju", "jum", "jump", "jumps"]),
    ("changes", &["changes"]),
    ("set", &["set", "se"]),
    (
        "setlocal",
        &["setl", "setlo", "setloc", "setloca", "setlocal"],
    ),
    (
        "setglobal",
        &[
            "setg",
            "setgl",
            "setglo",
            "setglob",
            "setgloba",
            "setglobal",
        ],
    ),
    (
        "setfiletype",
        &[
            "setf",
            "setfi",
            "setfil",
            "setfile",
            "setfilet",
            "setfilety",
            "setfiletyp",
            "setfiletype",
        ],
    ),
    ("undo", &["u", "un", "und", "undo"]),
    ("redo", &["red", "redo"]),
    ("undolist", &["undol", "undoli", "undolis", "undolist"]),
    (
        "earlier",
        &["ea", "ear", "earl", "earli", "earlie", "earlier"],
    ),
    ("later", &["lat", "late", "later"]),
    ("nohlsearch", &["noh", "nohlsearch"]),
    ("global", &["g", "gl", "glo", "glob", "globa", "global"]),
    (
        "vglobal",
        &["v", "vg", "vgl", "vglo", "vglob", "vgloba", "vglobal"],
    ),
    ("delete", &["d", "de", "del", "dele", "delet", "delete"]),
    ("move", &["m", "mo", "mov", "move"]),
    ("copy", &["t", "co", "cop", "copy"]),
    ("print", &["p", "pr", "pri", "prin", "print"]),
    ("put", &["pu", "put"]),
    ("highlight", &["hi", "highlight"]),
    ("wshada", &["wsh", "wsha", "wshad", "wshada"]),
    ("rshada", &["rsh", "rsha", "rshad", "rshada"]),
    // The server (`bemtvi-server/src/excmd.rs`).
    ("colorscheme", &["colorscheme", "colo"]),
    ("make", &["mak", "make"]),
    ("grep", &["gr", "gre", "grep"]),
    ("lmake", &["lmak", "lmake"]),
    ("lgrep", &["lgr", "lgre", "lgrep"]),
    ("source", &["so", "sou", "sour", "sourc", "source"]),
    ("cd", &["cd", "chd", "chdi", "chdir"]),
    ("tcd", &["tc", "tcd", "tch", "tchd", "tchdi", "tchdir"]),
    ("lcd", &["lc", "lcd", "lch", "lchd", "lchdi", "lchdir"]),
    ("pwd", &["pw", "pwd"]),
    ("reconnect", &["reconnect"]),
    ("disconnect", &["disconnect"]),
    (
        "autocmd",
        &["au", "aut", "auto", "autoc", "autocm", "autocmd"],
    ),
    ("augroup", &["aug", "augr", "augro", "augrou", "augroup"]),
    ("silent", &["sil", "sile", "silen", "silent"]),
    (
        "doautocmd",
        &[
            "doau",
            "doaut",
            "doauto",
            "doautoc",
            "doautocm",
            "doautocmd",
        ],
    ),
    ("command", &["com", "comm", "comma", "comman", "command"]),
    ("help", &["h", "help"]),
    ("helptags", &["helpt", "helpta", "helptag", "helptags"]),
    // Recognized before the name is split off (`editor/ex.rs`, `execute_ex_one`).
    ("normal", &["norm", "norma", "normal"]),
    ("&", &["&", "&&"]),
    ("=", &["="]),
    (
        "substitute",
        &[
            "s",
            "su",
            "sub",
            "subs",
            "subst",
            "substi",
            "substit",
            "substitu",
            "substitut",
            "substitute",
        ],
    ),
];

fn spellings() -> &'static HashMap<&'static str, &'static str> {
    static MAP: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    MAP.get_or_init(|| {
        let mut map = HashMap::new();
        for (canonical, spellings) in COMMANDS {
            for spelling in *spellings {
                let previous = map.insert(*spelling, *canonical);
                assert!(
                    previous.is_none(),
                    "ex command spelling `{spelling}` listed twice"
                );
            }
        }
        map
    })
}

/// The canonical name of the built-in command `name` spells (`e` → `edit`), or `None`
/// for a name that isn't a built-in's spelling (a user command's, or nothing).
pub fn canonical(name: &str) -> Option<&'static str> {
    spellings().get(name).copied()
}

/// The canonical name `name` goes by: its built-in's, or itself.
pub fn canonical_or_self(name: &str) -> &str {
    canonical(name).unwrap_or(name)
}

/// Every spelling of the command `name` is one of (`edit` → `e`, `edit`): the built-in's
/// whole list, or just `name` for anything else.
pub fn spellings_of(name: &str) -> Vec<&str> {
    match canonical(name) {
        Some(canonical) => COMMANDS
            .iter()
            .find(|(c, _)| *c == canonical)
            .map_or_else(Vec::new, |(_, spellings)| spellings.to_vec()),
        None => vec![name],
    }
}

/// Every `(canonical name, spellings)` pair, for tests and completion.
pub fn all() -> &'static [(&'static str, &'static [&'static str])] {
    COMMANDS
}

/// Whether a backslash before `c` in a file argument is an escape to drop — the chars
/// `btv.fname.escape` (`fnameescape`) backslashes, minus the start-of-argument guards
/// (`\+`, `\>`, `\-`), which would otherwise turn back into a `++opt` before the
/// argument is parsed. On Windows only `%` / `#`: there `\` is the path separator.
pub(crate) fn is_fname_escaped(c: char) -> bool {
    if cfg!(windows) {
        return matches!(c, '%' | '#');
    }
    matches!(
        c,
        ' ' | '\t'
            | '\n'
            | '*'
            | '?'
            | '['
            | '{'
            | '`'
            | '$'
            | '\\'
            | '%'
            | '#'
            | '\''
            | '"'
            | '|'
            | '!'
            | '<'
    )
}

/// `name` spelled as a file argument for `:edit`, `:write` and the like, so the command
/// opens the file it names: what the command line would read otherwise (`|` ending the
/// command, `%` / `#` the current and alternate file, a leading `~` home, a leading `+`
/// or `>` an option) is escaped, or kept away from the start with `./`. For a command
/// line built from a path a program holds (a file dialog's answer, an app's request).
pub fn escape_file_arg(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 2);
    if name.starts_with(['~', '+', '>', '-']) {
        out.push_str("./");
    }
    for c in name.chars() {
        if is_fname_escaped(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}
