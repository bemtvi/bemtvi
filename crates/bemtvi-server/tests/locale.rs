//! Lua's numbers don't follow the process's numeric locale. GTK sets the whole
//! process to the user's locale when the GUI starts (`setlocale(LC_ALL, "")`), and
//! PUC Lua formats and reads numbers through the C library, so under a decimal-comma
//! locale `tostring(0.9)` was `"0,9"`: `btv.ui.font` wrote `scale=0,9` into
//! `'guifont'` (E474), and any plugin formatting a number into JSON or a command
//! broke the same way.
//!
//! Its own test binary, since it changes the locale of the whole test process the
//! way GTK does.
#![cfg(unix)]

use bemtvi_server::ServerInit;
use bemtvi_test_harness::{exec_lua, start_attached};

/// Switch the process's numeric locale to one with a decimal comma, as GTK does
/// with the user's; `false` when the machine has none installed.
fn comma_locale() -> bool {
    for name in [
        "pt_BR.UTF-8",
        "pt_BR.utf8",
        "de_DE.UTF-8",
        "de_DE.utf8",
        "fr_FR.UTF-8",
    ] {
        let c = std::ffi::CString::new(name).unwrap();
        let set = unsafe { libc::setlocale(libc::LC_NUMERIC, c.as_ptr()) };
        if !set.is_null() {
            return true;
        }
    }
    false
}

#[tokio::test]
async fn lua_numbers_use_a_decimal_point_whatever_the_process_locale() {
    if !comma_locale() {
        eprintln!("skip: no decimal-comma locale installed (pt_BR, de_DE, fr_FR)");
        return;
    }
    let (rpc, _incoming) = start_attached(ServerInit::default(), 80, 24).await;
    let got = exec_lua(
        &rpc,
        "return table.concat({ \
           tostring(0.5), \
           tostring(tonumber('0.25')), \
           string.format('%.1f', 1.5), \
           tostring('2.5' + 1), \
           tostring(load('return 0.75')()), \
         }, '|')",
    )
    .await;
    assert_eq!(got.as_str(), Some("0.5|0.25|1.5|3.5|0.75"));
    let value = exec_lua(
        &rpc,
        "return btv.ui.font({ { 'Symbols Nerd Font Mono', scale = 0.9 } })",
    )
    .await;
    assert_eq!(value.as_str(), Some("Symbols Nerd Font Mono[scale=0.9]"));
}
