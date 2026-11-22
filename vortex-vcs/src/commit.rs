/// A commit, as listed by the commit log picker.
#[derive(Clone)]
pub struct CommitInfo {
    /// Full hex id.
    pub id: String,
    /// The id abbreviated as git does (`core.abbrev`, 7 characters at least), unique in the
    /// repository.
    pub short_id: String,
    /// First line of the message.
    pub summary: String,
    pub author: String,
    /// Author date, `YYYY-MM-DD`.
    pub date: String,
}

/// A row of the commit log: a commit, or the divider that starts a section of the log (the
/// commits of a branch).
#[derive(Clone)]
pub enum LogEntry {
    /// The divider of the section that follows.
    Divider(Divider),
    Commit(CommitInfo),
}

/// The divider that starts a section of the commit log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Divider {
    /// The name of the section: `main (unpushed)`, `origin/main`...
    pub label: String,
    /// Whether the section's commits are on no remote: the commits of the current branch that
    /// its upstream doesn't have, or all of those not on the base branch when it has no upstream.
    pub unpushed: bool,
}
