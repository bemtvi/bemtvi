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
    // The marker also carries the file's version (size + mtime), so the client can
    // re-decode when the file changes on disk. "PNGPLACEHOLDER\n" is 15 bytes.
    assert_eq!(
        map_get(img, "size").and_then(Value::as_u64),
        Some(15),
        "the image marker carries the file size"
    );
    assert!(
        map_get(img, "mtime_ms")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            > 0,
        "the image marker carries a nonzero mtime"
    );
}

#[tokio::test]
async fn set_ex_command_enables_imagepreview() {
    // `:set imagepreview` must enable it just like `nx.o.imagepreview = true` does
    // (it was missing from `apply_set_bool`'s slot match, so the `:set` path silently
    // no-op'd and an image still opened as text — the bug this guards).
    let (rpc, mut incoming) = start().await;
    let path = write_temp("imgset", "png", "PNGPLACEHOLDER\n");

    command(&rpc, "set imagepreview").await;
    command(&rpc, &format!("edit {path}")).await;

    assert_eq!(
        lines(&rpc).await,
        vec![String::new()],
        "`:set imagepreview` makes an image open as an inert preview, not text"
    );
    let frame = wait_redraw(&mut incoming, |m| {
        matches!(window0_field(m, "image"), Some(Value::Map(_)))
    })
    .await;
    assert!(
        matches!(window0_field(&frame, "image"), Some(Value::Map(_))),
        "the redraw carries the image marker after `:set imagepreview`"
    );
}

#[tokio::test]
async fn local_session_marks_not_remote_and_reads_bytes_over_the_rpc() {
    // An embedded (local-disk) session shares the filesystem, so the marker says the
    // bytes are *not* remote — the client decodes `path` directly. `nxvim_image_read`
    // still works (it reads local disk), so a client can route through it uniformly.
    let (rpc, mut incoming) = start().await;
    let path = write_temp("imglocal", "png", "PNGPLACEHOLDER\n");

    exec_lua(&rpc, "nx.o.imagepreview = true").await;
    command(&rpc, &format!("edit {path}")).await;

    let frame = wait_redraw(&mut incoming, |m| {
        matches!(window0_field(m, "image"), Some(Value::Map(_)))
    })
    .await;
    let Some(Value::Map(img)) = window0_field(&frame, "image") else {
        panic!("the redraw window carries an image marker");
    };
    assert_eq!(
        map_get(img, "remote").and_then(Value::as_bool),
        Some(false),
        "a local session's image preview is not remote (the client opens the path)"
    );

    // The RPC reads the file off the editor tick and replies with the raw bytes.
    let reply = rpc
        .request("nxvim_image_read", vec![Value::from(path.as_str())])
        .await
        .expect("nxvim_image_read responds");
    assert_eq!(
        reply,
        Value::Binary(b"PNGPLACEHOLDER\n".to_vec()),
        "nxvim_image_read returns the file's raw bytes"
    );

    // A bad path fails loud (the client shows its `[image: …]` placeholder) — never a
    // silent empty image.
    let err = rpc
        .request("nxvim_image_read", vec![Value::from("/no/such/img.png")])
        .await;
    assert!(
        err.is_err(),
        "nxvim_image_read fails loud on an unreadable path (got {err:?})"
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
