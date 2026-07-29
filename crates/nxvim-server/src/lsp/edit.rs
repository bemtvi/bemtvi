//! Buffer-mutating features: applying formatting/rename/workspace edits and
//! code actions (including the command-dispatch and resolve round-trips), plus
//! the byte<->LSP position conversion the apply path uses.

use nxvim_lsp::lsp_types::{Location, Position, Range, TextEdit, Url};
use nxvim_lsp::serde_json;
use nxvim_lsp::{
    ApplyEditOutcome, CodeActionData, LspRequest, PositionEncoding, WorkspaceChange,
    WorkspaceEditData,
};
use nxvim_lua::FsJob;

use super::*;
use crate::EditHost;

impl EditHost {
    /// Convert an LSP [`Range`] (in the negotiated `encoding`) to an absolute
    /// **current-buffer** byte range, resolving each endpoint against its line.
    pub(crate) fn lsp_range_to_bytes(
        &self,
        range: &Range,
        encoding: PositionEncoding,
    ) -> std::ops::Range<usize> {
        lsp_range_to_bytes_in(self.editor.buffer(), range, encoding)
    }

    /// A current-buffer `(row, byte-column)` point as an LSP [`Position`] in the
    /// server's negotiated encoding (Decision 4).
    pub(crate) fn lsp_position(
        &self,
        encoding: PositionEncoding,
        row: usize,
        byte_col: usize,
    ) -> Position {
        lsp_position_in(self.editor.buffer(), encoding, row, byte_col)
    }

    /// Apply whole-document formatting edits to the current buffer (one undo
    /// step) and re-sync so the server's version stays consistent. Empty ⇒ a
    /// brief message (already formatted), so a no-op re-run is visible.
    ///
    /// `encoding` is the **formatting server's** negotiated encoding, not the
    /// buffer's first server's: `format{ name = … }` can pick a server that
    /// negotiated utf-8 on a buffer whose other server negotiated utf-16, and
    /// reading one's columns as the other's shifts every edit on a line with any
    /// multi-byte character.
    pub(crate) fn apply_formatting_edits(
        &mut self,
        edits: Vec<TextEdit>,
        encoding: PositionEncoding,
    ) {
        if edits.is_empty() {
            self.editor.echo(LspReqKind::Formatting.empty_message());
            return;
        }
        let id = self.editor.current_buffer_id();
        let buffer = self.editor.buffer();
        let (byte_edits, endofline) = lsp_edits_to_byte_edits(
            buffer,
            edits.iter().map(|e| (&e.range, e.new_text.as_str())),
            encoding,
        );
        self.editor.apply_edits_to(id, byte_edits);
        // A whole-document format reaches the document's end, so the formatter's own
        // trailing newline (or lack of one) decides `'endofline'` from here on.
        self.set_endofline(id, endofline);
        self.sync_lsp_buffer(id);
    }

    /// Record the `'endofline'` an applied batch of LSP edits implies (see
    /// [`lsp_edits_to_byte_edits`]); `None` — no edit reached the document's end — leaves
    /// the flag alone.
    fn set_endofline(&mut self, id: nxvim_core::BufferId, endofline: Option<bool>) {
        if let Some(eol) = endofline {
            self.editor.set_buffer_option_bool(id, "endofline", eol);
        }
    }

    /// Apply a `WorkspaceEdit` handed up from Lua (`nx.lsp.apply_workspace_edit` /
    /// `vim.lsp.util.apply_workspace_edit`, Phase 7). Normalizes it through the same
    /// path the native rename / code-action replies use, then applies the per-document
    /// edits across the buffers it names. A malformed edit is echoed (loud, per the
    /// no-silent-stubs rule), never silently dropped.
    ///
    /// `encoding` is the caller's `opts.encoding` — the units the edit's `character`
    /// columns are counted in. There is no producing server to ask here (the edit was
    /// built in Lua, or handed over as a command's arguments), so the caller says, and
    /// the default is the protocol's own `utf-16` rather than whatever the current
    /// buffer's first server happens to have negotiated. An unrecognized name is
    /// `utf-16` too, matching [`show_lua_document`](Self::show_lua_document).
    pub(crate) fn apply_lua_workspace_edit(&mut self, edit: serde_json::Value, encoding: &str) {
        // Normalized from the value itself (the typed form has already lost its text
        // edits' `annotationId`s, which decide what gets confirmed) — but an edit that
        // doesn't parse is echoed rather than degraded to an empty one, which would
        // report "No applicable changes" for something we simply couldn't read.
        match nxvim_lsp::try_normalize_workspace_edit_value(&edit) {
            Ok(changes) => {
                // The outcome only matters when a *server* asked us to apply; here Lua
                // did, and anything that failed has already been echoed.
                self.apply_workspace_edit(changes, position_encoding(encoding));
                self.lsp_dirty = true;
            }
            Err(reason) => self.editor.echo(format!("apply_workspace_edit: {reason}")),
        }
    }

    /// Jump to an LSP location handed up from Lua (`vim.lsp.util.show_document`,
    /// Phase 7). Builds a [`Location`] from the URI / position and reuses the native
    /// single-location goto (open the file if needed, then refine the byte column on
    /// the landed line). An invalid URI is echoed rather than silently ignored.
    pub(crate) fn show_lua_document(
        &mut self,
        uri: &str,
        line: u32,
        character: u32,
        encoding: &str,
    ) {
        let Ok(url) = Url::parse(uri) else {
            self.editor
                .echo(format!("show_document: invalid uri: {uri}"));
            return;
        };
        let encoding = position_encoding(encoding);
        let position = Position { line, character };
        let location = Location {
            uri: url,
            range: Range {
                start: position,
                end: position,
            },
        };
        self.jump_to_lsp_location(&location, encoding);
        self.lsp_dirty = true;
    }

    /// Apply a normalized workspace edit (from rename or a code action) across the
    /// files it touches. Each URI is resolved to a buffer: the **open** buffer it
    /// names, else the file is loaded into a buffer on the spot so a project-wide
    /// rename reaches files you haven't visited — the edit lands in memory (the buffer
    /// left modified, saved with `:wa`), exactly as neovim's `apply_text_edits` does
    /// rather than writing straight to disk. Each URI's edits convert to bytes against
    /// *its* buffer, at the originating server's encoding, apply as one undo step,
    /// and re-sync.
    ///
    /// Loading an unopened file differs by session:
    /// - **Local** ([`Editor::ensure_buffer_loaded`]): the file is read synchronously
    ///   and edited inline, here and now.
    /// - **Off-tick** (daemon / web — [`Editor::host_fs_offtick`]): the file's bytes
    ///   cross the wire, so the load is async. The replica buffer is created and its
    ///   fetch enqueued ([`Editor::enqueue_replica_open`]), and these edits are stashed
    ///   in [`pending_replica_edits`](EditHost::pending_replica_edits); they apply when
    ///   the bytes land ([`apply_pending_replica_edit`](EditHost::apply_pending_replica_edit)).
    ///   Applying now would hit an empty buffer the fetch would then clobber.
    ///
    /// A URI whose file can't be brought into a buffer at all (a load failure, or a
    /// URI that doesn't map to a path) is collected and reported loud rather than
    /// silently dropped (the no-silent-stubs rule). An edit that touches — and defers —
    /// nothing applicable reports a brief message. The same collected trouble is
    /// returned as an [`ApplyEditOutcome`], which a server-initiated
    /// `workspace/applyEdit` answers with — so a server learns what did *not* happen
    /// instead of reading an unconditional "applied".
    ///
    /// The changes run **in order**: `documentChanges` is a sequence, and a refactor
    /// that extracts into a new file sends `create <uri>` before the edits that fill
    /// it. Precisely: the buffer-side work (text edits, `create`) happens here, in
    /// order; the file operations run afterwards, one at a time, also in order —
    /// moving bytes can only happen off the editor tick. A document addressed by the
    /// name one of those pending renames will give it is resolved back to the buffer
    /// holding it now ([`rewind_pending_renames`]), which is what keeps the two halves
    /// consistent.
    ///
    /// The three file resource operations split by whether they touch the disk:
    /// - `create` is a buffer, not a file — empty, modified, written by `:w`/`:wa`
    ///   like every other edit here ([`Editor::create_file_buffer`]);
    /// - `rename` / `delete` **must** move real bytes, which can only happen off the
    ///   editor tick, so they are queued on the `nx.fs` `FsJob` seam
    ///   ([`queue_workspace_fs_job`](Self::queue_workspace_fs_job)) and their buffer
    ///   half (rebind / wipe) runs when the result lands
    ///   ([`on_workspace_fs_result`](Self::on_workspace_fs_result)). They are still in
    ///   flight when this returns — hence [`AppliedEdit::pending_ops`].
    ///
    /// `origin_encoding` is the encoding negotiated by the server that **produced**
    /// this edit — every position in a `WorkspaceEdit` is in that one encoding,
    /// including those for target buffers with no server of their own. It is passed
    /// in rather than re-derived from the current buffer because a buffer can carry
    /// several servers at different encodings, and the one that answered is not
    /// necessarily the one listed first.
    pub(crate) fn apply_workspace_edit(
        &mut self,
        edit: WorkspaceEditData,
        origin_encoding: PositionEncoding,
    ) -> AppliedEdit {
        // Every apply gets a group id. The file operations it queues carry it, so a
        // failure can drop the rest of *this* edit's operations — the `abort` strategy
        // nxvim advertises — without touching another edit's, and a server-initiated
        // apply keys its held-back response by the same id (and, below, so does the
        // answer to a confirmation the user hasn't given yet).
        let group = self.next_workspace_group;
        self.next_workspace_group += 1;
        // `changeAnnotations` the changes point at and that ask first: nothing applies
        // until the user says so. The common edit has none and goes straight through.
        let confirmable = edit.confirmable();
        if !confirmable.is_empty() {
            return self.ask_before_applying(group, edit, origin_encoding, confirmable);
        }
        // Numbered here, once: the index travels with its change so a
        // `failedChange` still means the position in the edit the *server* sent, even
        // after a confirmation dropped some of the changes in front of it.
        let changes = edit.changes.into_iter().enumerate().collect();
        self.apply_workspace_changes(group, changes, origin_encoding)
    }

    /// Apply one edit's already-decided changes under an existing `group` — the body
    /// of [`apply_workspace_edit`](Self::apply_workspace_edit), and what a confirmed
    /// edit re-enters with the declined groups filtered out (under the *same* group id,
    /// so a server-initiated apply's held-back response still finds its record).
    ///
    /// Each change carries its index in the **server's** `documentChanges`, not its
    /// position in this list: a confirmation may have removed changes in front of it,
    /// and `failedChange` is only meaningful in the sender's own numbering.
    fn apply_workspace_changes(
        &mut self,
        group: u64,
        changes: Vec<(usize, WorkspaceChange)>,
        origin_encoding: PositionEncoding,
    ) -> AppliedEdit {
        let mut deferred = 0usize;
        let mut queued = 0usize;
        // Files a `create` op actually brought into existence, named in the closing
        // message (and put on disk, empty — see the chain below). A `create`
        // that `ignoreIfExists` turned into "open what is already there" is *not* one of
        // these: reporting "Created" for a file we deliberately left alone would be a
        // claim about something that did not happen.
        let mut created: Vec<String> = Vec::new();
        // Buffers this edit **brought into existence**, and so the ones it takes back
        // out if it aborts. Deliberately not every buffer a `create` reached: one that
        // was already open belongs to the user, and force-deleting it would throw away
        // whatever they had in it.
        let mut created_bufs: Vec<BufferId> = Vec::new();
        // Buffers a `create` is *filling*: to the server their document is **empty** —
        // it authored these edits against a file that does not exist — so the fill
        // consumes the rope's phantom newline (see the note at the apply below). A
        // superset of `created_bufs`, because a `create` that `ignoreIfExists` turned
        // out to be a real create after all may have reached its (empty, never-written)
        // buffer rather than made one.
        let mut filled_bufs: Vec<BufferId> = Vec::new();
        // Text edits **resolved but not yet applied**. Resolving every document first
        // is what makes the common failure — a URI that maps to no file we can open —
        // abort with *nothing* applied instead of half the refactor; the changes are
        // applied below, in order, once the whole list resolved.
        let mut staged: Vec<(BufferId, Vec<TextEdit>, bool)> = Vec::new();
        // The change that failed to resolve, by index — `abort` stops there.
        let mut failure: Option<(usize, String)> = None;
        // The `rename`s this edit has queued so far, `(from, to)` in the order sent.
        // They only *run* after the text edits below (moving bytes is off-tick), so a
        // later change addressing a document by its **new** name must be rewound
        // through them to reach the buffer that still holds it — see
        // [`rewind_pending_renames`].
        let mut pending_renames: Vec<(PathBuf, PathBuf)> = Vec::new();

        for (index, change) in changes {
            let (uri, edits) = match change {
                WorkspaceChange::Edits { uri, edits, .. } => (uri, edits),
                WorkspaceChange::Create {
                    uri,
                    overwrite,
                    ignore_if_exists,
                    ..
                } => {
                    let Some(path) = uri_to_path(&uri) else {
                        failure = Some((index, format!("could not open {uri}")));
                        break;
                    };
                    // A URI ending in `/` names a **directory**, not a file — the
                    // protocol's spelling for a folder create. There is nothing to open:
                    // make it on the same ordered fs seam (recursive, so it is
                    // idempotent and brings its parents), and a later `create` in this
                    // same edit can then put a file inside it.
                    if uri.path().ends_with('/') {
                        self.queue_workspace_fs_job(
                            group,
                            index,
                            FsJob::Mkdir {
                                path: path.to_string_lossy().into_owned(),
                                recursive: true,
                                mode: 0o755,
                            },
                            WorkspaceFsOp::MakeDir { dir: path },
                        );
                        queued += 1;
                        continue;
                    }
                    let name = self.buffer_path_for(&path);
                    // Protocol: `overwrite` wins over `ignoreIfExists`. Told to leave an
                    // existing file alone, bring it in the ordinary way instead, so the
                    // edits that follow land on its real content.
                    let spare_existing = ignore_if_exists && !overwrite;
                    let existing = spare_existing
                        .then(|| {
                            self.buffer_id_for_uri(&uri)
                                .or_else(|| self.editor.find_buffer_by_path(&path))
                                .or_else(|| {
                                    if self.editor.host_fs_offtick() {
                                        // Off-tick (daemon / browser): nothing on the
                                        // editor tick can see whether the file is there,
                                        // and `ensure_buffer_loaded` says so by returning
                                        // `None` — which used to fall through to "create
                                        // it empty", clobbering the very file we were
                                        // told to spare. So *ask*: the replica fetch is
                                        // the probe. Its answer decides both the content
                                        // the following edits land on and whether this
                                        // was really a create (see
                                        // `settle_workspace_create`, which runs when the
                                        // bytes — or their absence — arrive).
                                        let id = self.editor.enqueue_replica_open(&name);
                                        self.pending_create_writes.insert(id, (group, index));
                                        Some(id)
                                    } else {
                                        self.editor.ensure_buffer_loaded(&name)
                                    }
                                })
                        })
                        .flatten();
                    let id = match existing {
                        Some(id) => id,
                        None => {
                            let id = self.editor.create_file_buffer(&name);
                            // A *freshly emptied* buffer: it needs the phantom-newline
                            // handling below, it is the one whose file goes on disk, and
                            // it is the one dropped again if the edit aborts.
                            created_bufs.push(id);
                            filled_bufs.push(id);
                            self.queue_created_file_write(group, index, id, &path);
                            queued += 1;
                            created.push(name.display().to_string());
                            id
                        }
                    };
                    // `ignoreIfExists` over a file that turns out **not** to exist is a
                    // plain create, and a create puts the file on disk. Locally that is
                    // knowable right here — an absent file gives a buffer with no disk
                    // baseline; off-tick the same question is answered when the probe
                    // above lands.
                    if spare_existing
                        && !filled_bufs.contains(&id)
                        && !self.pending_create_writes.contains_key(&id)
                        && self
                            .editor
                            .buffer_of(id)
                            .is_some_and(|b| b.disk_stat().is_none())
                    {
                        // A create after all, so the fill has an empty document to land
                        // in — phantom newline and all. `ensure_buffer_loaded` hands back
                        // an empty rope (`"\n"`) for a file that isn't there, exactly as
                        // `create_file_buffer` does, and without this the fill inserts
                        // *before* that newline and the created file gains a spurious
                        // blank last line. (Not `created_bufs`: the buffer may be one the
                        // user opened with `:e` and never wrote, which this edit did not
                        // create and must not delete out from under them on an abort.)
                        filled_bufs.push(id);
                        self.queue_created_file_write(group, index, id, &path);
                        queued += 1;
                        created.push(name.display().to_string());
                    }
                    self.sync_lsp_buffer(id);
                    continue;
                }
                // The two operations that move real bytes on a real filesystem. Both
                // are queued off-tick (the only way they can work identically local,
                // over a daemon and in the browser) and run **one at a time, in this
                // order** — `documentChanges` is a sequence, and `rename a→b` followed
                // by `rename b→c` is nonsense the other way round. `pump_workspace_fs_queue`
                // (at the end of this function) starts the first; each landing in
                // `on_workspace_fs_result` starts the next.
                WorkspaceChange::Rename {
                    old_uri,
                    new_uri,
                    overwrite,
                    ignore_if_exists,
                    ..
                } => {
                    let (Some(from), Some(to)) = (uri_to_path(&old_uri), uri_to_path(&new_uri))
                    else {
                        failure = Some((index, format!("could not open {old_uri} → {new_uri}")));
                        break;
                    };
                    // The buffer takes the short name; the job itself moves the
                    // absolute paths (see `buffer_path_for`).
                    let to_name = self.buffer_path_for(&to);
                    // Protocol: `overwrite` wins over `ignoreIfExists`. Told to spare
                    // an existing destination, probe for it first — the seam's rename
                    // clobbers like `rename(2)`, and the editor tick can't see a
                    // daemon's (or the browser's) filesystem to check synchronously.
                    let (job, op) = if ignore_if_exists && !overwrite {
                        (
                            FsJob::Exists {
                                path: to.to_string_lossy().into_owned(),
                            },
                            WorkspaceFsOp::RenameGuard {
                                from: from.clone(),
                                to: to.clone(),
                                to_name,
                            },
                        )
                    } else {
                        (
                            FsJob::Rename {
                                from: from.to_string_lossy().into_owned(),
                                to: to.to_string_lossy().into_owned(),
                            },
                            WorkspaceFsOp::Rename {
                                from: from.clone(),
                                to: to.clone(),
                                to_name,
                            },
                        )
                    };
                    pending_renames.push((from, to));
                    self.queue_workspace_fs_job(group, index, job, op);
                    queued += 1;
                    continue;
                }
                WorkspaceChange::Delete {
                    uri,
                    recursive,
                    ignore_if_not_exists,
                    ..
                } => {
                    let Some(path) = uri_to_path(&uri) else {
                        failure = Some((index, format!("could not open {uri}")));
                        break;
                    };
                    self.queue_workspace_fs_job(
                        group,
                        index,
                        FsJob::Remove {
                            path: path.to_string_lossy().into_owned(),
                            recursive,
                        },
                        WorkspaceFsOp::Delete {
                            path,
                            ignore_missing: ignore_if_not_exists,
                        },
                    );
                    queued += 1;
                    continue;
                }
            };
            if edits.is_empty() {
                continue;
            }
            // A `rename` queued earlier in this same edit has **not run yet** — moving
            // bytes is off-tick, so every text edit lands first — while the server
            // addresses the document by the name it will have. Rewind through this
            // edit's own moves so the edits reach the buffer that holds the file now;
            // the rename rebinds that buffer when it lands. Without this the lookup
            // below finds nothing, opens a fresh buffer for a file that does not exist,
            // applies the edits to it, and the rename then binds a *second* buffer to
            // the same name — the edits silently lost.
            let uri = rewind_pending_renames(&pending_renames, uri);
            // The open buffer for the URI, else bring its file into one. A URI we
            // can't resolve to a buffer aborts the edit here, before anything has been
            // applied — never silently skipped, and never half-applied around.
            let id = match self.buffer_id_for_uri(&uri) {
                Some(id) => id,
                None => {
                    let Some(path) = uri_to_path(&uri) else {
                        failure = Some((index, format!("could not open {uri}")));
                        break;
                    };
                    // `buffer_id_for_uri` resolves symlinks via `fs::canonicalize`, which
                    // fails for an **off-tick / virtual** path the local disk can't see —
                    // so an already-open replica buffer slips past it. Match by normalized
                    // path here; only a genuinely unopened file needs bringing in.
                    let path = self.buffer_path_for(&path);
                    if let Some(id) = self.editor.find_buffer_by_path(&path) {
                        id
                    } else if self.editor.host_fs_offtick() {
                        // Off-tick: create the replica buffer + enqueue its fetch now.
                        // Its edits defer below, like any other buffer whose bytes are
                        // still in flight.
                        self.editor.enqueue_replica_open(&path)
                    } else {
                        match self.editor.ensure_buffer_loaded(&path) {
                            Some(id) => id,
                            None => {
                                failure = Some((index, format!("could not open {uri}")));
                                break;
                            }
                        }
                    }
                }
            };
            // Bytes still on their way? Then these edits wait for them. That covers the
            // fetch just enqueued above, a fetch an *earlier change of this same edit*
            // started (two changes naming one unopened document, or the `create` probe),
            // and any open already in flight when the edit arrived — in every one of
            // which applying now would write into an empty buffer the landing then
            // clobbers, silently losing the edit.
            let defer = self.editor.has_pending_open(id);
            staged.push((id, edits, defer));
        }

        // A change that could not even be resolved: abort with the edit untouched. The
        // file operations are only *queued* at this point (the pump below is what starts
        // them), so dropping them here means none of them ran; a buffer a `create` made
        // is empty and unwritten, so it goes too. This is as close to transactional as
        // the protocol's `abort` allows — and closer than it promises.
        if let Some((index, reason)) = failure {
            self.drop_workspace_group(group);
            for buffer in created_bufs {
                self.editor.delete_buffer(buffer, true);
            }
            self.editor.echo(format!("apply_workspace_edit: {reason}"));
            return AppliedEdit {
                group,
                pending: 0,
                awaiting_confirm: false,
                outcome: ApplyEditOutcome {
                    applied: false,
                    failure_reason: Some(reason),
                    failed_change: Some(index as u32),
                },
            };
        }

        // Everything resolved — apply the text edits, in order.
        let mut touched = 0usize;
        for (id, edits, defer) in staged {
            if defer {
                self.pending_replica_edits
                    .entry(id)
                    .or_insert_with(|| PendingReplicaEdit {
                        edits: Vec::new(),
                        encoding: origin_encoding,
                    })
                    .edits
                    .extend(edits);
                deferred += 1;
                continue;
            }
            // Always the ORIGIN's encoding, never the target buffer's own server's:
            // one `WorkspaceEdit` is authored end to end by the server that produced
            // it, so every position in it — including those for files served by
            // someone else, or by nobody — is in that one encoding. Re-deriving it
            // per target buffer (which is what this did) picks the target's *first*
            // server, so a rename answered by a utf-16 server shifted every edit on
            // a line with a multi-byte character whenever the buffer's first server
            // was utf-8. It is the same misread `apply_formatting_edits` was fixed
            // for; the rename path applies through here instead.
            let encoding = origin_encoding;
            let Some(buffer) = self.editor.buffer_of(id) else {
                continue;
            };
            // Document coordinates, so the edit that owns the document's tail consumes
            // the rope's phantom newline rather than being inserted before it. That is
            // what a buffer this same edit just `create`d needs: it is an **empty
            // document** to the server — which authored these edits against a file that
            // does not exist — so every position in it maps to byte 0 and a naive fill
            // would leave a spurious blank last line (a pasted `…}\n` landing as
            // `…}\n\n`). The empty document falls out of `'endofline'` being off for a
            // 0-byte file, so no length probe is needed.
            let (byte_edits, endofline) = lsp_edits_to_byte_edits(
                buffer,
                edits.iter().map(|e| (&e.range, e.new_text.as_str())),
                encoding,
            );
            self.editor.apply_edits_to(id, byte_edits);
            self.set_endofline(id, endofline);
            self.sync_lsp_buffer(id);
            touched += 1;
        }

        if !created.is_empty() {
            self.editor.echo(format!("Created {}", created.join(", ")));
        } else if touched == 0 && deferred == 0 && queued == 0 {
            self.editor.echo("No applicable changes");
        }
        // A `create`d file is put on disk **empty**, by the `create`'s own chain of file
        // operations ([`WorkspaceFsOp::CreateDir`] → [`WorkspaceFsOp::CreatePlaceholder`]).
        // That is all the resource operation asks for and all nxvim does: the file exists,
        // and the content the edits just put in its buffer is left modified and unsaved,
        // for you to write like any other edit in this workspace edit — neovim's model.
        //
        // Start the file operations (one at a time, in order).
        self.pump_workspace_fs_queue();
        AppliedEdit {
            group,
            pending: queued,
            awaiting_confirm: false,
            outcome: ApplyEditOutcome {
                applied: true,
                failure_reason: None,
                failed_change: None,
            },
        }
    }

    /// The spelling to *store* on a buffer for a workspace-edit path: relative to the
    /// session's effective directory when the file lives under it, else absolute — the
    /// name `:e <file>` would have given it. Buffer names are shown verbatim (`:ls`,
    /// the statusline, `nx.buf.name`), so a refactor that created or moved a file would
    /// otherwise blare the absolute path at you where every neighbouring buffer is
    /// short. Matching is unaffected: [`Editor::find_buffer_by_path`] compares
    /// cwd-anchored, so the two spellings are the same buffer.
    ///
    /// The **filesystem** side of the same operation always uses the absolute path: it
    /// may run on a daemon, where a relative path would resolve against the daemon's
    /// launch dir rather than the session's cwd.
    fn buffer_path_for(&self, path: &Path) -> PathBuf {
        let win = self.editor.current_window_id();
        let tab = self.editor.current_tab_id();
        let (_, base) = self.dirs.effective(win, tab);
        path.strip_prefix(base)
            .map(Path::to_path_buf)
            .unwrap_or_else(|_| path.to_path_buf())
    }

    /// Hold an edit whose server marked some of its changes `needsConfirmation` and
    /// ask the user, one question per annotation **label** (`groupsOnLabel`, which is
    /// what nxvim advertises) — the whole point of change annotations: a server can
    /// separate the part of a refactor it is sure about from the part a human should
    /// look at, and the second part does not happen behind your back.
    ///
    /// Asking is Lua's job (`nx.lsp._confirm_edit` → `nx.ui.confirm`, a promise chain):
    /// the confirm dialog, its keys and its rendering already live there, it works
    /// identically in every build, and a config can replace the presentation without
    /// touching the engine. The answer comes back as
    /// [`LspOp::WorkspaceEditDecision`](nxvim_lua::LspOp), which
    /// [`on_workspace_edit_decision`](Self::on_workspace_edit_decision) applies.
    ///
    /// A server-initiated `workspace/applyEdit` is left waiting meanwhile (its record
    /// is marked `awaiting_confirm`), because that is the honest answer: the server
    /// asked whether the edit was applied, and until the user says, it hasn't been.
    /// Lua that cannot be reached at all declines the lot rather than leaving both the
    /// user and the server waiting on a question nobody asked.
    fn ask_before_applying(
        &mut self,
        group: u64,
        edit: WorkspaceEditData,
        encoding: PositionEncoding,
        confirmable: Vec<String>,
    ) -> AppliedEdit {
        // One question per distinct label, carrying every annotation id that shares it.
        let mut groups: Vec<serde_json::Value> = Vec::new();
        let mut by_label: Vec<(String, Vec<String>, Option<String>)> = Vec::new();
        for id in &confirmable {
            let Some(a) = edit.annotations.get(id) else {
                continue;
            };
            match by_label.iter_mut().find(|(label, ..)| *label == a.label) {
                Some((_, ids, _)) => ids.push(id.clone()),
                None => by_label.push((a.label.clone(), vec![id.clone()], a.description.clone())),
            }
        }
        for (label, ids, description) in &by_label {
            groups.push(serde_json::json!({
                "label": label,
                "description": description,
                "ids": ids,
            }));
        }
        let payload = serde_json::Value::Array(groups);
        self.pending_confirm_edits
            .insert(group, PendingConfirmEdit { edit, encoding });
        if let Err(e) = self.lua.run_lsp_confirm_edit(group, &payload) {
            self.editor
                .echo(format!("E5108: Error confirming a workspace edit: {e}"));
            // Nothing was asked, so nothing is accepted — the same shape a user's
            // "no" takes, settled through the one path that reports it. Its outcome is
            // *returned*, not folded into the held-back response: this runs inside the
            // apply, before `on_apply_edit` has recorded one, so there is nothing to
            // fold into yet — and dropping it would answer the server `applied: true`
            // for an edit that was declined (and stop waiting on any file operation
            // the surviving changes queued).
            return self.on_workspace_edit_decision(group, Vec::new());
        }
        AppliedEdit {
            group,
            pending: 0,
            awaiting_confirm: true,
            outcome: ApplyEditOutcome {
                applied: true,
                failure_reason: None,
                failed_change: None,
            },
        }
    }

    /// The user's answer to [`ask_before_applying`](Self::ask_before_applying):
    /// `accepted` is the annotation ids they said yes to. Changes tagged with a
    /// *declined* annotation are dropped; everything else — untagged changes, and
    /// changes whose annotation never needed confirming — applies as it would have.
    ///
    /// Declining everything is not a failure of ours, but it *is* something a server
    /// must know: it asked whether its edit was applied, and it wasn't. So the answer
    /// carries `applied: false` with the reason, rather than a success that never
    /// happened.
    ///
    /// The outcome is both folded into the held-back response *and* returned, because
    /// this is reached two ways: normally from Lua a tick or more later (when the
    /// record exists, and the return value is ignored), and synchronously from
    /// [`ask_before_applying`](Self::ask_before_applying) when Lua could not be reached
    /// at all — where the record does not exist yet and the caller carries the outcome
    /// out instead.
    pub(crate) fn on_workspace_edit_decision(
        &mut self,
        group: u64,
        accepted: Vec<String>,
    ) -> AppliedEdit {
        // No edit parked under this group: a second answer for one already settled (a
        // config calling `nx._lsp_edit_decision` by hand, a duplicate from a
        // re-entered chain). Nothing to apply and nothing went wrong — and no server is
        // waiting on this call either, since the one that was got its answer with the
        // first.
        let Some(pending) = self.pending_confirm_edits.remove(&group) else {
            return AppliedEdit {
                group,
                pending: 0,
                awaiting_confirm: false,
                outcome: ApplyEditOutcome {
                    applied: true,
                    failure_reason: None,
                    failed_change: None,
                },
            };
        };
        let confirmable = pending.edit.confirmable();
        let declined: Vec<&String> = confirmable
            .iter()
            .filter(|id| !accepted.iter().any(|a| a == *id))
            .collect();
        let total = pending.edit.changes.len();
        // Numbered **before** filtering: what survives keeps its index in the edit the
        // server sent, which is the only numbering a `failedChange` means anything in.
        let kept: Vec<(usize, WorkspaceChange)> = pending
            .edit
            .changes
            .into_iter()
            .enumerate()
            .filter(|(_, c)| {
                c.annotation()
                    .is_none_or(|id| !declined.iter().any(|d| d.as_str() == id))
            })
            .collect();
        let dropped = total - kept.len();
        let applied = if kept.is_empty() {
            self.editor.echo("apply_workspace_edit: declined");
            AppliedEdit {
                group,
                pending: 0,
                awaiting_confirm: false,
                outcome: ApplyEditOutcome {
                    applied: false,
                    failure_reason: Some("the user declined the change(s)".to_string()),
                    failed_change: None,
                },
            }
        } else {
            if dropped > 0 {
                self.editor
                    .echo(format!("Skipped {dropped} declined change(s)"));
            }
            self.apply_workspace_changes(group, kept, pending.encoding)
        };
        // Fold the outcome into the response this edit's server is still waiting on
        // (there is none for a user-driven apply, which reports by echoing — nor for
        // the synchronous `ask_before_applying` failure path, whose caller carries the
        // returned outcome out to `on_apply_edit` instead).
        if let Some(record) = self.pending_apply_edits.get_mut(&group) {
            record.awaiting_confirm = false;
            record.outstanding += applied.pending;
            if let Some(reason) = applied.outcome.failure_reason.clone() {
                record.trouble.push(reason);
            }
            if record.failed_change.is_none() {
                record.failed_change = applied.outcome.failed_change;
            }
        }
        self.settle_apply_edit(group);
        self.lsp_dirty = true;
        applied
    }

    /// Queue the disk half of a `create`: a recursive `mkdir` of the file's directory,
    /// which chains the **empty** file itself when it lands
    /// ([`WorkspaceFsOp::CreateDir`](super::WorkspaceFsOp::CreateDir) →
    /// [`CreatePlaceholder`](super::WorkspaceFsOp::CreatePlaceholder)).
    ///
    /// Empty, not the buffer's contents: a `create` resource operation says the file
    /// exists, and what the edits after it put in the buffer is yours to save — the same
    /// in-memory contract every other change in a workspace edit gets, and neovim's
    /// behavior. (nxvim used to write the contents out too; that deviation is gone.)
    ///
    /// The directory comes first because a refactor may extract into one that doesn't
    /// exist yet. Off-tick because that is the only way one code path serves the local,
    /// daemon and browser sessions; `recursive` ⇒ an existing directory is a success, so
    /// the common same-directory create needs no special case.
    fn queue_created_file_write(
        &mut self,
        group: u64,
        index: usize,
        buffer: BufferId,
        path: &Path,
    ) {
        let dir = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        self.queue_workspace_fs_job(
            group,
            index,
            FsJob::Mkdir {
                path: dir.to_string_lossy().into_owned(),
                recursive: true,
                mode: 0o755,
            },
            WorkspaceFsOp::CreateDir {
                buffer,
                dir,
                path: path.to_path_buf(),
            },
        );
    }

    /// Finish a `create` whose "does this file already exist?" question could only be
    /// answered **off-tick** — `ignoreIfExists` in a daemon / browser session, where the
    /// editor tick cannot see the filesystem and the replica fetch is the probe. Called
    /// from the fetch-landing site with what it found, after the edit's own stashed
    /// edits have applied.
    ///
    /// `existed` ⇒ the server asked us to leave that file alone, and we did: its real
    /// content is in the buffer, the edits are on top of it, and (like every other
    /// workspace edit) they stay in memory until a `:w`. Otherwise this was a create
    /// after all, so the file appears on disk exactly as the synchronous path's does —
    /// empty, the edits staying in the buffer.
    /// A no-op for every other buffer — the common case on every open.
    pub(crate) fn settle_workspace_create(&mut self, buffer: BufferId, existed: bool) {
        let Some((group, index)) = self.pending_create_writes.remove(&buffer) else {
            return;
        };
        if existed {
            return;
        }
        let Some(path) = self.editor.buffer_of(buffer).and_then(|b| b.path.clone()) else {
            return;
        };
        let path = self.abs_buffer_path(&path);
        self.editor
            .echo(format!("Created {}", self.buffer_path_for(&path).display()));
        self.queue_created_file_write(group, index, buffer, &path);
        self.pump_workspace_fs_queue();
    }

    /// Queue one of a workspace edit's **file** operations: `job` is the filesystem
    /// half (the same `FsJob` seam `nx.fs` rides, so a local session runs it on the
    /// event-loop actor, a daemon session over `luafs_op`, and the browser over the
    /// daemon or OPFS), `op` the buffer half to run when it lands.
    ///
    /// Queued, not dispatched: `documentChanges` is a *sequence*, so the operations run
    /// one at a time in this order ([`pump_workspace_fs_queue`](Self::pump_workspace_fs_queue)).
    /// `group` ties them to their apply (so a failure can drop the rest of them, and
    /// only them) and `index` is the change's position in the edit, which the response's
    /// `failedChange` reports.
    fn queue_workspace_fs_job(&mut self, group: u64, index: usize, job: FsJob, op: WorkspaceFsOp) {
        let id = self.next_workspace_fs_id;
        self.next_workspace_fs_id += 1;
        self.workspace_fs_jobs.insert(
            id,
            WorkspaceFsJob {
                op,
                group,
                index,
                // `local: false` — a workspace edit names the *session's* files (the
                // ones the language server sees), which in a daemon session live on
                // the daemon.
                job: Some(job),
            },
        );
        self.workspace_fs_queue.push_back(id);
    }

    /// Start the next queued file operation, unless one is already running. Strict
    /// one-at-a-time ordering: the seam dispatches each job onto its own task (and, in a
    /// daemon session, its own round trip), so two operations queued together would race
    /// — and `rename a→b` racing `rename b→c` renames a file that isn't there yet.
    fn pump_workspace_fs_queue(&mut self) {
        if self.workspace_fs_inflight.is_some() {
            return;
        }
        while let Some(id) = self.workspace_fs_queue.pop_front() {
            // Gone (its group aborted) or already dispatched: skip to the next.
            let Some(job) = self
                .workspace_fs_jobs
                .get_mut(&id)
                .and_then(|entry| entry.job.take())
            else {
                continue;
            };
            self.workspace_fs_inflight = Some(id);
            self.apply_loop_op(nxvim_lua::LoopOp::Fs {
                id,
                job,
                local: false,
            });
            // Start the watchdog on this operation (re-arming replaces the previous
            // one-shot, so the budget is per operation, not per edit). A `workspace/applyEdit`
            // is a request the server is blocked on, and its answer waits for this
            // operation — so an fs leg that stops answering must not be able to block
            // that server forever.
            self.apply_loop_op(nxvim_lua::LoopOp::TimerStart {
                id: crate::WORKSPACE_FS_TIMEOUT_TIMER_ID,
                delay_ms: crate::workspace_fs_timeout_ms(),
                repeat_ms: 0,
            });
            return;
        }
        // Nothing left to run: disarm, so a later idle session isn't woken by a
        // watchdog for work that finished.
        self.apply_loop_op(nxvim_lua::LoopOp::TimerStop {
            id: crate::WORKSPACE_FS_TIMEOUT_TIMER_ID,
        });
    }

    /// The watchdog fired: the operation in flight has not answered within
    /// [`workspace_fs_timeout_ms`](crate::workspace_fs_timeout_ms). Fail *that*
    /// operation — through the ordinary result path, so it takes the same `abort`
    /// route a real error does (the rest of its edit is dropped, the user is told, and
    /// a server-initiated `workspace/applyEdit` is answered `applied: false` instead
    /// of being left hanging).
    ///
    /// The claim is deliberately hedged: giving up is not proof the operation failed —
    /// the fs leg may still complete it — so the reason says so rather than asserting
    /// something we don't know. A late result for the abandoned job is swallowed by
    /// [`on_workspace_fs_result`](Self::on_workspace_fs_result) (it is still one of
    /// ours by id), never handed to a Lua callback that never existed.
    pub(crate) fn on_workspace_fs_timeout(&mut self) {
        let Some(id) = self.workspace_fs_inflight else {
            return;
        };
        let result = Err(nxvim_lua::FsError {
            code: "ETIMEDOUT".to_string(),
            message: format!(
                "no answer in {}ms (it may still complete)",
                crate::workspace_fs_timeout_ms()
            ),
        });
        self.on_workspace_fs_result(id, &result);
    }

    /// Drop every not-yet-started file operation of one apply — the `abort` strategy
    /// nxvim advertises: when a change fails, the ones after it do not run. Returns how
    /// many were dropped (0 when the failure was the last one). Never touches another
    /// apply's operations, and never the one in flight (it is already gone from the
    /// queue by then).
    ///
    /// Its off-tick `create` probes go with them. A probe is not in this queue — it is
    /// a replica fetch, and it lands in [`settle_workspace_create`](Self::settle_workspace_create)
    /// — so leaving it would have this abandoned apply write a file out later, through
    /// the one door `drop_workspace_group` doesn't close. (The *buffer* stays: it may be
    /// holding an existing file's contents. It just isn't a create any more.)
    fn drop_workspace_group(&mut self, group: u64) -> usize {
        self.pending_create_writes.retain(|_, (g, _)| *g != group);
        let dropped: Vec<u64> = self
            .workspace_fs_queue
            .iter()
            .copied()
            .filter(|id| {
                self.workspace_fs_jobs
                    .get(id)
                    .is_some_and(|entry| entry.group == group)
            })
            .collect();
        self.workspace_fs_queue.retain(|id| !dropped.contains(id));
        for id in &dropped {
            self.workspace_fs_jobs.remove(id);
        }
        dropped.len()
    }

    /// Land one workspace-edit file operation: do its buffer half (rebind a renamed
    /// buffer, wipe a deleted one), start the next operation, report a failure loud, and
    /// settle the server-initiated `workspace/applyEdit` waiting on it (if any). Returns
    /// whether `id` was one of ours — the two `FsResult` landing sites (native
    /// `on_loop_event`, wasm `fs_op_result`) call this first and fall through to the Lua
    /// promise when it says no.
    pub(crate) fn on_workspace_fs_result(
        &mut self,
        id: u64,
        result: &Result<nxvim_lua::FsValue, nxvim_lua::FsError>,
    ) -> bool {
        // Classified by **id alone**, which is what the base exists for: an id in this
        // range is the editor's, never a Lua promise's. That includes an id whose job
        // is already gone — the watchdog gave up on it and the operation answered
        // afterwards — which has to be swallowed here rather than fall through to a
        // Lua callback that was never registered for it.
        if id < crate::WORKSPACE_FS_JOB_BASE {
            return false;
        }
        if self.workspace_fs_inflight == Some(id) {
            self.workspace_fs_inflight = None;
        }
        let Some(job) = self.workspace_fs_jobs.remove(&id) else {
            return true;
        };
        let failure = match (&job.op, result) {
            // The `create`d file's directory exists (we made it, or it already did):
            // put the **empty** file there. Queued behind the mkdir rather than fired
            // with it, so a refactor extracting into a new directory lands its file
            // instead of failing after the fact. Front of the queue, ahead of any later
            // change: it is the same change.
            (WorkspaceFsOp::CreateDir { buffer, path, .. }, Ok(_)) => {
                let (buffer, path) = (*buffer, path.clone());
                self.queue_workspace_fs_job(
                    job.group,
                    job.index,
                    FsJob::Write {
                        path: path.to_string_lossy().into_owned(),
                        data: Vec::new(),
                    },
                    WorkspaceFsOp::CreatePlaceholder { buffer, path },
                );
                if let Some(next) = self.workspace_fs_queue.pop_back() {
                    self.workspace_fs_queue.push_front(next);
                }
                // Counted before the generic decrement below, so the applyEdit waits for
                // the file to exist rather than merely for its directory.
                if let Some(pending) = self.pending_apply_edits.get_mut(&job.group) {
                    pending.outstanding += 1;
                }
                None
            }
            (WorkspaceFsOp::CreateDir { dir, .. }, Err(e)) => Some(format!(
                "create directory {} failed: {} ({})",
                dir.display(),
                e.message,
                e.code
            )),
            // The file exists on disk, empty — neovim's model: a `create` *creates the
            // file*, and the content the edits put in the buffer is yours to save.
            //
            // Both change detectors have to be told this write was **ours**, or each
            // reports it straight back as an external change to a modified buffer — a
            // W12 conflict over nxvim's own placeholder:
            //   * locally, re-snapshot the buffer's disk baseline (it had none, never
            //     having been read) — which is also what lets its file watch arm at all,
            //     since `sync_buffer_watches` skips a buffer with no snapshot;
            //   * over a daemon, re-arm the watch with **no** `known` stat. The arm
            //     re-baselines to the live file and, per the leg's contract, "an
            //     absent/equal `known` pushes nothing" — so the daemon silently adopts
            //     the file we just made. (Its `fs_write` leg self-suppresses this way for
            //     `:w`; the `luafs_op` leg this write rides has no such hook.)
            (WorkspaceFsOp::CreatePlaceholder { buffer, path }, Ok(_)) => {
                self.editor.restamp_disk_baseline(*buffer);
                if self.fx.has_remote_fs() {
                    let path = path.to_string_lossy().into_owned();
                    self.fx.fs_watch(path, None);
                }
                None
            }
            (WorkspaceFsOp::CreatePlaceholder { path, .. }, Err(e)) => Some(format!(
                "create {} failed: {} ({})",
                path.display(),
                e.message,
                e.code
            )),
            (WorkspaceFsOp::MakeDir { dir }, Ok(_)) => {
                self.editor
                    .echo(format!("Created {}/", self.buffer_path_for(dir).display()));
                None
            }
            (WorkspaceFsOp::MakeDir { dir }, Err(e)) => Some(format!(
                "create directory {} failed: {} ({})",
                dir.display(),
                e.message,
                e.code
            )),
            // The `ignoreIfExists` probe: a destination that is already there means
            // "leave it alone" (not a failure — the server asked for exactly this);
            // otherwise the rename itself is queued to run NEXT, before any later
            // change, since it is the same change.
            (WorkspaceFsOp::RenameGuard { from, to, to_name }, Ok(value)) => {
                if matches!(value, nxvim_lua::FsValue::Bool(true)) {
                    // The name the user would have typed, like every other buffer-facing
                    // message here — `to` stays absolute for the filesystem side only.
                    self.editor.echo(format!(
                        "Skipped rename → {} (already exists)",
                        to_name.display()
                    ));
                } else {
                    self.queue_workspace_fs_job(
                        job.group,
                        job.index,
                        FsJob::Rename {
                            from: from.to_string_lossy().into_owned(),
                            to: to.to_string_lossy().into_owned(),
                        },
                        WorkspaceFsOp::Rename {
                            from: from.clone(),
                            to: to.clone(),
                            to_name: to_name.clone(),
                        },
                    );
                    if let Some(next) = self.workspace_fs_queue.pop_back() {
                        self.workspace_fs_queue.push_front(next);
                    }
                    // The chained op carries the same waiting response, and is counted
                    // before the generic decrement below — so the applyEdit waits for
                    // the rename, not merely for the probe.
                    if let Some(pending) = self.pending_apply_edits.get_mut(&job.group) {
                        pending.outstanding += 1;
                    }
                }
                None
            }
            (WorkspaceFsOp::RenameGuard { from, to, .. }, Err(e)) => Some(format!(
                "rename {} → {} failed: {} ({})",
                from.display(),
                to.display(),
                e.message,
                e.code
            )),
            (WorkspaceFsOp::Rename { from, to, to_name }, Ok(_)) => {
                // The bytes moved with the file, so only the name follows: the buffer
                // keeps its content, its modified state and its undo history. Its LSP
                // document, though, is a different document now — close the old URI so
                // the next sync opens the new one (a server left holding the old path
                // would answer about a file that no longer exists).
                //
                // Resolved *here* rather than when the operation was queued: this same
                // edit's text-edit half may have opened the file in between (an edit
                // addressed by the new name rewinds to the old one — see
                // `rewind_pending_renames`), and that buffer is the one that has to
                // follow the move.
                if let Some(buffer) = self.editor.find_buffer_by_path(from) {
                    self.reopen_lsp_document(buffer);
                    if let Some(buf) = self.editor.buffer_of_mut(buffer) {
                        buf.set_path(Some(to_name.clone()));
                    }
                    self.sync_lsp_buffer(buffer);
                }
                let _ = to;
                self.editor.echo(format!(
                    "Renamed {} → {}",
                    self.buffer_path_for(from).display(),
                    to_name.display()
                ));
                None
            }
            (WorkspaceFsOp::Delete { path, .. }, Ok(_)) => {
                // Force: the file is already gone, so refusing to close a modified
                // buffer would leave a window onto nothing (and `:w` would recreate
                // the file the server asked to remove). Resolved at landing time, for
                // the same reason the rename above is.
                if let Some(buffer) = self.editor.find_buffer_by_path(path) {
                    self.editor.delete_buffer(buffer, true);
                }
                self.editor
                    .echo(format!("Deleted {}", self.buffer_path_for(path).display()));
                None
            }
            (WorkspaceFsOp::Rename { from, to, .. }, Err(e)) => Some(format!(
                "rename {} → {} failed: {} ({})",
                from.display(),
                to.display(),
                e.message,
                e.code
            )),
            (
                WorkspaceFsOp::Delete {
                    path,
                    ignore_missing,
                    ..
                },
                Err(e),
            ) => {
                // `ignoreIfNotExists`: an already-absent file is the outcome the
                // server asked for, not a failure to report back.
                (!(*ignore_missing && e.code == "ENOENT")).then(|| {
                    format!(
                        "delete {} failed: {} ({})",
                        path.display(),
                        e.message,
                        e.code
                    )
                })
            }
        };
        match failure {
            Some(reason) => {
                // `abort`: this change failed, so the ones after it don't run. The ones
                // before it stay applied — that is the strategy, and what we told the
                // server to expect.
                let dropped = self.drop_workspace_group(job.group);
                let detail = if dropped > 0 {
                    format!("{reason} ({dropped} later change(s) not applied)")
                } else {
                    reason
                };
                self.editor.echo(format!("apply_workspace_edit: {detail}"));
                if let Some(pending) = self.pending_apply_edits.get_mut(&job.group) {
                    pending.outstanding = 0;
                    pending.trouble.push(detail);
                    pending.failed_change.get_or_insert(job.index as u32);
                }
            }
            None => {
                if let Some(pending) = self.pending_apply_edits.get_mut(&job.group) {
                    pending.outstanding = pending.outstanding.saturating_sub(1);
                }
            }
        }
        self.settle_apply_edit(job.group);
        self.pump_workspace_fs_queue();
        self.lsp_dirty = true;
        true
    }

    /// Answer a server-initiated `workspace/applyEdit` once nothing it asked for is
    /// still in flight — a no-op while a file operation is outstanding, so the
    /// `applied` flag describes what actually happened rather than what was attempted.
    fn settle_apply_edit(&mut self, group: u64) {
        let Some(pending) = self.pending_apply_edits.get(&group) else {
            return;
        };
        // Still parked on the user, or on a file operation: either way nothing can be
        // said about `applied` yet, and saying it early is the whole failure mode this
        // record exists to prevent.
        if pending.awaiting_confirm || pending.outstanding > 0 {
            return;
        }
        let Some(pending) = self.pending_apply_edits.remove(&group) else {
            return;
        };
        let reason = (!pending.trouble.is_empty()).then(|| {
            let detail = pending.trouble.join("; ");
            // The server's own label for the operation ("Extract to new file") makes
            // the rejection legible on both ends; the apply already echoed the detail.
            match &pending.label {
                Some(label) => format!("{label}: {detail}"),
                None => detail,
            }
        });
        self.fx.lsp_apply_edit_response(
            pending.key,
            pending.id,
            ApplyEditOutcome {
                applied: reason.is_none(),
                failure_reason: reason,
                failed_change: pending.failed_change,
            },
        );
    }

    /// Apply the workspace edits stashed for an **off-tick** replica buffer once its
    /// bytes have landed — the deferred tail of [`apply_workspace_edit`], called from
    /// the fetch-landing site (`load_replica_bytes`, shared native/wasm).
    /// Converts each stashed edit's LSP range to bytes against the now-filled
    /// buffer, in the originating server's encoding (a freshly-fetched replica has no
    /// server of its own yet), applies as one undo step, and re-syncs. A no-op when
    /// nothing is stashed for `buffer` — the common case on every other open.
    pub(crate) fn apply_pending_replica_edit(&mut self, buffer: BufferId) {
        let Some(pending) = self.pending_replica_edits.remove(&buffer) else {
            return;
        };
        let Some(buf) = self.editor.buffer_of(buffer) else {
            return;
        };
        // Document coordinates, exactly as the synchronous `create` path uses: the edit
        // owning the document's tail consumes the rope's phantom newline, so a file this
        // edit is *creating* (an empty document to the server, whether it didn't exist or
        // landed empty) is filled without a spurious blank last line.
        let (byte_edits, endofline) = lsp_edits_to_byte_edits(
            buf,
            pending
                .edits
                .iter()
                .map(|e| (&e.range, e.new_text.as_str())),
            pending.encoding,
        );
        self.editor.apply_edits_to(buffer, byte_edits);
        self.set_endofline(buffer, endofline);
        self.sync_lsp_buffer(buffer);
    }

    /// Offer a code-action round's titles in the **select menu** (neovim's
    /// `vim.ui.select` model) and stash the actions so confirming applies the chosen
    /// one (`pending_code_action`, keyed by the chosen index). An empty round shows a
    /// brief message instead of an empty menu.
    ///
    /// The list is merged across servers: `servers[i]` produced `actions[i]`, so a
    /// lazy action resolves — and a `command` executes — against the server that
    /// understands its `data`.
    ///
    /// `cb_id` (`0` = fire-and-forget) is the async `code_action` promise: it is
    /// *stashed* onto the chooser (settled later on the confirm/cancel path), or
    /// settled `nil` now on an empty round.
    ///
    /// `opts` is the caller's `nx.lsp.code_action{ context = { only = … }, apply = … }`:
    /// the round is filtered by `only` here as well as at the server (honoring
    /// `context.only` is a protocol *should*, so a non-compliant server must not turn a
    /// one-shot into a chooser), and `apply` distinguishes the **one-shot** case from
    /// the one with **options** — a single survivor is applied straight away, two or
    /// more still open the chooser because there is a genuine choice to make.
    pub(crate) fn show_code_actions_from(
        &mut self,
        actions: Vec<CodeActionData>,
        servers: Vec<ServerKey>,
        cb_id: u64,
        opts: CodeActionOpts,
    ) {
        // Filter the merged list, keeping each action paired with its origin.
        let (actions, servers): (Vec<CodeActionData>, Vec<ServerKey>) = actions
            .into_iter()
            .zip(servers.into_iter().map(Some).chain(std::iter::repeat(None)))
            .filter(|(a, _)| opts.matches(a.kind.as_deref()))
            .filter_map(|(a, s)| s.map(|s| (a, s)))
            .unzip();
        self.lsp_code_action_servers = servers;
        if actions.is_empty() {
            self.editor.echo(LspReqKind::CodeAction.empty_message());
            self.settle_lsp_promise(cb_id, serde_json::Value::Null);
            return;
        }
        // One-shot: exactly one action survived a filter the caller asked to auto-apply.
        // Apply it directly — no menu, so this works headlessly (a save action) and on
        // the wasm edit-host, which has no confirm→apply path at all.
        if opts.apply && actions.len() == 1 {
            self.lsp_code_actions = actions;
            // A chooser still awaiting a pick is superseded by this apply; settle its
            // promise `nil` so it can't hang, then take the stash for our own —
            // `apply_code_action` settles `cb_id` once the edit (or its lazy resolve)
            // lands, exactly as the confirm path does.
            let prev = std::mem::replace(&mut self.code_action_cb, cb_id);
            if prev != 0 {
                self.settle_lsp_promise(prev, serde_json::Value::Null);
            }
            #[cfg(feature = "native")]
            {
                self.pending_code_action = false;
            }
            self.apply_code_action(0);
            return;
        }
        let lines: Vec<String> = actions.iter().map(|a| a.title.clone()).collect();
        self.lsp_code_actions = actions;
        self.editor
            .open_menu(lines, nxvim_core::MenuPlacement::Cursor, 0);
        // The select-menu → `apply_code_action` routing is native-only (the field and its
        // consumer in `effects.rs` are `#[cfg(feature = "native")]`), so the flag it sets
        // is too — keeps the wasm edit-host build (`--no-default-features`) compiling.
        #[cfg(feature = "native")]
        {
            self.pending_code_action = true;
            // Take over the promise stash. A prior chooser still awaiting confirm is
            // superseded (a second `code_action` before picking) — settle its promise
            // `nil` so it can't hang.
            let prev = std::mem::replace(&mut self.code_action_cb, cb_id);
            if prev != 0 {
                self.settle_lsp_promise(prev, serde_json::Value::Null);
            }
        }
        // On the wasm edit-host there is no confirm→apply path, so the promise would
        // never settle — resolve it `nil` now rather than leave it hanging.
        #[cfg(not(feature = "native"))]
        self.settle_lsp_promise(cb_id, serde_json::Value::Null);
    }

    /// Apply the code action selected (by index) in the code-action panel: apply
    /// its eager `edit` now, else resolve a lazy action's edit
    /// (`codeAction/resolve`) and apply when the reply lands, else (a bare
    /// command) a brief message. Clears the stashed actions either way; the select
    /// menu has already closed itself on confirm.
    pub(crate) fn apply_code_action(&mut self, index: usize) {
        // The stashed async `code_action` promise (`0` = fire-and-forget). Taken here
        // so every terminal branch settles it exactly once — except the lazy-resolve
        // branch, which hands it to `resolve_code_action` to settle when its reply lands.
        let cb = std::mem::take(&mut self.code_action_cb);
        let action = self.lsp_code_actions.get(index).cloned();
        // The server that produced THIS action, captured before the stash is cleared —
        // a lazy action's `codeAction/resolve` must go back to it, not to whichever
        // server the buffer happens to list first.
        let origin = self.lsp_code_action_servers.get(index).cloned();
        self.lsp_code_actions.clear();
        self.lsp_code_action_servers.clear();
        let Some(action) = action else {
            self.settle_lsp_promise(cb, serde_json::Value::Null);
            return;
        };
        let has_edit = action.edit.is_some();
        if let Some(changes) = action.edit {
            // At the ORIGIN server's encoding: the merged chooser can list ruff's
            // quick-fix next to pyright's refactor, and each action's positions are
            // in its own server's encoding.
            let encoding = self.reply_encoding(origin.as_ref());
            self.apply_workspace_edit(changes, encoding);
            self.lsp_dirty = true;
        }
        // An action may carry a `command` alongside (or instead of) its edit:
        // neovim applies the edit first, then runs the command. Dispatch it through
        // Lua so a client-side `vim.lsp.commands` handler wins over the server's
        // `workspace/executeCommand` (Phase 8) — at the ORIGIN server, for the same
        // reason its `codeAction/resolve` goes there: the command's name and
        // arguments are that server's own vocabulary, so running ruff's
        // `source.fixAll` on pyright is a wrong request, not a degraded one.
        if let Some(command) = action.command {
            self.dispatch_lsp_command(command, origin.as_ref());
            // Edit applied + command dispatched — the action's effect is done.
            self.settle_lsp_promise(cb, serde_json::Value::Null);
        } else if !has_edit {
            if let Some(raw) = action.resolve {
                // A lazy action: ask the server to fill in its edit, then apply
                // when the reply lands (reply-as-event, like format/rename). The
                // promise rides the resolve request and settles on that reply.
                self.resolve_code_action(raw, cb, origin);
            } else {
                self.editor.echo("Code action has no edit");
                self.lsp_dirty = true;
                self.settle_lsp_promise(cb, serde_json::Value::Null);
            }
        } else {
            // An eager edit with no command / resolve — applied above; done.
            self.settle_lsp_promise(cb, serde_json::Value::Null);
        }
    }

    /// Apply a server→client `workspace/applyEdit` and answer it
    /// ([`LspEvent::ApplyEdit`](nxvim_lsp::LspEvent)). This is how a refactor
    /// delivered as a `command` reaches the buffers: `workspace/executeCommand`
    /// replies with nothing and the server pushes the edit back instead (gopls's
    /// `extract_to_new_file`, ts_ls's move-to-file, …).
    ///
    /// Applied at the **asking** server's negotiated encoding — it authored every
    /// position in the edit, including those for files another server (or no server)
    /// owns. The answer carries the real outcome, so a server that asked for
    /// something we couldn't do sees `applied: false` with the reason, never a
    /// pretended success.
    pub(crate) fn on_apply_edit(
        &mut self,
        key: ServerKey,
        id: u64,
        label: Option<String>,
        changes: WorkspaceEditData,
    ) {
        let encoding = self.reply_encoding(Some(&key));
        let applied = self.apply_workspace_edit(changes, encoding);
        // The file operations the edit queued carry its group id, so each landing finds
        // this record: the response goes out with the last of them. With none (the usual
        // case — text edits and creates are synchronous) it settles immediately.
        self.pending_apply_edits.insert(
            applied.group,
            PendingApplyEdit {
                key,
                id,
                label,
                outstanding: applied.pending,
                trouble: applied.outcome.failure_reason.into_iter().collect(),
                failed_change: applied.outcome.failed_change,
                awaiting_confirm: applied.awaiting_confirm,
            },
        );
        self.lsp_dirty = true;
        self.settle_apply_edit(applied.group);
    }

    /// Dispatch an LSP code-action `command` (Phase 8): route it through Lua's
    /// `vim.lsp._dispatch_command`, which runs a registered client-side
    /// `vim.lsp.commands[name]` handler, else issues a `workspace/executeCommand`
    /// to `origin`'s server (via the Phase-5 `client:request` path). The queued
    /// request drains immediately so it leaves on this tick.
    ///
    /// `origin` is the server that offered the action carrying this command — the
    /// only one that can execute it. `None` (or an origin that has since exited)
    /// falls back to the buffer's code-action server.
    pub(crate) fn dispatch_lsp_command(
        &mut self,
        command: nxvim_lsp::lsp_types::Command,
        origin: Option<&ServerKey>,
    ) {
        let client_id = origin
            .and_then(|key| self.lsp_client_id_of(key))
            .or_else(|| self.current_lsp_client_id());
        let Some(client_id) = client_id else {
            self.editor.echo("No language server attached");
            return;
        };
        let cmd_json = match serde_json::to_value(&command) {
            Ok(v) => v,
            Err(e) => {
                self.editor
                    .echo(format!("Code action command malformed: {e}"));
                return;
            }
        };
        if let Err(e) = self.lua.run_lsp_command(client_id, &cmd_json) {
            self.editor
                .echo(format!("E5108: Error dispatching command: {e}"));
        }
        self.apply_lua_effects();
    }

    /// Fire a `codeAction/resolve` for a lazy action, recording it as a pending
    /// apply request (content-version guarded, like format/rename); its resolved
    /// edit is applied in [`EditHost::on_lsp_reply`]. `cb_id` (`0` = fire-and-forget)
    /// is the async `code_action` promise, carried on the request so the resolve reply
    /// settles it once the edit applies (no server ⇒ settle `nil` now).
    /// `origin` is the server that produced the action (from the merged list); it is
    /// the only server whose `codeAction/resolve` can make sense of the action's
    /// `data`. `None` falls back to the buffer's code-action server.
    pub(crate) fn resolve_code_action(
        &mut self,
        action: Box<nxvim_lsp::lsp_types::CodeAction>,
        cb_id: u64,
        origin: Option<ServerKey>,
    ) {
        let key = origin.or_else(|| {
            self.lsp_target_for(self.editor.current_buffer_id(), LspReqKind::CodeAction)
                .map(|(key, _, _)| key)
        });
        let Some(key) = key else {
            self.editor.echo("No language server attached");
            self.settle_lsp_promise(cb_id, serde_json::Value::Null);
            return;
        };
        // Recorded against `key`: the resolved edit comes back in THAT server's
        // encoding, and it is the only server whose `data` blob this action carries.
        let token = self.register_lsp_request_to(LspReqKind::ResolveCodeAction, cb_id, &key);
        self.fx
            .lsp_request(key, token, LspRequest::ResolveCodeAction { action });
    }
}

/// The [`PositionEncoding`] a Lua caller named (`nx.lsp.apply_workspace_edit` /
/// `nx.lsp.show_document`'s `opts.encoding`). Anything but the two exact alternatives
/// is the protocol's default, `utf-16` — including the absent case, which the Lua
/// side already fills in.
fn position_encoding(name: &str) -> PositionEncoding {
    match name {
        "utf-8" => PositionEncoding::Utf8,
        "utf-32" => PositionEncoding::Utf32,
        _ => PositionEncoding::Utf16,
    }
}

/// Rewind a document's URI through the `rename`s an in-progress workspace edit has
/// queued but not yet run — `[(from, to), …]` in the order the server sent them.
///
/// A `documentChanges` sequence may address a document by the name a *previous*
/// change gives it (`rename a → b`, then edits to `b`). Those renames move real
/// bytes, so they can only run off the editor tick — after the text edits are staged
/// against their buffers. Following the chain backwards (transitively: `a → b → c`
/// takes `c` back to `a`) is what lets the edits reach the buffer that holds the file
/// *now*; the rename rebinds that same buffer when it lands.
///
/// A URI that isn't a file path, or that no queued rename produced, is returned
/// unchanged. The walk is bounded by the number of renames, so a cyclic chain
/// (`a → b` plus `b → a`) terminates instead of spinning.
fn rewind_pending_renames(renames: &[(PathBuf, PathBuf)], uri: Url) -> Url {
    if renames.is_empty() {
        return uri;
    }
    let Some(path) = uri_to_path(&uri) else {
        return uri;
    };
    let mut current = path.clone();
    for _ in 0..renames.len() {
        // The *last* rename that produced this name is the one that will produce it,
        // so its source is where the file is one step earlier.
        match renames.iter().rev().find(|(_, to)| *to == current) {
            Some((from, _)) => current = from.clone(),
            None => break,
        }
    }
    if current == path {
        return uri;
    }
    Url::from_file_path(&current).unwrap_or(uri)
}
