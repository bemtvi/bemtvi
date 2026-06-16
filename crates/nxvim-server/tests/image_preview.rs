//! `'imagepreview'` policy + protocol (Phase 1): with the option on, opening an
//! image file marks an inert preview buffer — its bytes are *not* read as text —
//! and the redraw window carries an `image` reference (the path) the client renders
//! as a picture. With the option off, the same file opens as ordinary text and
//! carries no marker. (The actual pixel rendering is client-side and verified
//! manually — Phase 2; this asserts the server-side behavior end-to-end.)

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{
    command, exec_lua, lines, map_get, start_attached, wait_redraw, window0_field, write_temp,
};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(ServerInit::default(), 80, 24).await
}

#[tokio::test]
async fn image_file_previews_when_enabled() {
    let (rpc, mut incoming) = start().await;
    let path = write_temp("imgprev", "png", "PNGPLACEHOLDER\n");

    // The config knob (`nx.o` is canonical; `vim.o` is the alias) turns it on.
    exec_lua(&rpc, "nx.o.imagepreview = true").await;
    command(&rpc, &format!("edit {path}")).await;

    // The bytes are deliberately NOT read into the rope — the buffer is empty/inert,
    // so a `nvim_buf_get_lines` sees a single empty line, not the file's contents.
    assert_eq!(
        lines(&rpc).await,
        vec![String::new()],
        "an image-preview buffer does not load its bytes as text"
    );

    // The redraw's window carries the `image` marker (a sub-map) with the path the
    // client renders. `wait_redraw` awaits the frame (the open's redraw lands on a
    // later tick) and dodges the take-latest race.
    let frame = wait_redraw(&mut incoming, |m| {
        matches!(window0_field(m, "image"), Some(Value::Map(_)))
    })
    .await;
    let Some(Value::Map(img)) = window0_field(&frame, "image") else {
        panic!("the redraw window carries an image marker");
    };
    assert_eq!(
        map_get(img, "path").and_then(Value::as_str),
        Some(path.as_str()),
        "the image marker carries the opened file's path"
    );
}

#[tokio::test]
async fn image_file_opens_as_text_when_disabled() {
    let (rpc, mut incoming) = start().await;
    let path = write_temp("imgtext", "png", "PNGPLACEHOLDER\n");

    // Default: `'imagepreview'` is off, so an image file is just bytes-as-text.
    command(&rpc, &format!("edit {path}")).await;

    assert_eq!(
        lines(&rpc).await,
        vec!["PNGPLACEHOLDER".to_string()],
        "with imagepreview off, the file loads as ordinary text"
    );

    // And no `image` marker rides the redraw (the server emits Nil for a non-image
    // window; an absent key is equally fine).
    let frame = wait_redraw(&mut incoming, |_| true).await;
    let img = window0_field(&frame, "image");
    assert!(
        matches!(img, None | Some(Value::Nil)),
        "no image marker when imagepreview is off (got {img:?})"
    );
}
