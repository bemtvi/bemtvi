//! Windows file associations: the registry keys behind `btv.file_associations`, which
//! add the running editor to Explorer's "Open with" list for every extension bemtvi
//! recognizes and give those files bemtvi's document icon.
//!
//! The layout is VS Code's: one ProgID per extension (`bemtvi.rs`, …), each naming its
//! filetype, its icon (`"<exe>",-2`, the document icon `bemtvi-gui`'s `build.rs` embeds
//! as resource 2) and its open command, and listed in that extension's
//! `OpenWithProgids`. Windows never lets an app make itself the default (the user's
//! choice is hash-protected), so this only makes bemtvi a choice; picking it once per
//! type in "Open with" makes it the default. Two more entries cover what the per-type
//! list doesn't: `Applications\<exe>` gives the exe its name and the document icon when
//! a file is opened through the exe itself (a choice made before registering), and
//! `SystemFileAssociations\text\OpenWithList` offers bemtvi for every type Windows
//! counts as text. And a ProgID of the user's own that already opens files with this
//! exe (Explorer's "Browse…" makes an `<ext>_auto_file` one, and keeps choosing it for
//! the exe even after bemtvi's is listed) gets the document icon too, when it has none.
//!
//! Everything goes under `HKEY_CURRENT_USER`, so it needs no administrator rights and
//! touches no other user. The `classes` key the callers pass is
//! [`DEFAULT_CLASSES`] in real use; a test points it at a scratch key of its own.
//!
//! A pass makes thousands of registry calls (a few hundred milliseconds in a debug
//! build), so [`start`] runs it on a thread of its own and hands back a [`Job`] the Lua
//! side polls: the server's Lua thread never waits on the registry. One extension whose
//! keys can't be written doesn't stop the rest; the pass carries on and lists every
//! failure in its [`Report`].

/// Where per-user file types live (`HKCU\Software\Classes`, merged into `HKCR`).
pub(crate) const DEFAULT_CLASSES: &str = r"Software\Classes";

/// What a register / unregister did: which exe it named, for how many extensions it
/// succeeded, and what it couldn't do (one line per extension, or the exe's own keys).
pub(crate) struct Report {
    pub exe: String,
    pub extensions: usize,
    pub failed: Vec<String>,
}

/// A finished pass: its report, or why it couldn't run at all (no exe path).
pub(crate) type Outcome = Result<Report, String>;

/// A pass under way: [`Job::poll`] it until it hands back its [`Outcome`].
pub(crate) struct Job(std::sync::mpsc::Receiver<Outcome>);

impl Job {
    /// `None` while the pass runs; its outcome once it's done. A pass whose thread died
    /// without one (a panic) comes back as an error, rather than leaving the poller
    /// waiting forever.
    pub(crate) fn poll(&self) -> Option<Outcome> {
        use std::sync::mpsc::TryRecvError;
        match self.0.try_recv() {
            Ok(outcome) => Some(outcome),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err(
                "the registry pass stopped without finishing".to_string(),
            )),
        }
    }
}

/// Start a `register` (`true`) or `unregister` (`false`) pass over `classes`, off the
/// calling thread. Off Windows there is nothing to do and the job is done already.
pub(crate) fn start(register_keys: bool, classes: String) -> Job {
    // One pass at a time in the whole process. The Lua side queues its own, but each
    // session has its own Lua state: a pass left running by a session the GUI swapped
    // away from (`:workspace`, `:connect`) must not interleave with the next session's.
    static PASS: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let (tx, rx) = std::sync::mpsc::channel();
    let pass = move || {
        let _one_at_a_time = PASS.lock().unwrap_or_else(|e| e.into_inner());
        let outcome = if register_keys {
            register(&classes)
        } else {
            unregister(&classes)
        };
        let _ = tx.send(outcome);
    };
    #[cfg(windows)]
    {
        let spawned = std::thread::Builder::new()
            .name("bemtvi-fileassoc".to_string())
            .spawn(pass);
        if let Err(e) = spawned {
            let (tx, rx) = std::sync::mpsc::channel();
            let _ = tx.send(Err(format!("can't start the registry pass: {e}")));
            return Job(rx);
        }
    }
    #[cfg(not(windows))]
    pass();
    Job(rx)
}

/// The ProgID prefix: `bemtvi.<ext>`.
#[cfg(windows)]
const PROGID_PREFIX: &str = "bemtvi.";

/// Extensions beyond the filetype table: plain text, which no grammar claims.
#[cfg(windows)]
const PLAIN_TEXT: &[(&str, &str)] = &[("txt", "text"), ("log", "text")];

/// The extensions to register, lower-cased and deduplicated (the registry is
/// case-insensitive, so `C` and `c` are one key), each with the filetype bemtvi gives
/// a file spelled that way — so the shared key is named for `.c` (C), not `.C` (C++),
/// whatever order the table lists them in.
#[cfg(windows)]
fn extensions() -> Vec<(String, &'static str)> {
    let mut out: Vec<(String, &'static str)> = Vec::new();
    for (ext, ft) in bemtvi_core::known_extensions().iter().chain(PLAIN_TEXT) {
        let ext = ext.to_ascii_lowercase();
        if out.iter().any(|(e, _)| *e == ext) {
            continue;
        }
        let path = std::path::PathBuf::from(format!("file.{ext}"));
        let ft = bemtvi_core::language_of_path(Some(&path)).unwrap_or(ft);
        out.push((ext, ft));
    }
    out
}

/// The type name Explorer shows for a file of filetype `ft`: `rust` → `Rust file`,
/// `c_sharp` → `C sharp file`.
#[cfg(windows)]
fn type_name(ft: &str) -> String {
    let words = ft.replace('_', " ");
    let mut chars = words.chars();
    match chars.next() {
        Some(first) => format!("{}{} file", first.to_uppercase(), chars.as_str()),
        None => "File".to_string(),
    }
}

/// This process's executable: what the keys open files with.
pub(crate) fn current_exe() -> Result<std::path::PathBuf, String> {
    std::env::current_exe().map_err(|e| format!("can't tell which program is running: {e}"))
}

#[cfg(windows)]
pub(crate) use imp::{register, unregister};

#[cfg(not(windows))]
pub(crate) fn register(_classes: &str) -> Result<Report, String> {
    Err(unsupported())
}

#[cfg(not(windows))]
pub(crate) fn unregister(_classes: &str) -> Result<Report, String> {
    Err(unsupported())
}

#[cfg(not(windows))]
fn unsupported() -> String {
    format!(
        "file associations are registered on Windows only (this is {})",
        std::env::consts::OS
    )
}

#[cfg(windows)]
mod imp {
    use super::{current_exe, extensions, type_name, Report, DEFAULT_CLASSES, PROGID_PREFIX};
    use windows_sys::Win32::Foundation::{
        ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_SUCCESS, WIN32_ERROR,
    };
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteKeyValueW, RegDeleteKeyW, RegDeleteTreeW,
        RegEnumValueW, RegGetValueW, RegOpenKeyExW, RegQueryInfoKeyW, RegSetValueExW, HKEY,
        HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_NONE, REG_OPTION_NON_VOLATILE, REG_SZ,
        RRF_RT_REG_SZ,
    };
    use windows_sys::Win32::UI::Shell::{SHChangeNotify, SHCNE_ASSOCCHANGED, SHCNF_IDLIST};

    /// A NUL-terminated UTF-16 copy of `s`, for the `W` APIs.
    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn check(what: &str, err: WIN32_ERROR) -> Result<(), String> {
        if err == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(format!(
                "{what}: {}",
                std::io::Error::from_raw_os_error(err as i32)
            ))
        }
    }

    /// An open key, closed on drop.
    struct Key(HKEY);

    impl Drop for Key {
        fn drop(&mut self) {
            // SAFETY: `self.0` is a key this process opened and hasn't closed.
            unsafe { RegCloseKey(self.0) };
        }
    }

    impl Key {
        /// Open `HKCU\<path>` for writing, creating it (and its parents) if absent.
        fn create(path: &str) -> Result<Key, String> {
            let mut key: HKEY = std::ptr::null_mut();
            let sub = wide(path);
            // SAFETY: `sub` is NUL-terminated and outlives the call; `key` receives the
            // handle, closed by `Key`'s drop.
            let err = unsafe {
                RegCreateKeyExW(
                    HKEY_CURRENT_USER,
                    sub.as_ptr(),
                    0,
                    std::ptr::null(),
                    REG_OPTION_NON_VOLATILE,
                    KEY_WRITE,
                    std::ptr::null(),
                    &mut key,
                    std::ptr::null_mut(),
                )
            };
            check(&format!(r"can't create HKCU\{path}"), err)?;
            Ok(Key(key))
        }

        /// Set the string value `name` (`""`: the key's default value).
        fn set_str(&self, name: &str, value: &str) -> Result<(), String> {
            let name_w = wide(name);
            let data = wide(value);
            // SAFETY: both buffers are NUL-terminated and outlive the call; the byte
            // count covers `data` including its terminator, as REG_SZ wants.
            let err = unsafe {
                RegSetValueExW(
                    self.0,
                    name_w.as_ptr(),
                    0,
                    REG_SZ,
                    data.as_ptr().cast(),
                    (data.len() * 2) as u32,
                )
            };
            check(&format!("can't set {name:?}"), err)
        }

        /// Set an empty `REG_NONE` value `name` — the form an `OpenWithProgids` entry takes.
        fn set_none(&self, name: &str) -> Result<(), String> {
            let name_w = wide(name);
            // SAFETY: `name_w` is NUL-terminated and outlives the call; no data.
            let err = unsafe {
                RegSetValueExW(self.0, name_w.as_ptr(), 0, REG_NONE, std::ptr::null(), 0)
            };
            check(&format!("can't set {name:?}"), err)
        }
    }

    /// The string value `name` (`None`: the default value) of `HKCU\<path>`, or `None`
    /// when the key or value isn't there or isn't a string. A `REG_EXPAND_SZ` comes back
    /// expanded.
    fn read_str(path: &str, name: Option<&str>) -> Option<String> {
        let sub = wide(path);
        let name_w = name.map(wide);
        let name_ptr = name_w.as_ref().map_or(std::ptr::null(), |n| n.as_ptr());
        let mut buf: Vec<u16> = vec![0; 260];
        loop {
            let mut bytes = (buf.len() * 2) as u32;
            // SAFETY: `sub` / `name_w` are NUL-terminated and outlive the call; `buf`
            // holds `bytes` bytes, and the call writes at most that many.
            let err = unsafe {
                RegGetValueW(
                    HKEY_CURRENT_USER,
                    sub.as_ptr(),
                    name_ptr,
                    RRF_RT_REG_SZ,
                    std::ptr::null_mut(),
                    buf.as_mut_ptr().cast(),
                    &mut bytes,
                )
            };
            if err == ERROR_MORE_DATA {
                // Always grow: a size that doesn't (a value rewritten meanwhile, an
                // expansion reported short) must not spin this loop forever.
                let want = (bytes as usize).div_ceil(2) + 1;
                buf.resize(want.max(buf.len() * 2), 0);
                continue;
            }
            if err != ERROR_SUCCESS {
                return None;
            }
            let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            return Some(String::from_utf16_lossy(&buf[..len]));
        }
    }

    /// The value names of `HKCU\<path>` (empty when the key isn't there).
    fn value_names(path: &str) -> Vec<String> {
        let sub = wide(path);
        let mut key: HKEY = std::ptr::null_mut();
        // SAFETY: `sub` is NUL-terminated and outlives the call; `key` is closed below.
        if unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, sub.as_ptr(), 0, KEY_READ, &mut key) }
            != ERROR_SUCCESS
        {
            return Vec::new();
        }
        let key = Key(key);
        let mut names = Vec::new();
        // A value name is at most 16383 characters.
        let mut buf = vec![0u16; 16384];
        for index in 0.. {
            let mut len = buf.len() as u32;
            // SAFETY: `buf` holds `len` characters; no type or data is asked for.
            let err = unsafe {
                RegEnumValueW(
                    key.0,
                    index,
                    buf.as_mut_ptr(),
                    &mut len,
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            if err != ERROR_SUCCESS {
                break;
            }
            names.push(String::from_utf16_lossy(&buf[..len as usize]));
        }
        names
    }

    /// The program an open command runs: its first quoted word, or (unquoted) up to
    /// the `.exe`, or the first word.
    fn command_program(command: &str) -> &str {
        let command = command.trim_start();
        if let Some(rest) = command.strip_prefix('"') {
            return rest.split('"').next().unwrap_or(rest);
        }
        if let Some(end) = command.to_ascii_lowercase().find(".exe") {
            return &command[..end + 4];
        }
        command.split_whitespace().next().unwrap_or(command)
    }

    fn same_path(a: &str, b: &str) -> bool {
        a.replace('/', "\\")
            .eq_ignore_ascii_case(&b.replace('/', "\\"))
    }

    /// The ProgIDs, other than bemtvi's own, that `.ext` already resolves to — its
    /// default and its `OpenWithProgids` — whose open command runs `exe`. A choice made
    /// before registering (Explorer's "Browse…" writes an `<ext>_auto_file` ProgID and
    /// keeps choosing it for this exe afterwards) is one of these, and has no icon.
    fn progids_opening_with(classes: &str, ext: &str, exe: &str) -> Vec<String> {
        let own = format!("{PROGID_PREFIX}{ext}");
        let mut candidates: Vec<String> = Vec::new();
        let default = read_str(&format!(r"{classes}\.{ext}"), None);
        let listed = value_names(&format!(r"{classes}\.{ext}\OpenWithProgids"));
        for progid in default.into_iter().chain(listed) {
            if progid.is_empty()
                || progid.eq_ignore_ascii_case(&own)
                || candidates.iter().any(|c| c.eq_ignore_ascii_case(&progid))
            {
                continue;
            }
            candidates.push(progid);
        }
        candidates.retain(|progid| {
            read_str(&format!(r"{classes}\{progid}\shell\open\command"), None)
                .is_some_and(|command| same_path(command_program(&command), exe))
        });
        candidates
    }

    /// Delete `HKCU\<path>` and everything under it; a key that isn't there is fine.
    fn delete_tree(path: &str) -> Result<(), String> {
        let sub = wide(path);
        // SAFETY: `sub` is NUL-terminated and outlives the call.
        let err = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, sub.as_ptr()) };
        if err == ERROR_FILE_NOT_FOUND {
            return Ok(());
        }
        check(&format!(r"can't delete HKCU\{path}"), err)
    }

    /// Delete the value `name` of `HKCU\<path>`; one that isn't there is fine.
    fn delete_value(path: &str, name: &str) -> Result<(), String> {
        let (sub, name_w) = (wide(path), wide(name));
        // SAFETY: both buffers are NUL-terminated and outlive the call.
        let err = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, sub.as_ptr(), name_w.as_ptr()) };
        if err == ERROR_FILE_NOT_FOUND {
            return Ok(());
        }
        check(&format!(r"can't delete HKCU\{path}\{name}"), err)
    }

    /// Delete `HKCU\<path>` when it holds no values and no subkeys — the empty
    /// `.ext\OpenWithProgids` (and `.ext`) keys `register` created for a type the user
    /// had no key of their own for. A key that isn't there, or isn't empty, is left.
    fn delete_if_empty(path: &str) -> Result<(), String> {
        let sub = wide(path);
        let mut key: HKEY = std::ptr::null_mut();
        // SAFETY: `sub` is NUL-terminated and outlives the call; `key` is closed by `Key`.
        if unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, sub.as_ptr(), 0, KEY_READ, &mut key) }
            != ERROR_SUCCESS
        {
            return Ok(());
        }
        let key = Key(key);
        let (mut subkeys, mut values) = (0u32, 0u32);
        // SAFETY: only the two counts are asked for; every other out-pointer is null.
        let err = unsafe {
            RegQueryInfoKeyW(
                key.0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
                &mut subkeys,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut values,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        drop(key);
        if err != ERROR_SUCCESS || subkeys != 0 || values != 0 {
            return Ok(());
        }
        // SAFETY: `sub` is NUL-terminated and outlives the call.
        let err = unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, sub.as_ptr()) };
        if err == ERROR_FILE_NOT_FOUND {
            return Ok(());
        }
        check(&format!(r"can't delete HKCU\{path}"), err)
    }

    /// Tell Explorer the associations changed, so it redraws file icons now rather than
    /// at the next logon. Only for the real classes key — a test's scratch key isn't one
    /// Explorer reads.
    fn announce(classes: &str) {
        if classes.eq_ignore_ascii_case(DEFAULT_CLASSES) {
            // SAFETY: SHCNE_ASSOCCHANGED takes no items.
            unsafe {
                SHChangeNotify(
                    SHCNE_ASSOCCHANGED as i32,
                    SHCNF_IDLIST,
                    std::ptr::null(),
                    std::ptr::null(),
                )
            };
        }
    }

    /// The exe's path and file name, as the keys spell them.
    fn exe() -> Result<(String, String), String> {
        let path = current_exe()?;
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| format!("the program's name isn't valid text: {}", path.display()))?
            .to_string();
        let path = path
            .to_str()
            .ok_or_else(|| format!("the program's path isn't valid text: {}", path.display()))?
            .to_string();
        Ok((path, name))
    }

    /// Register one extension: its ProgID, its `OpenWithProgids` entry, and the icon for
    /// a ProgID of the user's that already opens with this exe.
    fn register_extension(
        classes: &str,
        ext: &str,
        ft: &str,
        exe: &str,
        icon: &str,
        command: &str,
    ) -> Result<(), String> {
        let progid = format!("{PROGID_PREFIX}{ext}");
        let base = format!(r"{classes}\{progid}");
        Key::create(&base)?.set_str("", &type_name(ft))?;
        Key::create(&format!(r"{base}\DefaultIcon"))?.set_str("", icon)?;
        Key::create(&format!(r"{base}\shell\open\command"))?.set_str("", command)?;
        Key::create(&format!(r"{classes}\.{ext}\OpenWithProgids"))?.set_none(&progid)?;
        // A ProgID of the user's that already opens with this exe gets the icon too,
        // unless it has one of its own.
        for other in progids_opening_with(classes, ext, exe) {
            let key = format!(r"{classes}\{other}\DefaultIcon");
            if read_str(&key, None).is_none_or(|i| i.is_empty()) {
                Key::create(&key)?.set_str("", icon)?;
            }
        }
        Ok(())
    }

    /// The exe's own keys: its name, the icon, and every text type's "Open with".
    fn register_app(
        classes: &str,
        exe_name: &str,
        icon: &str,
        command: &str,
    ) -> Result<(), String> {
        let app = format!(r"{classes}\Applications\{exe_name}");
        Key::create(&app)?.set_str("FriendlyAppName", "bemtvi")?;
        Key::create(&format!(r"{app}\DefaultIcon"))?.set_str("", icon)?;
        Key::create(&format!(r"{app}\shell\open\command"))?.set_str("", command)?;
        Key::create(&format!(
            r"{classes}\SystemFileAssociations\text\OpenWithList\{exe_name}"
        ))?;
        Ok(())
    }

    /// Write every key. An extension (or the exe's own keys) that fails is listed in the
    /// report's `failed`, and the pass carries on with the rest; only not knowing the
    /// exe stops it before it starts.
    pub(crate) fn register(classes: &str) -> Result<Report, String> {
        let (exe, exe_name) = exe()?;
        let icon = format!("\"{exe}\",-2");
        let command = format!("\"{exe}\" \"%1\"");
        let (mut done, mut failed) = (0, Vec::new());
        for (ext, ft) in &extensions() {
            match register_extension(classes, ext, ft, &exe, &icon, &command) {
                Ok(()) => done += 1,
                Err(e) => failed.push(format!(".{ext}: {e}")),
            }
        }
        if let Err(e) = register_app(classes, &exe_name, &icon, &command) {
            failed.push(format!("{exe_name}: {e}"));
        }
        announce(classes);
        Ok(Report {
            exe,
            extensions: done,
            failed,
        })
    }

    /// Remove what [`register_extension`] wrote for `ext`.
    fn unregister_extension(classes: &str, ext: &str, exe: &str, icon: &str) -> Result<(), String> {
        // Take back the icon `register` gave a ProgID of the user's; the ProgID
        // itself is their choice, and stays.
        for other in progids_opening_with(classes, ext, exe) {
            let key = format!(r"{classes}\{other}\DefaultIcon");
            if read_str(&key, None).as_deref() == Some(icon) {
                delete_tree(&key)?;
            }
        }
        let progid = format!("{PROGID_PREFIX}{ext}");
        let open_with = format!(r"{classes}\.{ext}\OpenWithProgids");
        delete_value(&open_with, &progid)?;
        delete_if_empty(&open_with)?;
        delete_if_empty(&format!(r"{classes}\.{ext}"))?;
        delete_tree(&format!(r"{classes}\{progid}"))
    }

    /// Remove what [`register_app`] wrote, but the exe's own `shell\open\command`: Windows
    /// writes the same one when a user picks the exe in "Open with", and a choice made
    /// that way must keep working.
    fn unregister_app(classes: &str, exe_name: &str) -> Result<(), String> {
        let app = format!(r"{classes}\Applications\{exe_name}");
        delete_value(&app, "FriendlyAppName")?;
        delete_tree(&format!(r"{app}\DefaultIcon"))?;
        delete_tree(&format!(
            r"{classes}\SystemFileAssociations\text\OpenWithList\{exe_name}"
        ))
    }

    /// Remove every key, carrying on past the ones that fail like [`register`] does.
    pub(crate) fn unregister(classes: &str) -> Result<Report, String> {
        let (exe, exe_name) = exe()?;
        let icon = format!("\"{exe}\",-2");
        let (mut done, mut failed) = (0, Vec::new());
        for (ext, _) in &extensions() {
            match unregister_extension(classes, ext, &exe, &icon) {
                Ok(()) => done += 1,
                Err(e) => failed.push(format!(".{ext}: {e}")),
            }
        }
        if let Err(e) = unregister_app(classes, &exe_name) {
            failed.push(format!("{exe_name}: {e}"));
        }
        announce(classes);
        Ok(Report {
            exe,
            extensions: done,
            failed,
        })
    }
}
