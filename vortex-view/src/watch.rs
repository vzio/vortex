//! Watch mode (fork feature): the changes made to the working tree since a watch started, for
//! following them as they happen (an agent at work), marking them and going through them.
//!
//! This is the state: the session's baseline (what "changed this session" is measured against),
//! its files and hunks in the order they changed, and whether the editor follows them. Watching
//! the disk, following and the keys are in `vortex-term`'s `watch` module.
//!
//! - **The baseline** of a file is its content when the watch started: the committed version for
//!   files that were clean in git (read the first time the file changes), a snapshot for files
//!   that were already dirty (staged, unstaged or untracked). Earlier uncommitted work never
//!   counts as a change of the session.
//! - **Hunks** are numbered as they appear (`seq`): a hunk that is still there, with the same
//!   lines, keeps its number when its file changes again elsewhere; a new or grown one gets a new
//!   number. So the hunks of every file can be gone through in the order they changed.
//! - Everything is in memory: quitting drops the session.

use std::any::Any;
use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use parking_lot::Mutex;
use vortex_vcs::DiffProviderRegistry;

use crate::DocumentId;

/// The contents of the files when the watch started: `None` for a file that didn't exist.
pub struct Baseline {
    /// Canonical, like the paths of the watcher's events.
    work_dir: PathBuf,
    git_dir: PathBuf,
    /// HEAD when the watch started, `None` before the first commit.
    head: Option<String>,
    trust_full: bool,
    providers: DiffProviderRegistry,
    /// The snapshots of the files that were dirty at the start, and the committed versions read
    /// since.
    files: Mutex<HashMap<PathBuf, Option<Arc<[u8]>>>>,
}

impl Baseline {
    /// `dirty` are the snapshots of the files that were dirty at the start, by absolute path.
    pub fn new(
        work_dir: PathBuf,
        git_dir: PathBuf,
        head: Option<String>,
        trust_full: bool,
        providers: DiffProviderRegistry,
        dirty: HashMap<PathBuf, Option<Arc<[u8]>>>,
    ) -> Baseline {
        Baseline {
            work_dir,
            git_dir,
            head,
            trust_full,
            providers,
            files: Mutex::new(dirty),
        }
    }

    pub fn work_dir(&self) -> &Path {
        &self.work_dir
    }

    /// Whether the watcher's `path` is a file of the working tree (not in the git dir).
    pub fn covers(&self, path: &Path) -> bool {
        path.starts_with(&self.work_dir) && !path.starts_with(&self.git_dir)
    }

    /// The content of `path` when the watch started: its snapshot, or its version in the commit
    /// HEAD was then, read once. `None` when it didn't exist (or isn't a file).
    pub fn get(&self, path: &Path) -> Option<Arc<[u8]>> {
        if let Some(content) = self.files.lock().get(path) {
            return content.clone();
        }
        let content = self.head.as_ref().and_then(|head| {
            self.providers
                .file_at_commit(&self.work_dir, self.trust_full, head, path)
                .ok()
                .flatten()
                .map(Arc::from)
        });
        self.files
            .lock()
            .entry(path.to_owned())
            .or_insert(content)
            .clone()
    }
}

/// What a file is, compared with its baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Modified,
    /// Didn't exist when the watch started: the whole file is a change.
    Created,
    Deleted,
    /// Back to its baseline: no longer a change.
    Unchanged,
}

/// Files that are listed but not followed, counted nor gone through with `n`/`N`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quiet {
    No,
    /// Named in `[editor.changes] lockfiles`.
    Lockfile,
    Binary,
}

/// A hunk of a file as it is now, against its baseline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HunkLines {
    /// Its lines in the baseline, from 0.
    pub before: Range<u32>,
    /// Its lines now, from 0. Empty where lines were removed.
    pub after: Range<u32>,
    /// A hash of its removed and added lines: the same hunk, moved by changes above it, has the
    /// same key.
    pub key: u64,
}

/// A file as the watcher found it after a change.
#[derive(Debug, Clone)]
pub struct FileUpdate {
    pub path: PathBuf,
    pub kind: FileKind,
    pub quiet: Quiet,
    /// Its hunks, in the order of the file (none for quiet and deleted files).
    pub hunks: Vec<HunkLines>,
    /// A hash of its content, to tell whether a quiet or deleted file changed again.
    pub content: u64,
    /// Its modification time: the files of a burst are numbered oldest first.
    pub modified: Option<SystemTime>,
}

/// A hunk of the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionHunk {
    pub before: Range<u32>,
    pub after: Range<u32>,
    key: u64,
    /// When it appeared: higher is newer.
    pub seq: u64,
}

/// A file changed during the session.
#[derive(Debug, Clone)]
pub struct SessionFile {
    pub kind: FileKind,
    pub quiet: Quiet,
    /// In the order of the file.
    pub hunks: Vec<SessionHunk>,
    /// When it last changed.
    pub seq: u64,
    content: u64,
}

impl SessionFile {
    /// Whether it is followed and gone through: a text file that exists.
    pub fn navigable(&self) -> bool {
        self.quiet == Quiet::No && matches!(self.kind, FileKind::Modified | FileKind::Created)
    }
}

/// A hunk, by its file and number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub path: PathBuf,
    pub seq: u64,
    /// Its lines now, from 0.
    pub lines: Range<u32>,
}

/// The highlight of the hunk following just jumped to, fading.
#[derive(Debug, Clone)]
pub struct Flash {
    pub doc: DocumentId,
    /// Lines from 0, at least one.
    pub lines: Range<usize>,
    pub start: Instant,
}

/// Where `n`/`N` go from where you are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// To this hunk.
    To(Target),
    /// Nowhere: `n` from the newest hunk, which stays where it is (no wrapping around).
    AtNewest,
    /// Nowhere: `N` from the oldest hunk.
    AtOldest,
}

/// A watch session: started by `:watch`, it watches the working tree until it is stopped
/// (`:watch-stop`), whether watch mode is on or was left with `Esc` (then it records the changes
/// without following them, and entering again resumes it). Its changes stay after it is stopped,
/// until the next watch or `:watch-clear`.
pub struct Session {
    /// Distinguishes the batches of the watcher of this session from a previous one's.
    pub id: u64,
    /// Watch mode is on: its keys and its bar, and changes are followed while `following`.
    pub active: bool,
    /// Changes are followed: opened, jumped to and flashed. Otherwise you are looking around.
    /// Kept while watch mode is left, for when it is entered again.
    pub following: bool,
    pub baseline: Arc<Baseline>,
    files: HashMap<PathBuf, SessionFile>,
    last_seq: u64,
    /// The newest hunk seen: followed to, or gone to with `n`/`N`. Newer ones are "new".
    seen: u64,
    /// The hunk followed or gone to last, where `n`/`N` go on from.
    pub at: Option<(PathBuf, u64)>,
    /// A search was made since watch mode ended: `n`/`N` repeat it rather than go through the
    /// session's hunks.
    pub search_owns_n: bool,
    pub flash: Option<Flash>,
    /// The latest activity, for the mode bar: what was just followed, a skipped buffer, a
    /// lockfile, a key that waits for watch mode to end.
    pub activity: Option<String>,
    /// The watcher; dropping it stops watching.
    pub watcher: Option<Box<dyn Any>>,
}

/// What a batch of updates brought.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Applied {
    /// The first new hunk of each file that has new hunks, oldest file first: following goes to
    /// the last one whose buffer has no unsaved edits.
    pub to_follow: Vec<Target>,
    /// The files whose state changed (to set their buffers' baselines).
    pub changed: Vec<PathBuf>,
}

impl Session {
    pub fn new(id: u64, baseline: Arc<Baseline>) -> Session {
        Session {
            id,
            active: true,
            following: true,
            baseline,
            files: HashMap::new(),
            last_seq: 0,
            seen: 0,
            at: None,
            search_owns_n: false,
            flash: None,
            activity: None,
            watcher: None,
        }
    }

    /// The number of files changed during the session.
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// Where the hunk followed or gone to last is among the hunks `n`/`N` go through: its
    /// number (from 1) and their count. The number is `None` before any.
    pub fn position(&self) -> (Option<usize>, usize) {
        let hunks = self.hunks_in_order();
        let at = self.at.as_ref().and_then(|(path, seq)| {
            hunks
                .iter()
                .position(|hunk| hunk.seq == *seq && &hunk.path == path)
                .map(|i| i + 1)
        });
        (at, hunks.len())
    }

    /// The files changed during the session.
    pub fn file(&self, path: &Path) -> Option<&SessionFile> {
        self.files.get(path)
    }

    /// The files changed during the session, the most recently changed first.
    pub fn files_newest_first(&self) -> Vec<(&PathBuf, &SessionFile)> {
        let mut files: Vec<_> = self.files.iter().collect();
        files.sort_by(|(a_path, a), (b_path, b)| b.seq.cmp(&a.seq).then(a_path.cmp(b_path)));
        files
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Takes in what the watcher found: numbers the new hunks, oldest file first.
    pub fn apply(&mut self, mut updates: Vec<FileUpdate>) -> Applied {
        updates.sort_by(|a, b| a.modified.cmp(&b.modified).then(a.path.cmp(&b.path)));
        let mut applied = Applied::default();
        for update in updates {
            let path = update.path.clone();
            let old = self.files.remove(&path);
            if update.kind == FileKind::Unchanged {
                if old.is_some() {
                    applied.changed.push(path);
                }
                continue;
            }
            let mut unmatched: Vec<&SessionHunk> = old
                .as_ref()
                .map(|old| old.hunks.iter().collect())
                .unwrap_or_default();
            let mut hunks = Vec::with_capacity(update.hunks.len());
            let mut first_new = None;
            for hunk in update.hunks {
                let seq = match unmatched.iter().position(|old| old.key == hunk.key) {
                    Some(i) => unmatched.swap_remove(i).seq,
                    None => {
                        self.last_seq += 1;
                        first_new.get_or_insert((self.last_seq, hunk.after.clone()));
                        self.last_seq
                    }
                };
                hunks.push(SessionHunk {
                    before: hunk.before,
                    after: hunk.after,
                    key: hunk.key,
                    seq,
                });
            }
            let same = old
                .as_ref()
                .is_some_and(|old| old.content == update.content && old.kind == update.kind);
            // the file changed now unless only its old hunks are left, unchanged
            let seq = match &old {
                _ if first_new.is_some() => self.last_seq,
                Some(old) if same => old.seq,
                _ => {
                    self.last_seq += 1;
                    self.last_seq
                }
            };
            let file = SessionFile {
                kind: update.kind,
                quiet: update.quiet,
                hunks,
                seq,
                content: update.content,
            };
            if !same {
                applied.changed.push(path.clone());
            }
            if file.navigable() {
                if let Some((seq, lines)) = first_new {
                    applied.to_follow.push(Target {
                        path: path.clone(),
                        seq,
                        lines,
                    });
                }
            }
            self.files.insert(path, file);
        }
        applied
    }

    /// The hunks `n`/`N` go through, oldest first.
    pub fn hunks_in_order(&self) -> Vec<Target> {
        let mut hunks: Vec<Target> = self
            .files
            .iter()
            .filter(|(_, file)| file.navigable())
            .flat_map(|(path, file)| {
                file.hunks.iter().map(move |hunk| Target {
                    path: path.clone(),
                    seq: hunk.seq,
                    lines: hunk.after.clone(),
                })
            })
            .collect();
        hunks.sort_by_key(|hunk| hunk.seq);
        hunks
    }

    /// The newest hunk `n` can go to.
    fn newest_seq(&self) -> Option<u64> {
        self.hunks_in_order().last().map(|hunk| hunk.seq)
    }

    /// How many hunks appeared since the newest one seen: what following would have shown.
    pub fn new_count(&self) -> usize {
        self.hunks_in_order()
            .iter()
            .filter(|hunk| hunk.seq > self.seen)
            .count()
    }

    /// Everything so far was seen: following showed it.
    pub fn see_all(&mut self) {
        self.seen = self.last_seq;
    }

    /// The hunk at `line` (from 0) of `path`, if it is one of the session's.
    fn hunk_at(&self, path: &Path, line: u32) -> Option<u64> {
        let file = self.files.get(path).filter(|file| file.navigable())?;
        file.hunks
            .iter()
            .find(|hunk| {
                hunk.after.contains(&line) || (hunk.after.is_empty() && hunk.after.start == line)
            })
            .map(|hunk| hunk.seq)
    }

    /// Where `n` (`forward`) or `N` goes from the cursor at `line` of `path`: the next (previous)
    /// hunk in the order they changed, stopping at the ends. From inside a hunk of the session it
    /// goes on from that hunk, else from the one gone to last; without either, `n` goes to the
    /// oldest and `N` to the newest. `None` when the session has no hunks.
    pub fn step(&self, cursor: Option<(&Path, u32)>, forward: bool) -> Option<Step> {
        let hunks = self.hunks_in_order();
        let from = cursor
            .and_then(|(path, line)| self.hunk_at(path, line))
            .or_else(|| {
                let (path, seq) = self.at.as_ref()?;
                self.files
                    .get(path)?
                    .hunks
                    .iter()
                    .any(|hunk| hunk.seq == *seq)
                    .then_some(*seq)
            });
        let (Some(first), Some(last)) = (hunks.first(), hunks.last()) else {
            return None;
        };
        let Some(from) = from else {
            let target = if forward { first } else { last };
            return Some(Step::To(target.clone()));
        };
        let found = if forward {
            hunks.iter().find(|hunk| hunk.seq > from)
        } else {
            hunks.iter().rev().find(|hunk| hunk.seq < from)
        };
        Some(match found {
            Some(target) => Step::To(target.clone()),
            None if forward => Step::AtNewest,
            None => Step::AtOldest,
        })
    }

    /// `n`/`N` went to `target`: it and older hunks are seen. While watch mode is on, reaching
    /// the newest hunk follows again, and any other pauses following; returns whether it
    /// started following again.
    pub fn went_to(&mut self, target: &Target) -> bool {
        self.at = Some((target.path.clone(), target.seq));
        self.seen = self.seen.max(target.seq);
        if !self.active {
            return false;
        }
        let newest = self.newest_seq() == Some(target.seq);
        let again = newest && !self.following;
        self.following = newest;
        if newest {
            self.see_all();
        }
        again
    }

    /// Following jumped to `target`.
    pub fn followed(&mut self, target: &Target) {
        self.at = Some((target.path.clone(), target.seq));
        self.see_all();
    }

    /// Moving pauses following, while watch mode is on.
    pub fn pause(&mut self) {
        self.following = false;
    }

    /// Leaves watch mode (`Esc`): the working tree is still watched and the changes recorded,
    /// not followed; where you are and whether you were following stay for [`Session::resume`].
    pub fn leave(&mut self) {
        self.active = false;
    }

    /// Enters watch mode again, where it was left.
    pub fn resume(&mut self) {
        self.active = true;
    }

    /// Stops the session for good (`:watch-stop`): no more watching nor following; the changes
    /// stay.
    pub fn stop(&mut self) {
        self.active = false;
        self.following = false;
        self.watcher = None;
    }

    /// The working tree is watched: the session wasn't stopped.
    pub fn is_live(&self) -> bool {
        self.watcher.is_some()
    }

    /// The number of changes of the session: hunks of text files, and other files whole.
    pub fn change_count(&self) -> usize {
        self.files
            .values()
            .map(|file| {
                if file.navigable() {
                    file.hunks.len()
                } else {
                    1
                }
            })
            .sum()
    }
}

/// The watch of the editor: a session, if one was started and not cleared.
#[derive(Default)]
pub struct WatchState {
    pub session: Option<Session>,
}

impl WatchState {
    /// The active session.
    pub fn active(&mut self) -> Option<&mut Session> {
        self.session.as_mut().filter(|session| session.active)
    }

    pub fn is_active(&self) -> bool {
        self.session.as_ref().is_some_and(|session| session.active)
    }

    /// A session that wasn't stopped, watch mode on or not.
    pub fn live(&mut self) -> Option<&mut Session> {
        self.session.as_mut().filter(|session| session.is_live())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn baseline() -> Arc<Baseline> {
        Arc::new(Baseline::new(
            PathBuf::from("/repo"),
            PathBuf::from("/repo/.git"),
            None,
            false,
            DiffProviderRegistry::default(),
            HashMap::new(),
        ))
    }

    fn hunk(after: Range<u32>, key: u64) -> HunkLines {
        HunkLines {
            before: after.clone(),
            after,
            key,
        }
    }

    fn update(path: &str, hunks: Vec<HunkLines>, modified: u64) -> FileUpdate {
        FileUpdate {
            path: PathBuf::from(path),
            kind: FileKind::Modified,
            quiet: Quiet::No,
            content: hunks.iter().map(|hunk| hunk.key).sum(),
            hunks,
            modified: Some(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(modified)),
        }
    }

    fn order(session: &Session) -> Vec<(String, u32)> {
        session
            .hunks_in_order()
            .into_iter()
            .map(|hunk| (hunk.path.display().to_string(), hunk.lines.start))
            .collect()
    }

    #[test]
    fn hunks_keep_their_number_and_new_ones_come_last() {
        let mut session = Session::new(1, baseline());
        let applied = session.apply(vec![update("/repo/a", vec![hunk(1..2, 10)], 1)]);
        assert_eq!(applied.to_follow.last().cloned().unwrap().lines, 1..2);
        // a new hunk above the first one moves it: it keeps its number
        let applied = session.apply(vec![update(
            "/repo/a",
            vec![hunk(0..1, 20), hunk(2..3, 10)],
            2,
        )]);
        assert_eq!(applied.to_follow.last().cloned().unwrap().lines, 0..1);
        assert_eq!(
            order(&session),
            [("/repo/a".into(), 2), ("/repo/a".into(), 0)]
        );
    }

    #[test]
    fn a_burst_is_numbered_oldest_file_first_and_the_newest_is_followed() {
        let mut session = Session::new(1, baseline());
        let applied = session.apply(vec![
            update("/repo/b", vec![hunk(5..6, 1)], 9),
            update("/repo/a", vec![hunk(1..2, 2)], 3),
        ]);
        assert_eq!(
            applied.to_follow.last().cloned().unwrap().path,
            PathBuf::from("/repo/b")
        );
        assert_eq!(
            order(&session),
            [("/repo/a".into(), 1), ("/repo/b".into(), 5)]
        );
    }

    #[test]
    fn quiet_and_deleted_files_are_not_followed_counted_nor_gone_through() {
        let mut session = Session::new(1, baseline());
        session.pause();
        let mut lock = update("/repo/Cargo.lock", vec![], 1);
        lock.quiet = Quiet::Lockfile;
        let mut gone = update("/repo/old.rs", vec![], 2);
        gone.kind = FileKind::Deleted;
        let applied = session.apply(vec![lock, gone]);
        assert_eq!(applied.to_follow, []);
        assert_eq!(applied.changed.len(), 2);
        assert_eq!(session.new_count(), 0);
        assert!(session.step(None, true).is_none());
        assert_eq!(session.change_count(), 2);
        assert_eq!(
            session.files_newest_first()[0].0,
            &PathBuf::from("/repo/old.rs")
        );
    }

    #[test]
    fn a_file_back_to_its_baseline_is_dropped() {
        let mut session = Session::new(1, baseline());
        session.apply(vec![update("/repo/a", vec![hunk(1..2, 10)], 1)]);
        let mut back = update("/repo/a", vec![], 2);
        back.kind = FileKind::Unchanged;
        let applied = session.apply(vec![back]);
        assert_eq!(applied.changed, [PathBuf::from("/repo/a")]);
        assert!(session.is_empty());
    }

    #[test]
    fn paused_changes_are_counted_and_n_to_the_newest_follows_again() {
        let mut session = Session::new(1, baseline());
        let applied = session.apply(vec![update("/repo/a", vec![hunk(1..2, 1)], 1)]);
        session.followed(&applied.to_follow.last().cloned().unwrap());
        session.pause();
        session.apply(vec![update(
            "/repo/a",
            vec![hunk(1..2, 1), hunk(8..9, 2)],
            2,
        )]);
        session.apply(vec![update("/repo/b", vec![hunk(3..4, 3)], 3)]);
        assert_eq!(session.new_count(), 2);

        // from the hunk followed last, in the order they changed
        let target = to(session.step(None, true));
        assert_eq!(target.lines.start, 8);
        assert!(!session.went_to(&target));
        assert_eq!(session.new_count(), 1);
        let target = to(session.step(None, true));
        assert_eq!(target.path, PathBuf::from("/repo/b"));
        assert!(session.went_to(&target));
        assert!(session.following);
        assert_eq!(session.new_count(), 0);
        // at the newest, n goes nowhere
        assert_eq!(session.step(None, true), Some(Step::AtNewest));
        // back to an older one: reading, following pauses
        let target = to(session.step(None, false));
        assert!(!session.went_to(&target));
        assert!(!session.following);
    }

    fn to(step: Option<Step>) -> Target {
        match step {
            Some(Step::To(target)) => target,
            step => panic!("expected a hunk, got {step:?}"),
        }
    }

    #[test]
    fn n_goes_on_from_the_hunk_under_the_cursor_and_stops_at_the_ends() {
        let mut session = Session::new(1, baseline());
        session.apply(vec![update("/repo/a", vec![hunk(1..3, 1)], 1)]);
        session.apply(vec![update("/repo/b", vec![hunk(4..5, 2)], 2)]);
        let a = PathBuf::from("/repo/a");
        assert_eq!(to(session.step(Some((&a, 2)), true)).lines.start, 4);
        // no wrapping around: N from the oldest, n from the newest go nowhere
        assert_eq!(session.step(Some((&a, 2)), false), Some(Step::AtOldest));
        let b = PathBuf::from("/repo/b");
        assert_eq!(session.step(Some((&b, 4)), true), Some(Step::AtNewest));
        assert_eq!(to(session.step(Some((&b, 4)), false)).lines.start, 1);
        // outside the hunks, nothing gone to yet: n to the oldest, N to the newest
        assert_eq!(to(session.step(Some((&a, 9)), true)).lines.start, 1);
        assert_eq!(to(session.step(Some((&a, 9)), false)).lines.start, 4);
        // no hunks at all
        assert_eq!(Session::new(2, baseline()).step(None, true), None);
    }

    #[test]
    fn stopping_keeps_the_changes() {
        let mut session = Session::new(1, baseline());
        session.apply(vec![update("/repo/a", vec![hunk(1..3, 1)], 1)]);
        session.stop();
        assert!(!session.active && !session.following && !session.is_live());
        let target = to(session.step(None, true));
        // no following once stopped
        assert!(!session.went_to(&target));
        assert!(!session.following);
    }

    #[test]
    fn leaving_keeps_following_and_the_place_for_resuming() {
        let mut session = Session::new(1, baseline());
        session.watcher = Some(Box::new(()));
        let applied = session.apply(vec![update("/repo/a", vec![hunk(1..3, 1)], 1)]);
        session.followed(&applied.to_follow[0]);
        session.leave();
        assert!(!session.active && session.following && session.is_live());
        // changes while away are recorded, unseen
        session.apply(vec![update("/repo/b", vec![hunk(4..5, 2)], 2)]);
        assert_eq!(session.new_count(), 1);
        // n/N while away move nothing of the mode's state
        let target = to(session.step(None, true));
        assert!(!session.went_to(&target));
        assert!(session.following);
        session.resume();
        assert!(session.active && session.following);
        assert_eq!(session.at.as_ref().map(|(path, _)| path), Some(&PathBuf::from("/repo/b")));

        // looking around is kept the same way
        session.pause();
        session.leave();
        session.resume();
        assert!(!session.following);
    }
}
