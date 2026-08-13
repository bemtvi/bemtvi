//! Tier 1: the winit `Key` + modifiers -> vim key-notation translation, tested
//! as the public function the client uses. Black-box, no window, no GPU — the
//! GUI analogue of `bemtvi-tui`'s `keys` test.

use bemtvi_gui::{
    altgr_composed, dialog_action, encode_key, encode_paste, encode_text, is_paste,
    open_dialog_verb, open_path_command, parse_guifont, save_dialog_needed, DialogAction,
    GuiConfig,
};
use winit::keyboard::{Key, ModifiersState, NamedKey};

fn note(key: Key, mods: ModifiersState) -> Option<String> {
    encode_key(&key, mods)
}

fn ch(c: &str) -> Key {
    Key::Character(c.into())
}

#[test]
fn plain_char_is_itself() {
    assert_eq!(note(ch("a"), ModifiersState::empty()).as_deref(), Some("a"));
}

#[test]
fn shifted_char_passes_through() {
    // winit folds Shift into the character payload, so it arrives uppercased and
    // is sent literally (no separate modifier).
    assert_eq!(note(ch("A"), ModifiersState::SHIFT).as_deref(), Some("A"));
}

#[test]
fn special_keys_use_angle_notation() {
    assert_eq!(
        note(Key::Named(NamedKey::Escape), ModifiersState::empty()).as_deref(),
        Some("<Esc>")
    );
    assert_eq!(
        note(Key::Named(NamedKey::Enter), ModifiersState::empty()).as_deref(),
        Some("<CR>")
    );
    assert_eq!(
        note(Key::Named(NamedKey::Backspace), ModifiersState::empty()).as_deref(),
        Some("<BS>")
    );
    assert_eq!(
        note(Key::Named(NamedKey::Tab), ModifiersState::empty()).as_deref(),
        Some("<Tab>")
    );
    assert_eq!(
        note(Key::Named(NamedKey::Space), ModifiersState::empty()).as_deref(),
        Some(" ")
    );
}

#[test]
fn shift_tab_is_s_tab_notation() {
    // winit reports Shift+Tab as `Tab` with the SHIFT modifier (it doesn't fold a
    // named key the way it folds a character). Shift must be notated for named keys
    // so it reaches the server as `<S-Tab>` (the cmdline wildmenu / snippet tabstop
    // backward key); previously the GUI dropped the modifier and sent a plain `<Tab>`.
    assert_eq!(
        note(Key::Named(NamedKey::Tab), ModifiersState::SHIFT).as_deref(),
        Some("<S-Tab>")
    );
}

#[test]
fn ctrl_and_alt_get_prefixed() {
    assert_eq!(
        note(ch("w"), ModifiersState::CONTROL).as_deref(),
        Some("<C-w>")
    );
    assert_eq!(note(ch("x"), ModifiersState::ALT).as_deref(), Some("<A-x>"));
}

#[test]
fn ctrl_chords_that_alias_a_named_key_stay_ctrl_chords() {
    // winit reports Ctrl+I / Ctrl+M / Ctrl+[ / Ctrl+H as the base `Character` +
    // CONTROL — distinct from `NamedKey::Tab`/`Enter`/`Escape`/`Backspace` — so the
    // GUI sends `<C-i>` &c., not the named twin. A native window always disambiguates,
    // and the client declares the keyboard protocol active at attach, so the server
    // keeps these apart from `<Tab>`/`<CR>`/`<Esc>`/`<BS>` (see the keymap tests).
    assert_eq!(
        note(ch("i"), ModifiersState::CONTROL).as_deref(),
        Some("<C-i>")
    );
    assert_eq!(
        note(ch("m"), ModifiersState::CONTROL).as_deref(),
        Some("<C-m>")
    );
    assert_eq!(
        note(ch("["), ModifiersState::CONTROL).as_deref(),
        Some("<C-[>")
    );
    assert_eq!(
        note(ch("h"), ModifiersState::CONTROL).as_deref(),
        Some("<C-h>")
    );
    // ...while the named keys themselves are still their own notation.
    assert_eq!(
        note(Key::Named(NamedKey::Tab), ModifiersState::empty()).as_deref(),
        Some("<Tab>")
    );
}

#[test]
fn multi_char_character_is_sent_literally() {
    // Some layouts / compose fallbacks deliver several characters in one keystroke
    // (winit's `Key::Character` is a string, not a char). The whole payload must
    // reach the server verbatim — not be silently truncated to the first char.
    assert_eq!(
        note(ch("日本"), ModifiersState::empty()).as_deref(),
        Some("日本")
    );
    // `<` is still escaped so a multi-char payload can't smuggle a `<...>` form.
    assert_eq!(
        note(ch("a<b"), ModifiersState::empty()).as_deref(),
        Some("a<lt>b")
    );
}

#[test]
fn literal_less_than_is_escaped() {
    assert_eq!(
        note(ch("<"), ModifiersState::empty()).as_deref(),
        Some("<lt>")
    );
}

#[test]
fn navigation_keys_use_angle_notation() {
    assert_eq!(
        note(Key::Named(NamedKey::ArrowLeft), ModifiersState::empty()).as_deref(),
        Some("<Left>")
    );
    assert_eq!(
        note(Key::Named(NamedKey::PageDown), ModifiersState::empty()).as_deref(),
        Some("<PageDown>")
    );
}

#[test]
fn altgr_composition_is_typing_not_a_chord() {
    // Windows reports AltGr as Ctrl+Alt: `AltGr+E` on a European layout arrives as
    // `Character("€")` with CONTROL|ALT set. The layout *composed* the key
    // (logical ≠ un-modified base), so it is typing — the client must send the €,
    // not the `<C-A-e>` chord that would swallow it.
    let ca = ModifiersState::CONTROL | ModifiersState::ALT;
    assert!(altgr_composed(&ch("€"), &ch("e"), ca));
    assert!(altgr_composed(&ch("@"), &ch("q"), ca)); // AltGr+Q on German layouts

    // A real Ctrl+Alt chord composes nothing: logical == base → chord behavior.
    assert!(!altgr_composed(&ch("e"), &ch("e"), ca));
    // Shift folds into the logical character ("#" from "3"), so a difference under
    // Ctrl+Alt+Shift proves nothing — the chord keeps chord behavior.
    assert!(!altgr_composed(
        &ch("#"),
        &ch("3"),
        ca | ModifiersState::SHIFT
    ));
    // CapsLock uppercases the logical key; a case-only difference isn't composition.
    assert!(!altgr_composed(&ch("A"), &ch("a"), ca));
    // Ctrl-only / Alt-only combos are never AltGr.
    assert!(!altgr_composed(&ch("€"), &ch("e"), ModifiersState::CONTROL));
    assert!(!altgr_composed(&ch("€"), &ch("e"), ModifiersState::ALT));
    // Named keys can't be composed characters.
    assert!(!altgr_composed(
        &Key::Named(NamedKey::Tab),
        &Key::Named(NamedKey::Tab),
        ca
    ));
}

#[test]
fn bare_modifier_is_dropped() {
    // A lone modifier key (Control pressed by itself) has no editor meaning.
    assert_eq!(
        note(Key::Named(NamedKey::Control), ModifiersState::empty()),
        None
    );
}

#[test]
fn open_dialog_maps_o_commands_to_their_base_verb() {
    // The `…o` open family (and bare `:e`/`:edit`, an alias of `:eo`) pops the open
    // dialog; the base verb to run with the chosen file comes back.
    assert_eq!(open_dialog_verb("eo"), Some("e"));
    assert_eq!(open_dialog_verb("e"), Some("e"));
    assert_eq!(open_dialog_verb("edit"), Some("e"));
    assert_eq!(open_dialog_verb("spo"), Some("sp"));
    assert_eq!(open_dialog_verb("vso"), Some("vs"));
    assert_eq!(open_dialog_verb("tabeo"), Some("tabe"));
    assert_eq!(open_dialog_verb("newo"), Some("new"));
    assert_eq!(open_dialog_verb("vnewo"), Some("vnew"));
    // Surrounding whitespace is ignored.
    assert_eq!(open_dialog_verb("  eo  "), Some("e"));
}

#[test]
fn open_path_command_matches_only_the_open_family_with_an_arg() {
    // The commands the server routes through `ex_edit` (where a directory argument
    // opens netrw) — each maps to its canonical base verb, carrying the raw arg.
    assert_eq!(open_path_command("e src"), Some(("e", "src")));
    assert_eq!(open_path_command("edit src"), Some(("e", "src")));
    assert_eq!(open_path_command("sp src"), Some(("sp", "src")));
    assert_eq!(open_path_command("split src"), Some(("sp", "src")));
    assert_eq!(open_path_command("vs src"), Some(("vs", "src")));
    assert_eq!(open_path_command("vsplit src"), Some(("vs", "src")));
    assert_eq!(open_path_command("tabe src"), Some(("tabe", "src")));
    assert_eq!(open_path_command("tabedit src"), Some(("tabe", "src")));
    assert_eq!(open_path_command("tabnew src"), Some(("tabe", "src")));
    // A bang (`:e! dir`) is irrelevant to listing a directory; the verb still maps.
    assert_eq!(open_path_command("e! src"), Some(("e", "src")));
    // The argument is taken raw (vim's `:edit` parser keeps the whole tail) and
    // surrounding whitespace is trimmed; a path with spaces stays intact.
    assert_eq!(open_path_command("  e   my dir  "), Some(("e", "my dir")));
}

#[test]
fn open_path_command_ignores_non_open_and_bare_commands() {
    // Commands that legitimately take a directory but must NOT pop a file picker:
    // changing the working directory, writing, grepping, setting an option, …
    assert_eq!(open_path_command("cd src"), None);
    assert_eq!(open_path_command("lcd src"), None);
    assert_eq!(open_path_command("tcd src"), None);
    assert_eq!(open_path_command("w src"), None);
    assert_eq!(open_path_command("write src"), None);
    assert_eq!(open_path_command("grep foo src"), None);
    assert_eq!(open_path_command("set path=src"), None);
    // A verb that merely starts with an open name is not one of them.
    assert_eq!(open_path_command("earlier src"), None);
    assert_eq!(open_path_command("spell src"), None);
    // Bare forms carry no argument → handled by `open_dialog_verb`, not here.
    assert_eq!(open_path_command("e"), None);
    assert_eq!(open_path_command("sp"), None);
    assert_eq!(open_path_command("e   "), None);
    assert_eq!(open_path_command(""), None);
}

#[test]
fn open_dialog_leaves_other_commands_alone() {
    // Bare splits/tabs keep their usual no-argument behavior (only the `…o` forms
    // open the dialog).
    assert_eq!(open_dialog_verb("sp"), None);
    assert_eq!(open_dialog_verb("vs"), None);
    assert_eq!(open_dialog_verb("tabe"), None);
    // Anything with an argument, a bang, or a non-open verb runs as typed.
    assert_eq!(open_dialog_verb("eo foo.txt"), None);
    assert_eq!(open_dialog_verb("e!"), None);
    assert_eq!(open_dialog_verb("w"), None);
    assert_eq!(open_dialog_verb(""), None);
}

#[test]
fn guifont_parses_family_and_size() {
    let fams = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();

    // `Family:h<size>` — the neovim/Neovide form set in init.lua.
    assert_eq!(
        parse_guifont("Source Code Pro:h14"),
        (fams(&["Source Code Pro"]), Some(14.0))
    );
    // Backslash-escaped spaces (the `:set guifont=...` form) are unescaped.
    assert_eq!(
        parse_guifont("Fira\\ Code:h12"),
        (fams(&["Fira Code"]), Some(12.0))
    );
    // A comma list is the full wezterm-style fallback chain (primary first), with
    // extra `:` options ignored and each family trimmed/unescaped.
    assert_eq!(
        parse_guifont("JetBrains Mono,Noto Color Emoji,Symbols Nerd Font:h16:b:#e-subpixel"),
        (
            fams(&["JetBrains Mono", "Noto Color Emoji", "Symbols Nerd Font"]),
            Some(16.0)
        )
    );
    // Family only / size only / empty — each component falls back independently.
    assert_eq!(parse_guifont("Iosevka"), (fams(&["Iosevka"]), None));
    assert_eq!(parse_guifont(":h20"), (Vec::new(), Some(20.0)));
    assert_eq!(parse_guifont(""), (Vec::new(), None));
    // A non-positive or junk size is rejected (kept as None, not 0).
    assert_eq!(parse_guifont("Mono:h0"), (fams(&["Mono"]), None));
    assert_eq!(parse_guifont("Mono:hx"), (fams(&["Mono"]), None));
}

#[test]
fn config_font_takes_a_comma_separated_fallback_list() {
    // `--font` / `BEMTVI_GUI_FONT` accepts the same comma list as `guifont`: the
    // primary first, then the fallback chain (each trimmed, `\ ` unescaped).
    let mut cfg = GuiConfig::default();
    assert!(cfg.fonts.is_empty()); // default = system monospace

    cfg.set_font("JetBrains\\ Mono, Noto Color Emoji ,Symbols Nerd Font");
    assert_eq!(
        cfg.fonts,
        vec![
            "JetBrains Mono".to_string(),
            "Noto Color Emoji".to_string(),
            "Symbols Nerd Font".to_string(),
        ]
    );

    // An all-blank spec leaves the existing list untouched (keeps the default rather
    // than asking the font system for `""`).
    cfg.set_font("  , ");
    assert_eq!(cfg.fonts.len(), 3);
}

#[test]
fn paste_gestures_are_recognized_and_dont_shadow_ctrl_v() {
    // Cmd+V (macOS), Ctrl+Shift+V (Linux/Windows), and Shift+Insert all paste.
    assert!(is_paste(&ch("v"), ModifiersState::SUPER));
    assert!(is_paste(
        &ch("V"),
        ModifiersState::CONTROL | ModifiersState::SHIFT
    ));
    assert!(is_paste(
        &Key::Named(NamedKey::Insert),
        ModifiersState::SHIFT
    ));
    // Plain `v`, vim's `<C-v>` (literal-insert / blockwise visual), and a bare
    // Insert must NOT be treated as paste.
    assert!(!is_paste(&ch("v"), ModifiersState::empty()));
    assert!(!is_paste(&ch("v"), ModifiersState::CONTROL));
    assert!(!is_paste(
        &Key::Named(NamedKey::Insert),
        ModifiersState::empty()
    ));
}

#[test]
fn ime_committed_text_encodes_multibyte_verbatim() {
    // Composed/non-ASCII input (dead-key accents, AltGr, CJK) arrives as an
    // `Ime::Commit`, which the GUI feeds through `encode_text`. The committed
    // characters — including multibyte ones — must reach the server byte-for-byte,
    // not be stripped to a base key or mangled.
    assert_eq!(encode_text("é"), "é");
    assert_eq!(encode_text("café"), "café");
    assert_eq!(encode_text("ñ"), "ñ");
    // Full IME composition can commit several characters at once (a CJK word).
    assert_eq!(encode_text("日本語"), "日本語");
    // `<` is still escaped so committed text can't open a `<...>` notation form.
    assert_eq!(encode_text("a<b"), "a<lt>b");
}

#[test]
fn an_ime_commit_is_not_encoded_as_a_paste() {
    // A commit is the user's own typing arriving as one string, so it must NOT be
    // wrapped in the bracketed-paste markers: those put the editor in paste mode,
    // where insert-mode behaviour (auto-pairs, auto-indent) stands down. Only the
    // clipboard path — `encode_paste` — brackets its payload.
    assert_eq!(encode_text("é"), "é");
    assert_eq!(encode_paste("é"), "<PasteStart>é<PasteEnd>");
}

#[test]
fn save_dialog_fires_for_wo_and_bare_write_on_unnamed() {
    // `:wo` always saves to a new file via the dialog, named buffer or not.
    assert!(save_dialog_needed("wo", false));
    assert!(save_dialog_needed("wo", true));
    // `:wn` is *not* a save dialog — it is vim's `:wnext`, left to run as typed.
    assert!(!save_dialog_needed("wn", true));
    // A bare `:w`/`:write` pops the dialog only when the buffer has no file yet.
    assert!(save_dialog_needed("w", true));
    assert!(save_dialog_needed("write", true));
    assert!(!save_dialog_needed("w", false));
    // An explicit target, a different command, or `:wq` runs as typed.
    assert!(!save_dialog_needed("w foo.txt", true));
    assert!(!save_dialog_needed("wq", true));
    assert!(!save_dialog_needed("e", true));
}

#[test]
fn dialog_action_is_suppressed_in_a_remote_session() {
    // Connected to a remote daemon, the buffers live on the *daemon's* fs — a local
    // native open/save dialog would browse and write the wrong machine. So `<CR>`
    // over every dialog-triggering `:` command must run as typed (None), letting the
    // server handle it (netrw listing, `E32` for a nameless `:w`, …). This is the
    // regression guard for the "GUI pops a native dialog while connected to remote" bug.
    for &(cmdline, unnamed) in &[
        ("eo", false),    // open family
        ("e", false),     // bare `:e` aliases `:eo`
        ("e src", false), // open-with-path (dir picker, locally)
        ("wo", false),    // save-as
        ("w", true),      // bare `:w` on an unnamed buffer
        ("write", true),
    ] {
        assert!(
            dialog_action(cmdline, unnamed, true).is_none(),
            "remote session must not pop a dialog for {cmdline:?}"
        );
    }
}

#[test]
fn dialog_action_routes_each_trigger_locally() {
    // The same triggers still pop their dialog in a *local* session (remote = false),
    // so the suppression above is the only behavior change.
    assert!(matches!(
        dialog_action("eo", false, false),
        Some(DialogAction::Open { base: "e" })
    ));
    assert!(matches!(
        dialog_action("e src", false, false),
        Some(DialogAction::OpenPath {
            base: "e",
            arg: "src"
        })
    ));
    assert!(matches!(
        dialog_action("wo", false, false),
        Some(DialogAction::Save)
    ));
    assert!(matches!(
        dialog_action("w", true, false),
        Some(DialogAction::Save)
    ));
    // A nameless concern that triggers nothing stays None even locally.
    assert!(dialog_action("wq", true, false).is_none());
}
