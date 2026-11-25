//! Finds the OSC 133 shell integration markers in the output of the shell, before it goes to
//! the emulator (which ignores them):
//!
//! - `ESC ] 133 ; A ST`: the prompt starts
//! - `ESC ] 133 ; B ST`: the prompt ends, the user types the command
//! - `ESC ] 133 ; C ST`: the command runs
//! - `ESC ] 133 ; D [; exit code] ST`: the command finished
//!
//! where ST is `BEL` or `ESC \`. A marker may be split between two reads.
//!
//! Two more sequences come through the same scanner: OSC 7 (`ESC ] 7 ; file://host/path ST`), the
//! shell's working directory, and Vortex's own OSC 6973 (`ESC ] 6973 ; hex ST`), the command line
//! the shell is editing, in hexadecimal bytes, which the integration sends when asked (editing
//! the command in a Vortex buffer).

/// A shell integration marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Marker {
    /// `A`: the prompt starts.
    PromptStart,
    /// `B`: the prompt ends, the user types the command.
    CommandStart,
    /// `C`: the command runs.
    CommandExecuted,
    /// `D`: the command finished, with its exit code when the shell gave one.
    CommandFinished(Option<i32>),
    /// OSC 7: the shell's working directory.
    Cwd(std::path::PathBuf),
    /// OSC 6973 (Vortex's integration): the command line being edited at the prompt.
    CommandLine(String),
}

/// The longest OSC that is kept to look at; longer ones (clipboard, images...) are skipped. Long
/// enough for a working directory and a command line in hexadecimal.
const MAX_OSC_LEN: usize = 32 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum State {
    #[default]
    Ground,
    /// Saw `ESC` outside of an OSC.
    Escape,
    /// Inside `ESC ]` ...
    Osc,
    /// Saw `ESC` inside an OSC: `\` ends it.
    OscEscape,
}

/// Incremental scanner for the markers: feed it the output of the shell chunk by chunk.
#[derive(Debug, Default)]
pub struct Scanner {
    state: State,
    buf: Vec<u8>,
    /// The OSC being read is too long to be a marker.
    overflow: bool,
}

impl Scanner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Scans a chunk of output, returning the markers that ended in it, in order. The bytes
    /// themselves are not changed: the emulator still gets all of them.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Marker> {
        self.feed_at(bytes)
            .into_iter()
            .map(|(_, marker)| marker)
            .collect()
    }

    /// As [`Self::feed`], with where each marker ends in `bytes`: the index of the byte after
    /// its terminator, so that the output before it can go to the emulator first.
    pub fn feed_at(&mut self, bytes: &[u8]) -> Vec<(usize, Marker)> {
        let mut markers = Vec::new();
        for (at, &byte) in bytes.iter().enumerate() {
            let end = at + 1;
            match self.state {
                State::Ground => {
                    if byte == 0x1b {
                        self.state = State::Escape;
                    }
                }
                State::Escape => match byte {
                    b']' => self.start_osc(),
                    0x1b => {}
                    _ => self.state = State::Ground,
                },
                State::Osc => match byte {
                    0x07 => self.end_osc(end, &mut markers),
                    0x1b => self.state = State::OscEscape,
                    // CAN and SUB cancel the sequence.
                    0x18 | 0x1a => self.state = State::Ground,
                    _ => {
                        if self.buf.len() < MAX_OSC_LEN {
                            self.buf.push(byte);
                        } else {
                            self.overflow = true;
                        }
                    }
                },
                State::OscEscape => match byte {
                    b'\\' => self.end_osc(end, &mut markers),
                    // `ESC ]` starts a new OSC, anything else is a new escape sequence.
                    b']' => self.start_osc(),
                    0x1b => {}
                    _ => self.state = State::Ground,
                },
            }
        }
        markers
    }

    fn start_osc(&mut self) {
        self.state = State::Osc;
        self.buf.clear();
        self.overflow = false;
    }

    fn end_osc(&mut self, end: usize, markers: &mut Vec<(usize, Marker)>) {
        self.state = State::Ground;
        if !self.overflow {
            if let Some(marker) = parse(&self.buf) {
                markers.push((end, marker));
            }
        }
        self.buf.clear();
    }
}

/// Parses the body of an OSC: `133;A`, `133;D;1`, `133;D;1;key=value`...
fn parse(body: &[u8]) -> Option<Marker> {
    if let Some(url) = body.strip_prefix(b"7;") {
        return parse_cwd(url).map(Marker::Cwd);
    }
    if let Some(hex) = body.strip_prefix(b"6973;") {
        return parse_hex(hex).map(Marker::CommandLine);
    }
    let rest = body.strip_prefix(b"133;")?;
    let mut params = rest.split(|&b| b == b';');
    let code = params.next()?;
    match code {
        b"A" => Some(Marker::PromptStart),
        b"B" => Some(Marker::CommandStart),
        b"C" => Some(Marker::CommandExecuted),
        b"D" => {
            let exit_code = params
                .next()
                .and_then(|p| std::str::from_utf8(p).ok())
                .and_then(|p| p.parse::<i32>().ok());
            Some(Marker::CommandFinished(exit_code))
        }
        _ => None,
    }
}

/// `file://host/path` (percent-encoded) to the path. The host is not checked: the shell runs on
/// this machine.
fn parse_cwd(url: &[u8]) -> Option<std::path::PathBuf> {
    let rest = url.strip_prefix(b"file://")?;
    let path = &rest[rest.iter().position(|&b| b == b'/')?..];
    let bytes = percent_decode(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Some(std::ffi::OsString::from_vec(bytes).into())
    }
    #[cfg(not(unix))]
    {
        String::from_utf8(bytes).ok().map(Into::into)
    }
}

fn percent_decode(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Some(out)
}

/// Hexadecimal bytes (any case, whitespace ignored) to text; invalid UTF-8 is replaced.
fn parse_hex(hex: &[u8]) -> Option<String> {
    let digits: Vec<u8> = hex
        .iter()
        .copied()
        .filter(|b| !b.is_ascii_whitespace())
        .collect();
    if !digits.len().is_multiple_of(2) {
        return None;
    }
    let bytes = digits
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
        .collect::<Option<Vec<u8>>>()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(bytes: &[u8]) -> Vec<Marker> {
        Scanner::new().feed(bytes)
    }

    /// Feeds `bytes` in two chunks split at every position, and byte by byte.
    fn scan_everywhere(bytes: &[u8]) -> Vec<Vec<Marker>> {
        let mut all = Vec::new();
        for at in 0..=bytes.len() {
            let mut scanner = Scanner::new();
            let mut markers = scanner.feed(&bytes[..at]);
            markers.extend(scanner.feed(&bytes[at..]));
            all.push(markers);
        }
        let mut scanner = Scanner::new();
        all.push(bytes.iter().flat_map(|b| scanner.feed(&[*b])).collect());
        all
    }

    #[test]
    fn each_marker() {
        assert_eq!(scan(b"\x1b]133;A\x07"), [Marker::PromptStart]);
        assert_eq!(scan(b"\x1b]133;B\x07"), [Marker::CommandStart]);
        assert_eq!(scan(b"\x1b]133;C\x07"), [Marker::CommandExecuted]);
        assert_eq!(
            scan(b"\x1b]133;D;3\x07"),
            [Marker::CommandFinished(Some(3))]
        );
    }

    #[test]
    fn string_terminator() {
        assert_eq!(scan(b"\x1b]133;A\x1b\\"), [Marker::PromptStart]);
        assert_eq!(
            scan(b"\x1b]133;D;7\x1b\\"),
            [Marker::CommandFinished(Some(7))]
        );
    }

    #[test]
    fn finished_with_and_without_code() {
        assert_eq!(scan(b"\x1b]133;D\x07"), [Marker::CommandFinished(None)]);
        assert_eq!(scan(b"\x1b]133;D;\x07"), [Marker::CommandFinished(None)]);
        assert_eq!(
            scan(b"\x1b]133;D;0\x07"),
            [Marker::CommandFinished(Some(0))]
        );
        assert_eq!(
            scan(b"\x1b]133;D;-1\x07"),
            [Marker::CommandFinished(Some(-1))]
        );
        assert_eq!(
            scan(b"\x1b]133;D;130\x07"),
            [Marker::CommandFinished(Some(130))]
        );
    }

    #[test]
    fn extra_params() {
        assert_eq!(
            scan(b"\x1b]133;D;2;aid=12\x07"),
            [Marker::CommandFinished(Some(2))]
        );
        assert_eq!(
            scan(b"\x1b]133;D;err=1\x07"),
            [Marker::CommandFinished(None)]
        );
        assert_eq!(scan(b"\x1b]133;A;aid=1;cl=m\x07"), [Marker::PromptStart]);
        assert_eq!(
            scan(b"\x1b]133;C;cmdline=ls\x1b\\"),
            [Marker::CommandExecuted]
        );
    }

    #[test]
    fn other_sequences_are_not_markers() {
        assert_eq!(scan(b"\x1b]0;title\x07"), []);
        assert_eq!(scan(b"\x1b]1330;A\x07"), []);
        assert_eq!(scan(b"\x1b]133;X\x07"), []);
        assert_eq!(scan(b"\x1b]133;AB\x07"), []);
        assert_eq!(scan(b"\x1b[31m133;A\x07"), []);
        assert_eq!(scan(b"133;A"), []);
        // Not terminated yet.
        assert_eq!(scan(b"\x1b]133;A"), []);
        // Cancelled.
        assert_eq!(scan(b"\x1b]133;A\x18\x07"), []);
    }

    #[test]
    fn long_sequences_are_skipped() {
        let mut bytes = b"\x1b]52;c;".to_vec();
        bytes.extend(vec![b'A'; 10_000]);
        bytes.extend_from_slice(b"\x07\x1b]133;C\x07");
        assert_eq!(scan(&bytes), [Marker::CommandExecuted]);
    }

    #[test]
    fn text_around_markers() {
        let bytes = b"hello\r\n\x1b[1mworld\x1b[0m\x1b]133;D;1\x07\x1b]133;A\x07$ \x1b]133;B\x07ls\r\n\x1b]133;C\x07out";
        let want = [
            Marker::CommandFinished(Some(1)),
            Marker::PromptStart,
            Marker::CommandStart,
            Marker::CommandExecuted,
        ];
        assert_eq!(scan(bytes), want);
        for markers in scan_everywhere(bytes) {
            assert_eq!(markers, want);
        }
    }

    #[test]
    fn split_at_every_position() {
        for bytes in [
            &b"\x1b]133;D;12\x07"[..],
            b"\x1b]133;D;12\x1b\\",
            b"\x1b]133;D;12;aid=3\x07",
        ] {
            for markers in scan_everywhere(bytes) {
                assert_eq!(markers, [Marker::CommandFinished(Some(12))]);
            }
        }
        for markers in scan_everywhere(b"\x1b]133;A\x1b\\\x1b]133;C\x07") {
            assert_eq!(markers, [Marker::PromptStart, Marker::CommandExecuted]);
        }
    }

    #[test]
    fn working_directory() {
        assert_eq!(
            scan(b"\x1b]7;file://host/home/me/my%20dir\x07"),
            [Marker::Cwd("/home/me/my dir".into())]
        );
        assert_eq!(
            scan(b"\x1b]7;file:///tmp\x1b\\"),
            [Marker::Cwd("/tmp".into())]
        );
        assert_eq!(scan(b"\x1b]7;http://x/y\x07"), []);
    }

    #[test]
    fn command_line() {
        assert_eq!(
            scan(b"\x1b]6973;6c73202d6c\x07"),
            [Marker::CommandLine("ls -l".into())]
        );
        assert_eq!(
            scan(b"\x1b]6973;\x07"),
            [Marker::CommandLine(String::new())]
        );
        // od's output: spaces and newlines between the bytes
        assert_eq!(
            scan(b"\x1b]6973; 65 63\n 68 6f 0a 78\x07"),
            [Marker::CommandLine("echo\nx".into())]
        );
        assert_eq!(scan(b"\x1b]6973;6\x07"), []);
    }

    #[test]
    fn where_markers_end() {
        let bytes = b"ab\x1b]133;A\x07cd\x1b]133;C\x1b\\";
        assert_eq!(
            Scanner::new().feed_at(bytes),
            [(10, Marker::PromptStart), (21, Marker::CommandExecuted)]
        );
    }

    #[test]
    fn osc_restarted_by_escape() {
        assert_eq!(scan(b"\x1b]0;ti\x1b]133;A\x07"), [Marker::PromptStart]);
        assert_eq!(scan(b"\x1b]133;A\x1b[0m\x07"), []);
    }
}
