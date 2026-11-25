//! Starting a terminal in a window, the scratch terminal that `C-t` shows and hides, more
//! terminals, `:sh`, the `vx` in a terminal, and a shell's exit.

use tokio::sync::mpsc::unbounded_channel;
use vortex_core::{pos_at_coords, Position, Selection};
use vortex_view::{
    align_view,
    document::Mode,
    editor::Action,
    screen::{Placement, ScreenKind},
    terminal::{Shell, ShellEvent, ShellOptions, Size, Terminal, Waiter},
    tree::Layout,
    Align, Document, DocumentId, Editor,
};

use super::shim;
use crate::job;

/// Starts a shell in a new terminal, without showing it: its screen document.
fn spawn(editor: &mut Editor) -> anyhow::Result<DocumentId> {
    let config = editor.config().terminal.clone();
    let doc = Document::screen_document(
        ScreenKind::Terminal,
        "terminal".into(),
        editor.config.clone(),
        editor.syn_loader.clone(),
    );
    let doc = editor.add_screen_document(doc);

    // `vx` in the shell talks to this editor; started first, so its directory goes on PATH
    let server = match shim::Server::start(move |request, reply| {
        tokio::spawn(job::dispatch(move |editor, _| {
            handle_shim_request(editor, doc, request, reply)
        }));
    }) {
        Ok(server) => Some(server),
        Err(err) => {
            log::warn!("terminal: no `vx` shim: {err}");
            None
        }
    };

    // the window's size comes with it; until then, the editor's
    let area = editor.tree.area();
    let options = ShellOptions {
        command: config.shell,
        cwd: Some(vortex_stdx::env::current_working_dir()),
        env: server
            .as_ref()
            .map(|server| server.env())
            .unwrap_or_default(),
        path_prefix: server.as_ref().map(|server| server.bin_dir().to_owned()),
        integration: config.shell_integration,
        size: Size {
            rows: area.height.max(2),
            cols: area.width.max(2),
        },
        scrollback: config.scrollback,
    };
    let (sender, mut receiver) = unbounded_channel();
    let shell = match Shell::spawn(options, sender) {
        Ok(shell) => shell,
        Err(err) => {
            let _ = editor.close_document(doc, true);
            return Err(err);
        }
    };

    tokio::spawn(async move {
        // the server lives as long as the shell: `vx` can't reach a dead one
        let _server = server;
        while let Some(event) = receiver.recv().await {
            match event {
                ShellEvent::Output | ShellEvent::Status(_) | ShellEvent::Title(_) => {
                    vortex_event::request_redraw()
                }
                ShellEvent::Cwd(_) => {
                    job::dispatch(move |editor, _| rename(editor, doc)).await;
                }
                ShellEvent::CommandLine(line) => {
                    job::dispatch(move |editor, _| super::edit::command_line(editor, doc, line))
                        .await;
                }
                ShellEvent::Exited(_) => {
                    job::dispatch(move |editor, _| exited(editor, doc)).await;
                    break;
                }
            }
        }
    });

    let terminal = Terminal::new(shell);
    let name = terminal.name();
    editor.terminals.list.insert(doc, terminal);
    if let Some(document) = editor.documents.get_mut(&doc) {
        document.set_screen_name(name);
    }
    Ok(doc)
}

/// The shell reported its working directory: the terminal's name follows it.
fn rename(editor: &mut Editor, doc: DocumentId) {
    let Some(name) = editor.terminals.get(doc).map(Terminal::name) else {
        return;
    };
    if let Some(document) = editor.documents.get_mut(&doc) {
        document.set_screen_name(name);
    }
}

/// The shell exited (`exit`, `Ctrl-d`): its window closes. The scratch terminal's next `C-t`
/// starts a fresh shell.
fn exited(editor: &mut Editor, doc: DocumentId) {
    if !editor.terminals.is_terminal(doc) {
        return;
    }
    let was_focused = editor.tree.try_get(editor.tree.focus).map(|view| view.doc) == Some(doc);
    let _ = editor.close_document(doc, true);
    if was_focused {
        // the shell may have changed files
        crate::handlers::disk::check_all(editor);
    }
}

/// Starts the scratch terminal when it doesn't run. An error (the shell can't start) is shown in
/// the statusline.
fn ensure_scratch(editor: &mut Editor) -> Option<DocumentId> {
    if let Some(doc) = editor.terminals.scratch {
        return Some(doc);
    }
    match spawn(editor) {
        Ok(doc) => {
            editor.terminals.scratch = Some(doc);
            Some(doc)
        }
        Err(err) => {
            editor.set_error(format!("Couldn't start the shell: {err}"));
            None
        }
    }
}

/// `C-t`: shows the scratch terminal (starting its shell when none runs) and focuses it, or hides
/// it when it is shown, wherever the focus is. Hidden, its shell keeps running.
pub fn toggle_scratch(editor: &mut Editor) {
    let shown = editor.terminals.scratch.and_then(|doc| editor.view_of(doc));
    match shown {
        Some(view) => {
            // the last window: something to show instead
            if editor.tree.views().count() == 1 {
                editor.new_file(Action::HorizontalSplit);
            }
            editor.close(view);
            // the shell may have changed files
            crate::handlers::disk::check_all(editor);
        }
        None => {
            if let Some(doc) = ensure_scratch(editor) {
                editor.show_screen(doc);
            }
        }
    }
}

/// `:sh command`: shows and focuses the scratch terminal, and types the command in its shell and
/// presses Enter (once the shell is ready). `:sh` alone only shows it.
pub fn run(editor: &mut Editor, command: &str) {
    let Some(doc) = ensure_scratch(editor) else {
        return;
    };
    editor.show_screen(doc);
    if let Some(terminal) = editor.terminals.get(doc) {
        if !command.is_empty() {
            terminal.shell.write(format!("{command}\r").as_bytes());
        }
    }
}

/// A new terminal in a split of the focused window (`layout`): an ordinary window, which `C-t`
/// doesn't touch and closing kills.
pub fn open_new(editor: &mut Editor, layout: Layout) {
    match spawn(editor) {
        Ok(doc) => {
            editor.open_screen_window(doc, Placement::Split(layout));
        }
        Err(err) => editor.set_error(format!("Couldn't start the shell: {err}")),
    }
}

/// `vx` in a terminal: opens files in the window for documents, or just gives it the focus.
fn handle_shim_request(
    editor: &mut Editor,
    terminal: DocumentId,
    request: shim::Request,
    reply: shim::Reply,
) {
    let (files, wait) = match request {
        shim::Request::Focus => {
            focus_documents(editor);
            crate::handlers::disk::check_all(editor);
            return reply.send(shim::Response::Done);
        }
        shim::Request::Open { files, wait } => (files, wait),
    };
    let mut opened = Vec::new();
    for file in files {
        // from the terminal, a file opens in the window for documents
        match editor.open(&file.path, Action::Replace) {
            Ok(doc) => {
                let (view, document) = vortex_view::current!(editor);
                if let Some(line) = file.line {
                    let position = Position::new(
                        line.saturating_sub(1),
                        file.column.unwrap_or(1).saturating_sub(1),
                    );
                    let pos = pos_at_coords(document.text().slice(..), position, true);
                    document.set_selection(view.id, Selection::point(pos));
                    align_view(document, view, Align::Center);
                }
                opened.push(doc);
            }
            Err(err) => {
                let message = format!("{}: {err}", file.path.display());
                editor.set_error(message.clone());
                return reply.send(shim::Response::Error(message));
            }
        }
    }
    crate::handlers::disk::check_all(editor);
    if !wait || opened.is_empty() {
        return reply.send(shim::Response::Done);
    }
    // `$EDITOR`: done when all of them are closed
    let reply = std::sync::Arc::new(parking_lot::Mutex::new((opened.len(), Some(reply))));
    for doc in opened {
        let reply = reply.clone();
        editor.terminals.waiters.push(Waiter {
            doc,
            terminal,
            done: Box::new(move || {
                let mut reply = reply.lock();
                reply.0 -= 1;
                if reply.0 == 0 {
                    if let Some(reply) = reply.1.take() {
                        reply.send(shim::Response::Done);
                    }
                }
            }),
        });
    }
}

/// `vx` alone: the focus goes to the window for documents (the terminal stays shown).
fn focus_documents(editor: &mut Editor) {
    if let Some(view) = editor.document_view() {
        editor.focus(view);
    } else {
        editor.new_file(Action::VerticalSplit);
    }
    if editor.mode() != Mode::Normal {
        editor.mode = Mode::Normal;
    }
}
