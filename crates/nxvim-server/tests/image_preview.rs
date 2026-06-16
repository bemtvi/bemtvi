//! `'imagepreview'` policy + protocol (Phase 1): with the option on, opening an
//! image file marks an inert preview buffer — its bytes are *not* read as text —
//! and the redraw window carries an `image` reference (the path) the client renders
//! as a picture. With the option off, the same file opens as ordinary text and
//! carries no marker. (The actual pixel rendering is client-side and verified
//! manually — Phase 2; this asserts the server-side behavior end-to-end.)

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{
    attach, command, exec_lua, lines, map_get, spawn, start_attached, temp_dir, wait_redraw,
    window0_field, write_temp,
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

#[tokio::test]
async fn cli_file_arg_previews_after_config_enables_it() {
    // The reported flow: `NXVIM_CONFIG=… nxvim photo.png`. The file arg is opened at
    // editor construction — *before* the config runs — so previews are still off at
    // that first open. A config that turns them on must still reconcile that buffer
    // into a preview (otherwise it shows the raw bytes until a manual `:e %`).
    let dir = temp_dir("imgcli");
    std::fs::write(dir.join("init.lua"), "nx.o.imagepreview = true\n").expect("write init.lua");
    let img = dir.join("pic.png");
    std::fs::write(&img, "PNGPLACEHOLDER\n").expect("write image");
    let img_path = img.to_string_lossy().into_owned();

    let (rpc, mut incoming) = spawn(ServerInit {
        file: Some(img_path.clone()),
        config_dir: Some(dir.clone()),
        runtimepath: vec![dir],
        ..Default::default()
    });
    attach(&rpc, 80, 24).await;

    // Reconciled to a preview after the config enabled it: the buffer is empty (bytes
    // not loaded as text) and the redraw carries the image marker for the file arg.
    assert_eq!(
        lines(&rpc).await,
        vec![String::new()],
        "the startup file arg was reconciled to an image preview"
    );
    let frame = wait_redraw(&mut incoming, |m| {
        matches!(window0_field(m, "image"), Some(Value::Map(_)))
    })
    .await;
    let Some(Value::Map(im)) = window0_field(&frame, "image") else {
        panic!("the startup redraw carries an image marker");
    };
    assert_eq!(
        map_get(im, "path").and_then(Value::as_str),
        Some(img_path.as_str()),
        "the marker carries the file-arg path"
    );
}
