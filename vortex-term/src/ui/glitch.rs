//! The glitch (this fork, see FORK.md, "Theme effects"): a reusable effect that makes cells of
//! the screen show random characters for a short time, then go back to what they were. The
//! dashboard's title and background and the ghost text of error diagnostics use it, each with
//! the rate its theme sets (`[effects]`).
//!
//! - **Rate**: a chance per cell per second. Glitches are a Poisson process over the cells on
//!   screen: with `n` cells at rate `r`, the time to the next glitch is drawn from an
//!   exponential distribution of rate `n × r`, and the glitch lands on one of the cells at
//!   random. The process is memoryless, so when the number of cells changes the time is simply
//!   drawn again from the frame.
//! - **Characters**: ASCII only ([`CHARS`]), one column each. A glitch lasts [`DURATION`] and
//!   shows a different character every [`STEP`]: four distinct ones by default.
//! - **No constant redraws**: the owner draws a frame, then [`Glitch::schedule`] asks for one
//!   more frame at the next change, a new glitch, a glitch's next character or one ending, and
//!   nothing else. With no cell on
//!   screen (or a rate of 0) nothing is asked for and the editor stays idle.
//!
//! The random source is seeded once per run; nothing is saved.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::time::{Duration, Instant};

use tui::buffer::Buffer as Surface;

/// The characters a glitched cell shows: letters, digits and symbols, ASCII only, for every
/// terminal and font.
pub const CHARS: &[u8] =
    b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz@#$%&*+=-:;/\\|<>?!";

/// How long a glitch shows, by default.
pub const DURATION: Duration = Duration::from_millis(400);

/// How long a glitched cell shows each of its characters: a glitch of [`DURATION`] shows four.
pub const STEP: Duration = Duration::from_millis(100);

/// A small random source (splitmix64): fast, good enough for looks, seeded per run.
#[derive(Debug, Clone)]
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A number in (0, 1].
    fn unit(&mut self) -> f64 {
        ((self.next_u64() >> 11) + 1) as f64 / (1u64 << 53) as f64
    }

    fn below(&mut self, n: usize) -> usize {
        (((self.next_u64() >> 32) * n as u64) >> 32) as usize
    }
}

/// A seed that differs from run to run, without a dependency: std's random hasher keys.
fn run_seed() -> u64 {
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(0x5eed);
    hasher.finish()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Active {
    /// The cell, as an index into the owner's cells of the frame.
    cell: usize,
    /// The characters it shows, one per [`STEP`], all different.
    chars: Vec<char>,
    start: Instant,
    until: Instant,
}

impl Active {
    /// The index of the character shown at `now`.
    fn step_at(&self, now: Instant) -> usize {
        let steps = now.saturating_duration_since(self.start).as_nanos() / STEP.as_nanos();
        (steps as usize).min(self.chars.len() - 1)
    }

    /// When it shows its next character, or ends.
    fn next_change(&self, now: Instant) -> Instant {
        let next = self.start + STEP * (self.step_at(now) as u32 + 1);
        next.min(self.until)
    }
}

/// A frame asked for at `at`.
#[derive(Debug)]
struct Wake {
    at: Instant,
    task: Option<tokio::task::AbortHandle>,
}

/// The glitches of one set of cells (a title, a background, the ghost text of diagnostics).
#[derive(Debug)]
pub struct Glitch {
    rate: f64,
    duration: Duration,
    rng: Rng,
    /// The time of the next glitch, drawn for `cells` cells.
    next: Option<Instant>,
    cells: usize,
    active: Vec<Active>,
    /// The time of the last frame, for the characters shown and the next change.
    now: Option<Instant>,
    wake: Option<Wake>,
    /// The glitches started so far, for the tests' statistics.
    started: u64,
}

impl Glitch {
    /// Glitches at `rate`, a chance per cell per second, seeded for this run.
    pub fn new(rate: f64) -> Self {
        Self::seeded(rate, run_seed())
    }

    /// Glitches with a given seed: the same seed and frames give the same glitches.
    pub fn seeded(rate: f64, seed: u64) -> Self {
        Self {
            rate: rate.max(0.0),
            duration: DURATION,
            rng: Rng(seed),
            next: None,
            cells: 0,
            active: Vec::new(),
            now: None,
            wake: None,
            started: 0,
        }
    }

    /// How long a glitch shows.
    pub fn with_duration(mut self, duration: Duration) -> Self {
        self.duration = duration;
        self
    }

    pub fn rate(&self) -> f64 {
        self.rate
    }

    /// The time to the next glitch among `cells` cells.
    fn gap(&mut self, cells: usize) -> Duration {
        let per_second = self.rate * cells as f64;
        Duration::from_secs_f64((-self.rng.unit().ln() / per_second).min(3600.0))
    }

    /// Brings the glitches to `now`, for a frame showing `cells` cells (visible ones only):
    /// glitches that ended are gone, those due since the last frame start.
    pub fn advance(&mut self, now: Instant, cells: usize) {
        self.now = Some(now);
        if cells != self.cells {
            // memoryless: the time to the next glitch is drawn again for the new count
            self.cells = cells;
            self.next = None;
        }
        self.active
            .retain(|glitch| glitch.until > now && glitch.cell < cells);
        if cells == 0 || self.rate <= 0.0 {
            self.next = None;
            return;
        }
        let mut next = match self.next {
            Some(next) => next,
            None => now + self.gap(cells),
        };
        // after a long pause (no frame drawn), the glitches that would already be over don't
        // matter: start again from the oldest time one could still show
        if let Some(oldest) = now.checked_sub(self.duration) {
            if next < oldest {
                next = oldest + self.gap(cells);
            }
        }
        while next <= now {
            let until = next + self.duration;
            if until > now {
                let cell = self.rng.below(cells);
                let chars = self.distinct_chars(self.char_count());
                self.active.retain(|glitch| glitch.cell != cell);
                self.active.push(Active {
                    cell,
                    chars,
                    start: next,
                    until,
                });
                self.started += 1;
            }
            next += self.gap(cells);
        }
        self.next = Some(next);
    }

    /// How many characters a glitch shows: one per [`STEP`] of its duration, at least one.
    fn char_count(&self) -> usize {
        let steps = self.duration.as_nanos().div_ceil(STEP.as_nanos());
        (steps as usize).clamp(1, CHARS.len())
    }

    /// `count` different characters of [`CHARS`], in a random order.
    fn distinct_chars(&mut self, count: usize) -> Vec<char> {
        let mut chars: Vec<char> = Vec::with_capacity(count);
        while chars.len() < count {
            let c = CHARS[self.rng.below(CHARS.len())] as char;
            if !chars.contains(&c) {
                chars.push(c);
            }
        }
        chars
    }

    /// The glitched cells of the frame: their index among the frame's cells and the character
    /// they show now.
    pub fn glitched(&self) -> impl Iterator<Item = (usize, char)> + '_ {
        let now = self.now;
        self.active.iter().map(move |glitch| {
            let step = now.map_or(0, |now| glitch.step_at(now));
            (glitch.cell, glitch.chars[step])
        })
    }

    /// When the glitches change next: a new one, a glitch's next character, or one ending.
    /// `None` when nothing will.
    pub fn next_change(&self) -> Option<Instant> {
        let now = self.now;
        self.active
            .iter()
            .map(|glitch| match now {
                Some(now) => glitch.next_change(now),
                None => glitch.until,
            })
            .chain(self.next)
            .min()
    }

    /// Asks the editor for a frame at the next change, unless a frame is already asked for at
    /// or before it. Outside a tokio runtime (unit tests) the wake-up is only remembered.
    pub fn schedule(&mut self, now: Instant) {
        let Some(at) = self.next_change() else {
            self.cancel();
            return;
        };
        if let Some(wake) = &self.wake {
            let pending =
                wake.at > now && wake.task.as_ref().is_none_or(|task| !task.is_finished());
            if pending && wake.at <= at {
                return;
            }
        }
        self.cancel();
        let task = tokio::runtime::Handle::try_current().ok().map(|runtime| {
            runtime
                .spawn(async move {
                    tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await;
                    vortex_event::request_redraw();
                })
                .abort_handle()
        });
        self.wake = Some(Wake { at, task });
    }

    /// The frame asked for, if any.
    pub fn wake_at(&self) -> Option<Instant> {
        self.wake.as_ref().map(|wake| wake.at)
    }

    fn cancel(&mut self) {
        if let Some(Wake {
            task: Some(task), ..
        }) = self.wake.take()
        {
            task.abort();
        }
    }

    /// The glitches started so far.
    pub fn started(&self) -> u64 {
        self.started
    }

    /// One frame over the cells at `cells` of `surface` (visible ones only, in a stable order):
    /// brings the glitches to `now`, draws the glitched cells' characters in their own style,
    /// and asks for a frame at the next change.
    pub fn draw(&mut self, now: Instant, surface: &mut Surface, cells: &[(u16, u16)]) {
        self.advance(now, cells.len());
        for (index, c) in self.glitched() {
            let (x, y) = cells[index];
            if let Some(cell) = surface.get_mut(x, y) {
                cell.set_char(c);
            }
        }
        self.schedule(now);
    }
}

impl Drop for Glitch {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// A glitch at the rate a theme sets, made when the rate is set and dropped when it is 0, made
/// again when it changes (another theme): what owners keep across frames.
#[derive(Debug, Default)]
pub struct Slot(Option<Glitch>);

impl Slot {
    /// The glitch at `rate`, or `None` when the rate is 0.
    pub fn at(&mut self, rate: f64) -> Option<&mut Glitch> {
        if rate <= 0.0 {
            self.0 = None;
            return None;
        }
        if self.0.as_ref().is_none_or(|glitch| glitch.rate() != rate) {
            self.0 = Some(Glitch::new(rate));
        }
        self.0.as_mut()
    }

    /// The glitch, if there is one, without changing it.
    pub fn get(&self) -> Option<&Glitch> {
        self.0.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vortex_core::unicode::width::UnicodeWidthChar;
    use vortex_view::graphics::{Color, Rect, Style};

    const FRAME: Duration = Duration::from_millis(16);

    /// Frames every 16 ms for `seconds` simulated seconds over `cells` cells.
    fn run_frames(glitch: &mut Glitch, start: Instant, cells: usize, seconds: u64) -> Instant {
        let end = start + Duration::from_secs(seconds);
        let mut now = start;
        while now < end {
            glitch.advance(now, cells);
            now += FRAME;
        }
        now
    }

    /// Drives the glitch only at the times it asks for, from one first frame, until `end`.
    /// Returns the number of frames drawn.
    fn run_on_demand(glitch: &mut Glitch, start: Instant, cells: usize, seconds: u64) -> usize {
        let end = start + Duration::from_secs(seconds);
        let mut now = start;
        let mut frames = 1;
        glitch.advance(now, cells);
        glitch.schedule(now);
        while let Some(at) = glitch.next_change() {
            if at > end {
                break;
            }
            assert!(at > now, "a frame asked for at or before the last one");
            assert_eq!(glitch.wake_at(), Some(at));
            now = at;
            glitch.advance(now, cells);
            glitch.schedule(now);
            frames += 1;
        }
        frames
    }

    fn assert_near(value: f64, mean: f64, sds: f64, what: &str) {
        let sd = mean.sqrt();
        assert!(
            (value - mean).abs() <= sds * sd,
            "{what}: {value} is not within {sds} sd ({sd:.1}) of {mean}"
        );
    }

    #[test]
    fn a_high_rate_starts_rate_times_cells_times_seconds_glitches() {
        for seed in 1..=5 {
            let mut glitch = Glitch::seeded(0.6, seed);
            run_frames(&mut glitch, Instant::now(), 150, 60);
            assert_near(
                glitch.started() as f64,
                0.6 * 150.0 * 60.0,
                5.0,
                "high rate",
            );
        }
    }

    #[test]
    fn a_low_rate_starts_rate_times_cells_times_seconds_glitches() {
        for seed in 1..=5 {
            let mut glitch = Glitch::seeded(0.01, seed);
            run_on_demand(&mut glitch, Instant::now(), 40, 2000);
            assert_near(
                glitch.started() as f64,
                0.01 * 40.0 * 2000.0,
                5.0,
                "low rate",
            );
        }
    }

    /// A lone glitch on one cell for each seed (no second one before it ends): its start.
    fn lone_glitches(duration: Duration) -> impl Iterator<Item = (Glitch, Instant)> {
        (1..=50).filter_map(move |seed| {
            let mut glitch = Glitch::seeded(0.5, seed).with_duration(duration);
            let t0 = Instant::now();
            glitch.advance(t0, 1);
            assert_eq!(glitch.glitched().count(), 0);
            let start = glitch.next_change().unwrap();
            glitch.advance(start, 1);
            assert_eq!(glitch.glitched().count(), 1);
            // a second glitch before this one ends would replace it: skip those seeds
            (glitch.next.unwrap() >= start + duration + Duration::from_millis(5))
                .then_some((glitch, start))
        })
    }

    #[test]
    fn a_glitch_lasts_0_4_s_and_shows_a_character_every_0_1_s() {
        assert_eq!(DURATION, Duration::from_millis(400));
        assert_eq!(STEP, Duration::from_millis(100));
        let one = Duration::from_millis(1);
        let mut checked = 0;
        for (mut glitch, start) in lone_glitches(DURATION) {
            checked += 1;
            let mut shown = Vec::new();
            for step in 0..4u32 {
                let from = start + STEP * step;
                // the same character for the whole step
                let mut chars = Vec::new();
                for at in [from, from + one, from + STEP / 2, from + STEP - one] {
                    glitch.advance(at, 1);
                    let glitched: Vec<_> = glitch.glitched().collect();
                    assert_eq!(glitched.len(), 1, "step {step}");
                    assert_eq!(glitched[0].0, 0);
                    chars.push(glitched[0].1);
                }
                assert!(chars.windows(2).all(|w| w[0] == w[1]), "{chars:?}");
                // the next frame is asked for at the next step, or at the end
                assert_eq!(glitch.next_change(), Some(from + STEP));
                shown.push(chars[0]);
            }
            // four characters, all different
            let mut distinct = shown.clone();
            distinct.sort_unstable();
            distinct.dedup();
            assert_eq!(distinct.len(), 4, "{shown:?}");
            glitch.advance(start + DURATION, 1);
            assert_eq!(glitch.glitched().count(), 0);
        }
        assert!(checked > 20, "{checked}");
    }

    #[test]
    fn a_custom_duration_shows_a_character_per_started_step() {
        let one = Duration::from_millis(1);
        for (duration, count) in [
            (Duration::from_millis(350), 4),
            (Duration::from_millis(100), 1),
            (Duration::from_millis(50), 1),
            (Duration::from_millis(250), 3),
        ] {
            let mut checked = 0;
            for (mut glitch, start) in lone_glitches(duration) {
                checked += 1;
                let mut shown = Vec::new();
                let mut at = start;
                while at < start + duration {
                    glitch.advance(at, 1);
                    let (_, c) = glitch.glitched().next().unwrap();
                    if shown.last() != Some(&c) {
                        shown.push(c);
                    }
                    at += one;
                }
                assert_eq!(shown.len(), count, "{duration:?}: {shown:?}");
                // the last step is cut short by the end
                glitch.advance(start + duration - one, 1);
                assert_eq!(glitch.next_change(), Some(start + duration));
                glitch.advance(start + duration, 1);
                assert_eq!(glitch.glitched().count(), 0, "{duration:?}");
            }
            assert!(checked > 20, "{duration:?}: {checked}");
        }
    }

    #[test]
    fn the_characters_of_a_glitch_are_always_distinct() {
        let mut glitch = Glitch::seeded(5.0, 13);
        let mut now = Instant::now();
        let mut glitches = 0;
        for _ in 0..2000 {
            glitch.advance(now, 50);
            for active in &glitch.active {
                assert_eq!(active.chars.len(), 4);
                let mut chars = active.chars.clone();
                chars.sort_unstable();
                chars.dedup();
                assert_eq!(chars.len(), 4, "{:?}", active.chars);
            }
            now += FRAME;
        }
        glitches += glitch.started();
        assert!(glitches > 1000, "{glitches}");
    }

    #[test]
    fn the_share_of_glitched_cells_is_rate_times_duration() {
        for (rate, duration) in [(0.2, DURATION), (0.1, Duration::from_millis(250))] {
            let mut glitch = Glitch::seeded(rate, 42).with_duration(duration);
            let cells = 100;
            let mut now = Instant::now();
            glitch.advance(now, cells);
            let samples = 5000;
            let mut glitched = 0;
            for _ in 0..samples {
                now += Duration::from_secs(1);
                glitch.advance(now, cells);
                glitched += glitch.glitched().count();
            }
            let share = glitched as f64 / (samples * cells) as f64;
            let expected = rate * duration.as_secs_f64();
            assert!(
                (share - expected).abs() < expected * 0.1,
                "{share} vs {expected}"
            );
        }
    }

    #[test]
    fn glitched_characters_are_ascii_of_one_column() {
        let mut glitch = Glitch::seeded(0.6, 7);
        let mut now = Instant::now();
        let mut seen = std::collections::HashSet::new();
        for _ in 0..4000 {
            glitch.advance(now, 150);
            for (_, c) in glitch.glitched() {
                assert!(CHARS.contains(&(c as u8)), "{c:?}");
                assert!(c.is_ascii(), "{c:?}");
                assert_eq!(UnicodeWidthChar::width(c), Some(1), "{c:?}");
                seen.insert(c);
            }
            now += FRAME;
        }
        // all of them come up in the long run
        assert!(seen.len() > CHARS.len() * 9 / 10, "{}", seen.len());
        assert!(CHARS.iter().all(|c| c.is_ascii() && !c.is_ascii_control()));
    }

    #[test]
    fn it_only_asks_for_frames_at_changes() {
        let mut glitch = Glitch::seeded(0.01, 3);
        let seconds = 1000;
        let frames = run_on_demand(&mut glitch, Instant::now(), 40, seconds);
        let started = glitch.started() as usize;
        assert!(started > 300, "{started}");
        // per glitch, one frame to start it, one for each of its three other characters and one
        // to end it, and the first; a glitch replaced on its cell before it ends has fewer
        assert!(
            frames + 5 >= 4 * started && frames <= 5 * started + 5,
            "{frames} frames for {started} glitches"
        );
        assert!(frames < seconds as usize * 30 / 10, "{frames}");
    }

    #[test]
    fn nothing_is_asked_for_without_cells_or_rate() {
        let now = Instant::now();
        for (rate, cells) in [(0.5, 0), (0.0, 40), (0.0, 0)] {
            let mut glitch = Glitch::seeded(rate, 1);
            glitch.advance(now, cells);
            assert_eq!(glitch.next_change(), None);
            glitch.schedule(now);
            assert_eq!(glitch.wake_at(), None);
            glitch.advance(now + Duration::from_secs(100), cells);
            glitch.schedule(now + Duration::from_secs(100));
            assert_eq!(glitch.next_change(), None);
            assert_eq!(glitch.wake_at(), None);
            assert_eq!(glitch.started(), 0);
        }
        // and a negative rate is no rate
        assert_eq!(Glitch::seeded(-1.0, 1).rate(), 0.0);
        // a glitch that had cells and loses them stops asking
        let mut glitch = Glitch::seeded(0.5, 1);
        glitch.advance(now, 10);
        glitch.schedule(now);
        assert!(glitch.wake_at().is_some());
        glitch.advance(now, 0);
        assert_eq!(glitch.next_change(), None);
        glitch.schedule(now);
        assert_eq!(glitch.wake_at(), None);
    }

    #[test]
    fn the_next_wake_up_comes_from_the_rate() {
        for (rate, cells) in [(0.5, 10), (0.02, 100), (3.0, 1)] {
            let now = Instant::now();
            let seeds = 4000;
            let total: f64 = (0..seeds)
                .map(|seed| {
                    let mut glitch = Glitch::seeded(rate, seed * 7919 + 1);
                    glitch.advance(now, cells);
                    assert_eq!(glitch.glitched().count(), 0);
                    (glitch.next_change().unwrap() - now).as_secs_f64()
                })
                .sum();
            let mean = total / seeds as f64;
            let expected = 1.0 / (rate * cells as f64);
            // the sd of the mean of an exponential is its mean over sqrt(n)
            assert!(
                (mean - expected).abs() < 5.0 * expected / (seeds as f64).sqrt(),
                "{mean} vs {expected}"
            );
        }
    }

    #[test]
    fn only_visible_cells_glitch() {
        let mut glitch = Glitch::seeded(2.0, 11);
        let mut now = Instant::now();
        for frame in 0..3000 {
            let cells = [1, 5, 40, 3, 0, 17][frame / 7 % 6];
            glitch.advance(now, cells);
            assert!(glitch.glitched().all(|(cell, _)| cell < cells));
            now += FRAME;
        }
        // glitches on cells beyond the new count are dropped at once
        let mut glitch = Glitch::seeded(5.0, 2);
        let now = Instant::now();
        glitch.advance(now, 200);
        let mut now = now;
        while glitch.glitched().filter(|&(cell, _)| cell >= 5).count() < 3 {
            now += FRAME;
            glitch.advance(now, 200);
        }
        glitch.advance(now, 5);
        assert!(glitch.glitched().all(|(cell, _)| cell < 5));
        assert!(glitch.next_change().is_some());
    }

    #[test]
    fn draw_changes_only_the_listed_cells_and_keeps_their_style() {
        let area = Rect::new(0, 0, 20, 3);
        let style = Style::default().fg(Color::Red).bg(Color::Blue);
        let mut base = Surface::empty(area);
        for y in 0..3 {
            for x in 0..20 {
                base.set_string(x, y, ".", style);
            }
        }
        let listed: Vec<(u16, u16)> = (2..12).map(|x| (x, 1)).collect();
        let mut glitch = Glitch::seeded(50.0, 5);
        let t0 = Instant::now();
        let mut changed = std::collections::HashSet::new();
        for frame in 0..40 {
            let mut surface = base.clone();
            glitch.draw(t0 + FRAME * frame, &mut surface, &listed);
            for y in 0..3 {
                for x in 0..20 {
                    let cell = &surface[(x, y)];
                    assert_eq!((cell.fg, cell.bg), (Color::Red, Color::Blue));
                    if cell.symbol.as_str() != "." {
                        assert!(listed.contains(&(x, y)), "{x},{y} is not listed");
                        let c = cell.symbol.chars().next().unwrap();
                        assert!(CHARS.contains(&(c as u8)), "{c:?}");
                        changed.insert((x, y));
                    }
                }
            }
        }
        assert!(changed.len() > 5, "{changed:?}");
        // a draw asks for the next frame
        assert!(glitch.wake_at().is_some());
        // no cells listed: nothing changes
        let mut surface = base.clone();
        glitch.draw(t0 + Duration::from_secs(5), &mut surface, &[]);
        assert_eq!(surface, base);
    }

    #[test]
    fn a_slot_follows_the_rate() {
        let mut slot = Slot::default();
        assert!(slot.get().is_none());
        assert!(slot.at(0.0).is_none());
        assert!(slot.at(-1.0).is_none());
        let now = Instant::now();
        {
            let glitch = slot.at(10.0).unwrap();
            assert_eq!(glitch.rate(), 10.0);
            run_frames(glitch, now, 10, 2);
            assert!(glitch.started() > 0);
        }
        // the same rate keeps the same glitch
        let started = slot.at(10.0).unwrap().started();
        assert!(started > 0);
        assert_eq!(slot.get().unwrap().started(), started);
        // another rate makes a new one
        let glitch = slot.at(20.0).unwrap();
        assert_eq!(glitch.rate(), 20.0);
        assert_eq!(glitch.started(), 0);
        run_frames(glitch, now, 10, 1);
        assert!(slot.get().unwrap().started() > 0);
        // a rate of 0 drops it
        assert!(slot.at(0.0).is_none());
        assert!(slot.get().is_none());
        // and it starts again from nothing
        assert_eq!(slot.at(20.0).unwrap().started(), 0);
    }

    #[test]
    fn the_same_seed_and_frames_give_the_same_glitches() {
        let now = Instant::now();
        let record = |seed| {
            let mut glitch = Glitch::seeded(0.8, seed);
            let mut out = Vec::new();
            for frame in 0..500 {
                glitch.advance(now + FRAME * frame, 60);
                out.push(glitch.glitched().collect::<Vec<_>>());
            }
            (out, glitch.started())
        };
        assert_eq!(record(9), record(9));
        assert_ne!(record(9), record(10));
    }
}
