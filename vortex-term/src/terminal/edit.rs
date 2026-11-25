//! Editing a terminal's command line in a Vortex buffer (the reverse of zsh's
//! `edit-command-line`): from normal mode, `e` asks the shell for the line it is editing
//! (Vortex's integration answers with OSC 6973), which opens in a buffer split below the
//! terminal, in the shell's language; `:w` (`:wq`, `:x`) sends the buffer back to the shell's
//! line, replacing what was there, without running it; `:q` (`:q!` once changed) leaves the line
//! as it was. Only at a prompt, which the OSC 133 markers tell.

use vortex_core::{Rope, Selection};
use vortex_view::{document::Mode, editor::Action, Document, DocumentId, Editor};

/// The share of the terminal's window the buffer takes.
const SHARE: u16 = 30;

/// `e` in a terminal's normal mode: asks the shell for its command line. Says why when it can't.
pub fn request(editor: &mut Editor) {
    let Some((doc, _)) = super::modes::focused(editor) else {
        return;
    };
    let Some(terminal) = editor.terminals.get_mut(doc) else {
        return;
    };
    let shell = &terminal.shell;
    let refusal = if !shell.has_markers() {
        Some("no prompt: the shell sends no OSC 133 markers")
    } else if !shell.at_prompt() {
        Some("a program runs: there is no command line to edit")
    } else if !shell.has_vortex_integration() {
        Some("editing the command line needs Vortex's shell integration (bash, zsh)")
    } else {
        None
    };
    if let Some(refusal) = refusal {
        editor.set_error(refusal);
        return;
    }
    terminal.awaiting_command_line = true;
    terminal.shell.request_command_line();
}

/// The shell sent its command line: it opens in a buffer below the terminal, if it was asked for.
pub fn command_line(editor: &mut Editor, terminal: DocumentId, line: String) {
    let Some(term) = editor.terminals.get_mut(terminal) else {
        return;
    };
    if !std::mem::take(&mut term.awaiting_command_line) {
        return;
    }
    let language = match term.shell.name().as_str() {
        "fish" => "fish",
        "nu" => "nu",
        _ => "bash",
    };
    let mut doc = Document::from(
        Rope::from(line.as_str()),
        None,
        editor.config.clone(),
        editor.syn_loader.clone(),
    );
    let loader = editor.syn_loader.load();
    let _ = doc.set_language_by_language_id(language, &loader);
    drop(loader);
    // from the terminal's window: the buffer opens in a split of it
    if editor
        .view_of(terminal)
        .is_some_and(|view| view != editor.tree.focus)
    {
        editor.show_screen(terminal);
    }
    let doc_id = editor.new_file_from_document(Action::HorizontalSplit, doc);
    editor.terminals.edits.insert(doc_id, terminal);
    let (view, document) = vortex_view::current!(editor);
    let end = document.text().len_chars();
    document.set_selection(view.id, Selection::point(end));
    let view_id = view.id;
    editor.tree.set_share(view_id, Some(SHARE));
    editor.enter_normal_mode();
    if editor.mode() != Mode::Normal {
        editor.mode = Mode::Normal;
    }
}

/// `:w` in a command line's buffer: sends it to its terminal's shell, replacing the line there.
pub fn send(editor: &mut Editor) -> anyhow::Result<()> {
    let (view, doc) = vortex_view::current!(editor);
    let Some(&terminal) = editor.terminals.edits.get(&doc.id()) else {
        anyhow::bail!("not a command line");
    };
    let text = doc.text().to_string();
    let line = text.trim_end_matches(['\n', '\r']);
    doc.append_changes_to_history(view);
    doc.reset_modified();
    let Some(term) = editor.terminals.get(terminal) else {
        anyhow::bail!("its terminal is gone");
    };
    if !term.shell.at_prompt() {
        anyhow::bail!("a program runs in the terminal: the line wasn't sent");
    }
    term.shell.replace_command_line(line);
    Ok(())
}
