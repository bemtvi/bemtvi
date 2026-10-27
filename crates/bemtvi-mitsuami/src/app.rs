//! The editor's surface in a mitsuami view: the client behind it, and what carries out
//! its effects on mitsuami.

use std::cell::RefCell;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bemtvi_gpu::{report_session_error, Answer, Client, CursorShape, Effect};
use mitsuami::prelude::{
    clipboard_text, node_ref, open_file, save_file, signal, sleep, spawn_local, Cursor,
    ElementBuilder, GpuSurface, OpenFile, Rect, SaveFile, Signal, SurfaceHandle, SurfaceSize,
    SystemCursor,
};
use mitsuami::raw_window_handle::HasWindowHandle;

use crate::{editor, input};

/// The editor's surface in a mitsuami view: the toolkit-free [`Client`] behind it, and
/// what carries out the client's [`Effect`]s on mitsuami — the surface's signals (the
/// pointer, the input method's caret and switch, the backdrop), its dialogs and
/// clipboard, the host's callbacks — plus the UI-thread tasks that drive the client: the
/// pump on the IO thread's wake, the paint loop and the timers.
pub(crate) struct App {
    pub(crate) client: Client,
    /// The editor's surface, once mitsuami made it. The renderer is built on it at its
    /// first non-empty size.
    surface: Option<SurfaceHandle>,
    /// The surface, and the UI it's in, for the host to focus.
    pub(crate) node: Option<(mitsuami::prelude::NodeRef, mitsuami::prelude::Ui)>,
    /// What the editor's host does when the editor quits (`bemtvi_exit`, or the
    /// connection closed): `bemtvi-gui` closes its window. Called once.
    on_exit: Rc<dyn Fn()>,
    /// What the host does with the editor's events (`btv.host.send`), if it listens.
    on_event: Option<Rc<dyn Fn(editor::EditorEvent)>>,
    /// The surface's own state, made in its reactive scope: the pointer's cursor over
    /// the surface (a resize arrow over a split separator), the caret the input
    /// method's candidate window opens by, whether input methods are on (unless
    /// `'imdisable'`), and whether the window's backdrop is under the surface (an
    /// opacity below `100`).
    cursor: Option<Signal<Cursor>>,
    caret: Option<Signal<Option<Rect>>>,
    input_method: Option<Signal<bool>>,
    transparent: Option<Signal<bool>>,
    /// Wakes the paint loop on an [`Effect::Redraw`]: any number of them, one paint.
    redraw: Rc<tokio::sync::Notify>,
    /// Wakes the timer task when the client's next deadline may have moved.
    timer: Rc<tokio::sync::Notify>,
}

impl App {
    pub(crate) fn new(
        client: Client,
        on_exit: Rc<dyn Fn()>,
        on_event: Option<Rc<dyn Fn(editor::EditorEvent)>>,
    ) -> Rc<RefCell<Self>> {
        Rc::new(RefCell::new(Self {
            client,
            surface: None,
            node: None,
            on_exit,
            on_event,
            cursor: None,
            caret: None,
            input_method: None,
            transparent: None,
            redraw: Rc::new(tokio::sync::Notify::new()),
            timer: Rc::new(tokio::sync::Notify::new()),
        }))
    }

    /// Call into the client, then carry out what it asked for. The effects run with the
    /// app let go of, so a host callback (`on_exit`, `on_event`) can drive the editor
    /// again from in there (through its `EditorHandle`).
    fn drive(this: &Rc<RefCell<Self>>, call: impl FnOnce(&mut Client)) {
        let effects = {
            let mut app = this.borrow_mut();
            call(&mut app.client);
            app.client.take_effects()
        };
        // The call may have moved the next deadline.
        this.borrow().timer.notify_one();
        for effect in effects {
            Self::effect(this, effect);
            // A host callback (`on_exit`, `on_event`) may have unmounted the editor,
            // disposing the surface's signals with its scope: what's left of the batch
            // was for a surface that's gone, and setting a disposed signal panics.
            if !this.borrow().mounted() {
                return;
            }
        }
    }

    /// Whether the editor's reactive scope is still alive: its surface's signals were
    /// made in it, so they go with it. True before the surface is made (nothing can
    /// drive the app then), false once the editor is unmounted; a remount is another
    /// `App`.
    fn mounted(&self) -> bool {
        self.cursor.is_none_or(|cursor| cursor.is_alive())
    }

    fn effect(this: &Rc<RefCell<Self>>, effect: Effect) {
        let app = this.borrow();
        match effect {
            Effect::Redraw => app.redraw.notify_one(),
            Effect::Cursor(shape) => {
                if let Some(cursor) = app.cursor {
                    cursor.set(match shape {
                        CursorShape::ResizeHorizontal => {
                            Cursor::System(SystemCursor::ResizeEastWest)
                        }
                        CursorShape::ResizeVertical => {
                            Cursor::System(SystemCursor::ResizeNorthSouth)
                        }
                        CursorShape::Text => Cursor::System(SystemCursor::Text),
                        CursorShape::Default => Cursor::Default,
                    });
                }
            }
            Effect::ImeArea(area) => {
                if let Some(caret) = app.caret {
                    caret.set(area.map(|a| Rect::new(a.x, a.y, a.width, a.height)));
                }
            }
            Effect::InputMethod(on) => {
                if let Some(input_method) = app.input_method {
                    input_method.set(on);
                }
            }
            Effect::Transparent(on) => {
                if let Some(transparent) = app.transparent {
                    transparent.set(on);
                }
            }
            // The platform's own dialog, on the window (a sheet on macOS); it doesn't
            // block the UI thread, and the client hears the answer when it comes.
            Effect::OpenDialog {
                request,
                title,
                start_dir,
            } => {
                let mut dialog = OpenFile::new().title(title);
                if let Some(dir) = start_dir {
                    dialog = dialog.start_folder(dir);
                }
                let me = Rc::downgrade(this);
                spawn_local(async move {
                    let picked = open_file(dialog)
                        .await
                        .and_then(|paths| paths.into_iter().next());
                    if let Some(app) = me.upgrade() {
                        Self::drive(&app, |c| c.answer(request, Answer::Path(picked)));
                    }
                });
            }
            Effect::SaveDialog { request, title } => {
                let me = Rc::downgrade(this);
                spawn_local(async move {
                    let picked = save_file(SaveFile::new().title(title)).await;
                    if let Some(app) = me.upgrade() {
                        Self::drive(&app, |c| c.answer(request, Answer::Path(picked)));
                    }
                });
            }
            // The platform's clipboard, through mitsuami.
            Effect::ReadClipboard { request } => {
                let me = Rc::downgrade(this);
                spawn_local(async move {
                    let text = clipboard_text().await;
                    if let Some(app) = me.upgrade() {
                        Self::drive(&app, |c| c.answer(request, Answer::Text(text)));
                    }
                });
            }
            Effect::Exit => {
                let on_exit = app.on_exit.clone();
                drop(app);
                on_exit();
            }
            Effect::Host { name, payload } => match app.on_event.clone() {
                Some(on_event) => {
                    drop(app);
                    on_event(editor::EditorEvent::Host { name, payload });
                }
                // Sent to an app that isn't listening: say so where it was sent from,
                // rather than let the message vanish.
                None => {
                    if let Ok(rpc) = app.client.handle().rpc() {
                        report_session_error(
                            &rpc,
                            "btv.host.send",
                            &anyhow::anyhow!(
                                "{name:?} went nowhere: the app hears no editor events"
                            ),
                        );
                    }
                }
            },
        }
    }

    /// The editor's surface, taking the space its row or column has left. Built in the
    /// editor's own reactive scope (see [`editor::Editor`]), where it starts the UI-thread
    /// tasks — the pump on the IO thread's `wake`, the paint loop and the timers — which
    /// end with that scope. `autofocus` focuses it as it's made, so keys go to it from
    /// the start.
    pub(crate) fn surface(
        this: Rc<RefCell<Self>>,
        wake: Arc<tokio::sync::Notify>,
        ui: &mitsuami::prelude::Ui,
        label: String,
        autofocus: bool,
    ) -> GpuSurface {
        let cursor = signal(Cursor::Default);
        let caret = signal(None::<Rect>);
        let input_method = signal(true);
        let transparent = signal(false);
        {
            let mut app = this.borrow_mut();
            app.cursor = Some(cursor);
            app.caret = Some(caret);
            app.input_method = Some(input_method);
            app.transparent = Some(transparent);
        }
        // The tasks hold the app weakly: the surface and the scope's cleanup keep it
        // alive while it's mounted. A task that unmounts the editor itself (the pump,
        // calling an `on_exit` that closes the pane) isn't cancelled by that — it's
        // the one running — so a strong hold would keep the client forever. That task
        // ends itself once the editor is unmounted, or it would wait on a `wake` that
        // never comes again (see `pump_loop`).
        spawn_local(Self::pump_loop(Rc::downgrade(&this), wake));
        spawn_local(Self::paint_loop(Rc::downgrade(&this)));
        spawn_local(Self::timer_loop(Rc::downgrade(&this)));

        let surface = node_ref();
        if autofocus {
            surface.focus();
        }
        this.borrow_mut().node = Some((surface, ui.clone()));
        let (ready, resized, input) = (this.clone(), this.clone(), this);
        GpuSurface::new()
            .label(label)
            .node_ref(surface)
            // It has no size of its own: it takes the space its row or column has
            // left (stretched across the other way), so it sits beside an app's own
            // controls as well as filling `bemtvi-gui`'s window.
            .grow(1.0)
            // Text as a text field takes it: layouts, dead keys, AltGr and
            // input methods (composed accents, CJK) all type — input methods
            // unless a config sets `'imdisable'`.
            .text_input(true)
            .input_method(input_method)
            // The window's backdrop under it, for `'guiopacity'` to show
            // (macOS's vibrancy; Windows' Mica is always there).
            .transparent(transparent)
            .text_cursor(caret)
            .cursor(cursor)
            .on_ready(move |handle| ready.borrow_mut().surface = Some(handle))
            .on_resize(move |size| Self::resized(&resized, size))
            // In the client's own types; what it has no use for (the pointer leaving, raw
            // motion) stops here.
            .on_input(move |event| {
                if let Some(event) = input::input_event(&event) {
                    Self::drive(&input, |c| c.input(&event));
                }
            })
    }

    /// Drains what the IO thread queued, each time it says there is some.
    /// The only task that can unmount the editor (an `on_exit` or `on_event` it calls
    /// may close the pane), and so the only one its scope can't cancel: it's running.
    async fn pump_loop(this: Weak<RefCell<Self>>, wake: Arc<tokio::sync::Notify>) {
        loop {
            wake.notified().await;
            let Some(app) = this.upgrade() else { return };
            Self::drive(&app, Client::pump);
            if !app.borrow().mounted() {
                return;
            }
        }
    }

    /// Paints whenever a repaint was asked for (any number of requests, one paint),
    /// and every 8 ms while a scroll slide runs: `present` (Fifo vsync) then paces the
    /// actual paint to the display refresh.
    async fn paint_loop(this: Weak<RefCell<Self>>) {
        let Some(wake) = this.upgrade().map(|app| app.borrow().redraw.clone()) else {
            return;
        };
        loop {
            wake.notified().await;
            loop {
                let Some(app) = this.upgrade() else { return };
                let mut sliding = false;
                Self::drive(&app, |c| sliding = c.paint());
                drop(app);
                if !sliding {
                    break;
                }
                sleep(Duration::from_millis(8)).await;
            }
        }
    }

    /// Sleeps until the client's next deadline (the `timeoutlen` flush, a drag's
    /// auto-scroll, the caret's blink) and ticks it; any call into the client may move
    /// the deadline, so a nudge restarts the wait.
    async fn timer_loop(this: Weak<RefCell<Self>>) {
        let Some(nudge) = this.upgrade().map(|app| app.borrow().timer.clone()) else {
            return;
        };
        loop {
            let Some(deadline) = this
                .upgrade()
                .map(|app| app.borrow().client.next_deadline())
            else {
                return;
            };
            match deadline {
                Some(deadline) => {
                    let wait = deadline.saturating_duration_since(Instant::now());
                    tokio::select! {
                        _ = sleep(wait) => {}
                        _ = nudge.notified() => continue,
                    }
                }
                None => {
                    nudge.notified().await;
                    continue;
                }
            }
            let Some(app) = this.upgrade() else { return };
            Self::drive(&app, |c| c.tick(Instant::now()));
        }
    }

    /// The surface's size in pixels, or its scale, changed. The first non-empty size
    /// builds the renderer on the surface — unless it has no window to present to
    /// (mitsuami's headless backend, which the embed tests run on): the editor then
    /// runs (its server, the host's handle) without a renderer.
    fn resized(this: &Rc<RefCell<Self>>, size: SurfaceSize) {
        if size.is_empty() {
            return;
        }
        let surface = this.borrow().surface.clone();
        Self::drive(this, |c| {
            if c.has_renderer() {
                c.resize(gpu_size(size));
                return;
            }
            let Some(surface) = surface.filter(|s| s.window_handle().is_ok()) else {
                return;
            };
            if let Err(e) = c.attach_surface(surface, gpu_size(size)) {
                // The client quit the session (with nothing to show, a blank window
                // shouldn't stay up); say why.
                eprintln!("bemtvi-mitsuami: {e:#}");
            }
        });
    }
}

/// mitsuami's surface size as the renderer takes it.
fn gpu_size(size: SurfaceSize) -> bemtvi_gpu::SurfaceSize {
    bemtvi_gpu::SurfaceSize {
        width: size.width,
        height: size.height,
        scale: size.scale,
    }
}
