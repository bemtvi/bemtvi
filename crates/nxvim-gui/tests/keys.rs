//! Tier 1: the winit `Key` + modifiers -> vim key-notation translation, tested
//! as the public function the client uses. Black-box, no window, no GPU — the
//! GUI analogue of `nxvim-tui`'s `keys` test.

use nxvim_gui::{
    dialog_action, encode_key, is_paste, open_dialog_verb, open_path_command, parse_guifont,
    save_dialog_needed, DialogAction,
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
fn ctrl_and_alt_get_prefixed() {
    assert_eq!(
        note(ch("w"), ModifiersState::CONTROL).as_deref(),
        Some("<C-w>")
    );
    assert_eq!(note(ch("x"), ModifiersState::ALT).as_deref(), Some("<A-x>"));
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
    // `Family:h<size>` — the neovim/Neovide form set in init.lua.
    assert_eq!(
        parse_guifont("Source Code Pro:h14"),
        (Some("Source Code Pro".to_string()), Some(14.0))
    );
    // Backslash-escaped spaces (the `:set guifont=...` form) are unescaped.
    assert_eq!(
        parse_guifont("Fira\\ Code:h12"),
        (Some("Fira Code".to_string()), Some(12.0))
    );
    // A fallback list uses the first font; extra `:` options are ignored.
    assert_eq!(
        parse_guifont("JetBrains Mono,Noto Sans:h16:b:#e-subpixel"),
        (Some("JetBrains Mono".to_string()), Some(16.0))
    );
    // Family only / size only / empty — each component falls back independently.
    assert_eq!(
        parse_guifont("Iosevka"),
        (Some("Iosevka".to_string()), None)
    );
    assert_eq!(parse_guifont(":h20"), (None, Some(20.0)));
    assert_eq!(parse_guifont(""), (None, None));
    // A non-positive or junk size is rejected (kept as None, not 0).
    assert_eq!(parse_guifont("Mono:h0"), (Some("Mono".to_string()), None));
    assert_eq!(parse_guifont("Mono:hx"), (Some("Mono".to_string()), None));
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
