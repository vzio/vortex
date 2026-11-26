//! The hex view as a screen (this fork): the state of a [`ScreenKind::Hex`] screen, and how the
//! editor opens one. A window shows either a text document or a screen; the hex view is a screen,
//! so a file in it never becomes a [`Document`](crate::Document), and nothing about bytes lives
//! in the documents.
//!
//! A [`HexView`] owns the bytes ([`HexBuffer`]), the file they are from (none for a debugged
//! program's memory), what that file looked like on disk when it was read or last written (to
//! tell a change made by another program), and the first key of a `g` sequence.

use std::{
    io,
    path::{Path, PathBuf},
    time::SystemTime,
};

use crate::{
    document::DiskChange,
    editor::Action,
    screen::{Placement, Screen, ScreenId, ScreenKind},
    DocumentId, Editor,
};

use super::{HexBuffer, MemorySource};

/// What a file of the hex view looks like on disk: its metadata, never its content (the file can
/// be too big to read and hash), so any new modification time or size is a change.
type Stat = (SystemTime, u64);

fn stat(path: &Path) -> Option<Stat> {
    let metadata = path.metadata().ok()?;
    Some((metadata.modified().ok()?, metadata.len()))
}

/// The state of a hex screen.
pub struct HexView {
    /// The bytes.
    pub buffer: HexBuffer,
    /// The file the bytes are from; `None` for a debugged program's memory.
    pub path: Option<PathBuf>,
    /// The first key of a `g` sequence, waiting for the second.
    pub pending: Option<char>,
    /// The file as of the last time it was read or written.
    disk: Option<Stat>,
    /// The disk state found by the last [`Self::disk_change`] when it differs, and the one the
    /// user was told about.
    changed: Option<Stat>,
    warned: Option<Stat>,
}

impl std::fmt::Debug for HexView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HexView")
            .field("path", &self.path)
            .field("buffer", &self.buffer)
            .finish()
    }
}

impl HexView {
    /// The hex view of the file at `path`, which is not read (only what is shown is).
    pub fn file(path: &Path) -> io::Result<Self> {
        if path.metadata().is_ok_and(|metadata| !metadata.is_file()) {
            return Err(io::Error::other("path must be a regular file"));
        }
        let buffer = HexBuffer::open(path)?;
        Ok(Self {
            buffer,
            path: Some(path.to_path_buf()),
            pending: None,
            disk: stat(path),
            changed: None,
            warned: None,
        })
    }

    /// The hex view of a debugged program's memory.
    pub fn memory(memory: MemorySource) -> Self {
        Self {
            buffer: HexBuffer::debuggee(memory),
            path: None,
            pending: None,
            disk: None,
            changed: None,
            warned: None,
        }
    }

    /// The name of the screen: the file relative to the working directory, or what the memory is.
    pub fn name(&self) -> String {
        match (&self.path, self.buffer.memory()) {
            (Some(path), _) => vortex_stdx::path::get_relative_path(path)
                .to_string_lossy()
                .into_owned(),
            (None, Some(memory)) => format!("memory: {}", memory.label),
            (None, None) => "bytes".into(),
        }
    }

    /// Whether some bytes are overwritten and not written yet.
    pub fn is_modified(&self) -> bool {
        self.buffer.is_modified()
    }

    /// How the file compares to what it was when read or last written, by its metadata. Cheap:
    /// meant to be asked often.
    pub fn disk_change(&mut self) -> DiskChange {
        let (Some(path), Some(known)) = (self.path.as_deref(), self.disk) else {
            return DiskChange::Unchanged;
        };
        match stat(path) {
            Some(now) if now == known => DiskChange::Unchanged,
            Some(now) => {
                self.changed = Some(now);
                DiskChange::Changed
            }
            None if path.exists() => DiskChange::Unchanged,
            None => DiskChange::Missing,
        }
    }

    /// Whether the user was not yet told of the disk state found by the last
    /// [`Self::disk_change`]. Asking counts as telling.
    pub fn take_disk_change_warning(&mut self) -> bool {
        let due = self.changed != self.warned;
        self.warned = self.changed;
        due
    }

    /// Reads the file again: the bytes as they are now, the overwritten ones dropped, the
    /// cursors kept.
    pub fn reload(&mut self) -> io::Result<()> {
        let Some(path) = self.path.clone() else {
            return Ok(());
        };
        let mut buffer = HexBuffer::open(&path)?;
        buffer.keep_cursors(&self.buffer);
        self.buffer = buffer;
        self.pending = None;
        self.refresh_disk_state();
        Ok(())
    }

    /// Writes the overwritten bytes to the file, in place. Returns how many were written.
    pub fn write(&mut self) -> io::Result<usize> {
        let Some(path) = self.path.clone() else {
            return Err(io::Error::other("memory isn't a file"));
        };
        let written = self.buffer.write_in_place(&path)?;
        self.refresh_disk_state();
        Ok(written)
    }

    /// Takes the file as it is now for the state of the disk.
    fn refresh_disk_state(&mut self) {
        self.disk = self.path.as_deref().and_then(stat);
        self.changed = None;
        self.warned = None;
    }
}

impl Editor {
    /// The hex screens, with their state.
    pub fn hex_screens(&self) -> impl Iterator<Item = (ScreenId, &HexView)> {
        self.screens.iter().filter_map(|screen| {
            screen
                .state::<HexView>()
                .filter(|_| screen.kind == ScreenKind::Hex)
                .map(|view| (screen.id(), view))
        })
    }

    /// The hex screen of the file at `path` (canonical), if one is open.
    pub fn hex_screen_of(&self, path: &Path) -> Option<ScreenId> {
        self.hex_screens()
            .find(|(_, view)| view.path.as_deref() == Some(path))
            .map(|(id, _)| id)
    }

    /// The hex screens with bytes overwritten and not written, with their names.
    pub fn unwritten_hex_screens(&self) -> Vec<(ScreenId, String)> {
        self.hex_screens()
            .filter(|(_, view)| view.is_modified())
            .map(|(id, _)| {
                (
                    id,
                    self.screens
                        .get(id)
                        .map(|s| s.name.clone())
                        .unwrap_or_default(),
                )
            })
            .collect()
    }

    /// The screen of the hex view of the file at `path`, not shown yet.
    fn hex_screen(&self, path: &Path) -> io::Result<Screen> {
        let view = HexView::file(path)?;
        let source = self.document_id_by_path(path);
        Ok(Screen::new(ScreenKind::Hex, view.name())
            .with_source(source)
            .with_state(view))
    }

    /// Shows the file at `path` in the hex view, in a window at `placement` (`Cover`: over the
    /// window it was asked from, which comes back with `Esc`). The file is not read: only what
    /// is on screen is. A file already shown is only focused.
    pub fn open_hex(&mut self, path: &Path, placement: Placement) -> anyhow::Result<ScreenId> {
        let path = vortex_stdx::path::canonicalize(path);
        if let Some(id) = self.hex_screen_of(&path) {
            self.open_screen(id, placement);
            return Ok(id);
        }
        let screen = self.hex_screen(&path)?;
        let id = self.add_screen(screen);
        self.open_screen(id, placement);
        Ok(id)
    }

    /// Shows a debugged program's memory in the hex view, in a window at `placement`.
    pub fn open_memory(&mut self, memory: MemorySource, placement: Placement) -> ScreenId {
        let view = HexView::memory(memory);
        let id = self.add_screen(Screen::new(ScreenKind::Hex, view.name()).with_state(view));
        self.open_screen(id, placement);
        id
    }

    /// Opens the binary file at `path` (canonical, not open as a document) in the hex view, in
    /// the window `action` is for: a screen over the window a text file would open in (a split
    /// of the current buffer, for the split actions). Returns the document the covered window
    /// still shows under the screen.
    pub(crate) fn open_binary(
        &mut self,
        path: &Path,
        mut action: Action,
    ) -> Result<DocumentId, crate::document::DocumentOpenError> {
        use crate::document::DocumentOpenError;
        if matches!(action, Action::Load) {
            return Err(io::Error::other(
                "a binary file is shown in the hex view only when opened in a window",
            )
            .into());
        }
        if path.metadata().is_ok_and(|metadata| !metadata.is_file()) {
            return Err(DocumentOpenError::IrregularFile);
        }
        // an open view of the file is only focused
        if let Some(id) = self.hex_screen_of(path) {
            if let Some(view) = self.view_of(id) {
                let top = self.tree.top_of(view);
                self.focus(top);
                return Ok(self.document_under(view).unwrap_or_else(|| self.scratch()));
            }
        }
        // (the file is checked before any window is made)
        let screen = self.hex_screen(path)?;
        if matches!(action, Action::Replace) {
            self.leave_screen_for_text(&mut action);
        }
        let current = self
            .tree
            .try_get(self.tree.text_focus())
            .map(|view| view.doc);
        match (action, current) {
            (Action::Replace, Some(_)) => {}
            (Action::Replace, None) => {
                self.new_file(Action::VerticalSplit);
            }
            (_, Some(doc)) => self.switch(doc, action),
            (_, None) => {
                self.new_file(action);
            }
        }
        let covered = self.tree.focus;
        let doc = self.tree.try_get(covered).map(|view| view.doc);
        let id = self.add_screen(screen);
        self.open_screen(id, Placement::Cover);
        Ok(doc.unwrap_or_else(|| self.scratch()))
    }

    /// The document the window `view` shows under the screens that cover it.
    fn document_under(&self, view: crate::ViewId) -> Option<DocumentId> {
        self.tree
            .chain(view)
            .into_iter()
            .find_map(|id| self.tree.try_get(id).map(|view| view.doc))
    }

    /// Any document, a new scratch buffer if there is none.
    fn scratch(&mut self) -> DocumentId {
        match self.documents.keys().next().copied() {
            Some(doc) => doc,
            None => self.new_document(crate::Document::default(
                self.config.clone(),
                self.syn_loader.clone(),
            )),
        }
    }
}
