//! Tier 1: the surface's key events (mitsuami's `SurfaceInput`) -> vim key-notation,
//! end to end: the gui's translation into `bemtvi-gpu`'s events, then its encoder.
//! Black-box, no window, no GPU — the GUI analogue of `bemtvi-tui`'s `keys` test.

use bemtvi_mitsuami::{
    dialog_action, encode_input, encode_paste, encode_text, is_paste, open_dialog_verb,
    open_path_command, save_dialog_needed, DialogAction, GuiConfig,
};
use bemtvi_view::FontSpec;
use mitsuami::prelude::{KeyCode, Modifiers, SurfaceInput};

const NONE: Modifiers = Modifiers {
    shift: false,
    control: false,
    alt: false,
    meta: false,
};
const SHIFT: Modifiers = Modifiers {
    shift: true,
    ..NONE
};
const CONTROL: Modifiers = Modifiers {
    control: true,
    ..NONE
};
const ALT: Modifiers = Modifiers { alt: true, ..NONE };
const META: Modifiers = Modifiers { meta: true, ..NONE };
const CONTROL_ALT: Modifiers = Modifiers {
    control: true,
    alt: true,
    ..NONE
};

/// A key going down at `code`, which typed `text` (as the platform's text input
/// gave it) and types `unmodified` with no modifiers in the keyboard's layout.
fn press(
    code: KeyCode,
    text: Option<&str>,
    unmodified: Option<char>,
    mods: Modifiers,
) -> SurfaceInput {
    SurfaceInput::Key {
        code,
        native: 0,
        pressed: true,
        repeat: false,
        modifiers: mods,
        text: text.map(str::to_owned),
        unmodified,
    }
}

fn note(input: SurfaceInput) -> Option<String> {
    encode_input(&input, false)
}

/// A key that types no character (Escape, an arrow, …).
fn named(code: KeyCode, mods: Modifiers) -> Option<String> {
    note(press(code, None, None, mods))
}

#[test]
fn plain_char_is_itself() {
    assert_eq!(
        note(press(KeyCode::KeyA, Some("a"), Some('a'), NONE)).as_deref(),
        Some("a")
    );
}

#[test]
fn shifted_char_passes_through() {
    // The platform folds Shift into the text, so it arrives uppercased and is sent
    // literally (no separate modifier).
    assert_eq!(
        note(press(KeyCode::KeyA, Some("A"), Some('a'), SHIFT)).as_deref(),
        Some("A")
    );
    assert_eq!(
        note(press(KeyCode::Digit1, Some("!"), Some('1'), SHIFT)).as_deref(),
        Some("!")
    );
}

#[test]
fn special_keys_use_angle_notation() {
    assert_eq!(named(KeyCode::Escape, NONE).as_deref(), Some("<Esc>"));
    assert_eq!(named(KeyCode::Enter, NONE).as_deref(), Some("<CR>"));
    assert_eq!(named(KeyCode::NumpadEnter, NONE).as_deref(), Some("<CR>"));
    assert_eq!(named(KeyCode::Backspace, NONE).as_deref(), Some("<BS>"));
    assert_eq!(named(KeyCode::Tab, NONE).as_deref(), Some("<Tab>"));
    assert_eq!(
        note(press(KeyCode::Space, Some(" "), Some(' '), NONE)).as_deref(),
        Some(" ")
    );
}

#[test]
fn shift_tab_is_s_tab_notation() {
    // Shift+Tab types nothing: it's `Tab` with Shift held. Shift must be notated for
    // named keys so it reaches the server as `<S-Tab>` (the cmdline wildmenu /
    // snippet tabstop backward key), not a plain `<Tab>`.
    assert_eq!(named(KeyCode::Tab, SHIFT).as_deref(), Some("<S-Tab>"));
}

#[test]
fn ctrl_and_alt_get_prefixed() {
    // A chord types nothing; its character is the key's own in the layout.
    assert_eq!(
        note(press(KeyCode::KeyW, None, Some('w'), CONTROL)).as_deref(),
        Some("<C-w>")
    );
    assert_eq!(
        note(press(KeyCode::KeyX, None, Some('x'), ALT)).as_deref(),
        Some("<A-x>")
    );
    // The layout's character, not the key's US place: the key right of Tab is `z`
    // on a German keyboard, so Ctrl+that key is `<C-z>`.
    assert_eq!(
        note(press(KeyCode::KeyY, None, Some('z'), CONTROL)).as_deref(),
        Some("<C-z>")
    );
    // Shift with a chord is its own modifier (`<C-S-c>`), as the TUI's kitty path has it.
    assert_eq!(
        note(press(
            KeyCode::KeyC,
            None,
            Some('c'),
            Modifiers {
                shift: true,
                ..CONTROL
            }
        ))
        .as_deref(),
        Some("<C-S-c>")
    );
}

#[test]
fn alt_chord_ignores_text_the_toolkit_reports_with_it() {
    // Some toolkits (Qt) report the letter as text with Alt held. Alt+x is still the
    // `<A-x>` chord, not typing.
    assert_eq!(
        note(press(KeyCode::KeyX, Some("x"), Some('x'), ALT)).as_deref(),
        Some("<A-x>")
    );
}

#[test]
fn ctrl_chords_that_alias_a_named_key_stay_ctrl_chords() {
    // Ctrl+I / Ctrl+M / Ctrl+[ / Ctrl+H are their own keys with Control, distinct from
    // Tab/Enter/Escape/Backspace — so the GUI sends `<C-i>` &c., not the named twin. A
    // native window always disambiguates, and the client declares the keyboard
    // protocol active at attach, so the server keeps these apart from
    // `<Tab>`/`<CR>`/`<Esc>`/`<BS>` (see the keymap tests).
    for (code, c, want) in [
        (KeyCode::KeyI, 'i', "<C-i>"),
        (KeyCode::KeyM, 'm', "<C-m>"),
        (KeyCode::BracketLeft, '[', "<C-[>"),
        (KeyCode::KeyH, 'h', "<C-h>"),
    ] {
        assert_eq!(
            note(press(code, None, Some(c), CONTROL)).as_deref(),
            Some(want)
        );
    }
    // ...while the named keys themselves are still their own notation.
    assert_eq!(named(KeyCode::Tab, NONE).as_deref(), Some("<Tab>"));
}

#[test]
fn several_typed_characters_are_sent_literally() {
    // A dead key that doesn't compose with the next key types both (`´` then `x`
    // types `´x`): the whole text must reach the server verbatim — not be silently
    // truncated to the first char.
    assert_eq!(
        note(press(KeyCode::KeyX, Some("´x"), Some('x'), NONE)).as_deref(),
        Some("´x")
    );
    // `<` is still escaped so several characters can't smuggle a `<...>` form.
    assert_eq!(
        note(press(KeyCode::KeyA, Some("a<b"), Some('a'), NONE)).as_deref(),
        Some("a<lt>b")
    );
}

#[test]
fn dead_keys_type_once_composed() {
    // The dead key itself types nothing; the next key types the composed letter.
    assert_eq!(
        note(press(KeyCode::BracketLeft, None, Some('´'), NONE)),
        None
    );
    assert_eq!(
        note(press(KeyCode::KeyE, Some("é"), Some('e'), NONE)).as_deref(),
        Some("é")
    );
}

#[test]
fn literal_less_than_is_escaped() {
    assert_eq!(
        note(press(KeyCode::Comma, Some("<"), Some(','), SHIFT)).as_deref(),
        Some("<lt>")
    );
}

#[test]
fn navigation_keys_use_angle_notation() {
    assert_eq!(named(KeyCode::ArrowLeft, NONE).as_deref(), Some("<Left>"));
    assert_eq!(
        named(KeyCode::PageDown, NONE).as_deref(),
        Some("<PageDown>")
    );
    assert_eq!(
        named(KeyCode::ArrowLeft, CONTROL).as_deref(),
        Some("<C-Left>")
    );
}

#[test]
fn altgr_typing_is_typing_not_a_chord() {
    // Windows reports AltGr as Ctrl+Alt: `AltGr+E` on a European layout arrives with
    // CONTROL|ALT set — but the platform says it *typed* `€`. That's typing: the
    // client must send the €, not the `<C-A-e>` chord that would swallow it.
    assert_eq!(
        note(press(KeyCode::KeyE, Some("€"), Some('e'), CONTROL_ALT)).as_deref(),
        Some("€")
    );
    // AltGr+Q on ABNT2 types `/`; with Shift too (AltGr+Shift) it's still typing.
    assert_eq!(
        note(press(KeyCode::KeyQ, Some("/"), Some('q'), CONTROL_ALT)).as_deref(),
        Some("/")
    );
    assert_eq!(
        note(press(
            KeyCode::KeyQ,
            Some("Q"),
            Some('q'),
            Modifiers {
                shift: true,
                ..CONTROL_ALT
            }
        ))
        .as_deref(),
        Some("Q")
    );
    // A real Ctrl+Alt chord types nothing → chord behavior.
    assert_eq!(
        note(press(KeyCode::KeyE, None, Some('e'), CONTROL_ALT)).as_deref(),
        Some("<C-A-e>")
    );
}

#[test]
fn macos_option_makes_chords() {
    // On macOS Option composes (Option+C types `ç`), but `<A-c>` bindings must work
    // there too: with `option_chords`, Option is a chord of the key's own character.
    let option_c = press(KeyCode::KeyC, Some("ç"), Some('c'), ALT);
    assert_eq!(encode_input(&option_c, true).as_deref(), Some("<A-c>"));
    // Elsewhere a Ctrl+Alt key that typed text is AltGr typing; on macOS Control+
    // Option is a chord too.
    let control_option = press(KeyCode::KeyE, Some("´"), Some('e'), CONTROL_ALT);
    assert_eq!(
        encode_input(&control_option, true).as_deref(),
        Some("<C-A-e>")
    );
}

#[test]
fn input_method_commits_are_typed() {
    assert_eq!(
        note(SurfaceInput::Text("日本".to_owned())).as_deref(),
        Some("日本")
    );
    assert_eq!(
        note(SurfaceInput::Text("<".to_owned())).as_deref(),
        Some("<lt>")
    );
}

/// An input method taking back what keys typed (macOS's accents, offered as a
/// key is held, replace the letter it typed) deletes it around the cursor.
#[test]
fn input_method_replacements_delete_what_they_replace() {
    let delete = |before, after| note(SurfaceInput::DeleteSurrounding { before, after });
    assert_eq!(delete(1, 0).as_deref(), Some("<BS>"));
    assert_eq!(delete(2, 1).as_deref(), Some("<BS><BS><Del>"));
    assert_eq!(delete(0, 0), None);
}

#[test]
fn bare_modifier_and_release_are_dropped() {
    // A lone modifier key (Control pressed by itself) has no editor meaning.
    assert_eq!(named(KeyCode::ControlLeft, CONTROL), None);
    // Nor does a release.
    let release = SurfaceInput::Key {
        code: KeyCode::KeyA,
        native: 0,
        pressed: false,
        repeat: false,
        modifiers: NONE,
        text: None,
        unmodified: Some('a'),
    };
    assert_eq!(note(release), None);
}

#[test]
fn command_chords_are_d_notation() {
    // Command (macOS) / Super is neovim's `D-`: the key's own character, Shift as
    // `S-`, and the named keys wrapped like any other modifier.
    assert_eq!(
        note(press(KeyCode::KeyC, None, Some('c'), META)),
        Some("<D-c>".into())
    );
    let meta_shift = Modifiers {
        shift: true,
        ..META
    };
    assert_eq!(
        note(press(KeyCode::KeyZ, None, Some('z'), meta_shift)),
        Some("<D-S-z>".into())
    );
    assert_eq!(named(KeyCode::ArrowLeft, META), Some("<D-Left>".into()));
    assert_eq!(named(KeyCode::ArrowUp, meta_shift), Some("<D-S-Up>".into()));
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
fn config_font_takes_a_comma_separated_fallback_list() {
    // `--font` / `BEMTVI_GUI_FONT` accepts the same comma list as `guifont`: the
    // primary first, then the fallback chain (each trimmed, `\ ` unescaped).
    let mut cfg = GuiConfig::default();
    assert!(cfg.fonts.is_empty()); // default = system monospace

    cfg.set_font("JetBrains\\ Mono, Noto Color Emoji ,Symbols Nerd Font[scale=0.9]");
    assert_eq!(
        cfg.fonts,
        vec![
            FontSpec::named("JetBrains Mono"),
            FontSpec::named("Noto Color Emoji"),
            FontSpec {
                scale: Some(0.9),
                ..FontSpec::named("Symbols Nerd Font")
            },
        ]
    );

    // An all-blank spec leaves the existing list untouched (keeps the default rather
    // than asking the font system for `""`).
    cfg.set_font("  , ");
    assert_eq!(cfg.fonts.len(), 3);
    // So does one that doesn't parse (it says why on stderr).
    cfg.set_font("Mono[wieght=500]");
    assert_eq!(cfg.fonts.len(), 3);
}

#[test]
fn paste_gestures_are_recognized_and_dont_shadow_ctrl_v() {
    // Cmd+V (macOS), Ctrl+Shift+V (Linux/Windows), and Shift+Insert all paste.
    assert!(is_paste(KeyCode::KeyV, Some('v'), META));
    assert!(is_paste(
        KeyCode::KeyV,
        Some('v'),
        Modifiers {
            shift: true,
            ..CONTROL
        }
    ));
    assert!(is_paste(KeyCode::Insert, None, SHIFT));
    // V is the layout's: on Dvorak it's where US has `.`.
    assert!(is_paste(KeyCode::Period, Some('v'), META));
    // Plain `v`, vim's `<C-v>` (literal-insert / blockwise visual), and a bare
    // Insert must NOT be treated as paste.
    assert!(!is_paste(KeyCode::KeyV, Some('v'), NONE));
    assert!(!is_paste(KeyCode::KeyV, Some('v'), CONTROL));
    assert!(!is_paste(KeyCode::Insert, None, NONE));
}

#[test]
fn ime_committed_text_encodes_multibyte_verbatim() {
    // Several typed characters (a dead key that didn't compose, an input method's
    // commit, CJK) go through `encode_text`. The committed
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
