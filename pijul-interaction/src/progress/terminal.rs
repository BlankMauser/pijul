use std::sync::{Arc, LazyLock};
use std::time::Duration;

use super::{ProgressBarTrait, SpinnerTrait};
use indicatif::{MultiProgress, ProgressBar, ProgressState, ProgressStyle};

static MULTI_PROGRESS: LazyLock<MultiProgress> = LazyLock::new(MultiProgress::new);

fn colors_enabled() -> bool {
    std::env::var_os("NO_COLOR").is_none()
}

fn fg(w: &mut dyn std::fmt::Write, (r, g, b): (u8, u8, u8)) {
    if colors_enabled() {
        let _ = write!(w, "\x1b[38;2;{r};{g};{b}m");
    }
}

fn reset(w: &mut dyn std::fmt::Write) {
    if colors_enabled() {
        let _ = w.write_str("\x1b[0m");
    }
}

/// Format `{rate}` as a compact throughput figure, e.g. `1.2k/s`.
fn render_rate(state: &ProgressState, w: &mut dyn std::fmt::Write) {
    let per_sec = state.per_sec();
    if !per_sec.is_finite() || per_sec <= 0.0 {
        let _ = w.write_str("  --/s");
        return;
    }
    let s = if per_sec >= 1_000_000.0 {
        format!("{:.1}M/s", per_sec / 1_000_000.0)
    } else if per_sec >= 1_000.0 {
        format!("{:.1}k/s", per_sec / 1_000.0)
    } else {
        format!("{:.0}/s", per_sec)
    };
    let _ = write!(w, "{s:>6}");
}

/// Green used for a completed patch's bullet, and the "all done" summary.
const DONE_GREEN: (u8, u8, u8) = (0x87, 0xd7, 0x5f);
/// Cyan used for the diamond on the patch currently in flight.
const ACTIVE_CYAN: (u8, u8, u8) = (0x5f, 0xd7, 0xd7);

/// Bullet marking a completed patch (monochrome, single-width — renders and
/// aligns everywhere, unlike a colour emoji).
const DONE_BULLET: &str = "●";
/// The in-flight patch pulses between a filled and hollow diamond, a calm
/// heartbeat that signals activity without a busy spinner.
const ACTIVE_FILLED: &str = "◆";
const ACTIVE_HOLLOW: &str = "◇";

/// Longest patch title we print before eliding, to keep each line on one row —
/// wrapping would desync indicatif's multi-line redraw.
const MAX_TITLE: usize = 64;

/// `{count}` — the factual `[done/total]` figure.
fn render_count(state: &ProgressState, w: &mut dyn std::fmt::Write) {
    let len = state.len().unwrap_or(0);
    let _ = write!(w, "[{}/{}]", state.pos(), len);
}

/// `{elapsed_c}` — compact elapsed wall-clock, e.g. `3s`, `1m04s`, `2h07m`.
fn render_elapsed(state: &ProgressState, w: &mut dyn std::fmt::Write) {
    let _ = w.write_str(&fmt_elapsed(state.elapsed()));
}

fn fmt_elapsed(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    }
}

/// Truncate a title to `MAX_TITLE` display chars, appending `…` when elided.
/// Newlines are flattened so a multi-line message can never break the layout.
fn truncate_title(title: &str) -> String {
    let flat = title.replace(['\n', '\r'], " ");
    let flat = flat.trim();
    if flat.chars().count() <= MAX_TITLE {
        flat.to_string()
    } else {
        let head: String = flat.chars().take(MAX_TITLE - 1).collect();
        format!("{head}…")
    }
}

/// The half-open range `[lo, hi)` of title indices to show: the trailing
/// `window` items ending at the in-flight patch (index `pos`), or, once every
/// patch is done (`pos == total`), the last `window` completed ones.
fn window_bounds(pos: usize, total: usize, window: usize) -> (usize, usize) {
    let window = window.max(1);
    let hi = (pos + 1).min(total);
    let lo = hi.saturating_sub(window);
    (lo, hi)
}

/// Render the sliding window of patch titles plus, when the batch overflows the
/// window, a one-line summary. `pos` patches are done; index `pos` (if any) is
/// the one currently transferring.
fn render_patch_window(
    titles: &[String],
    window: usize,
    message: &str,
    state: &ProgressState,
    w: &mut dyn std::fmt::Write,
) {
    let total = titles.len();
    let pos = (state.pos() as usize).min(total);
    let (lo, hi) = window_bounds(pos, total, window);

    // A slow (~400ms) pulse for the in-flight diamond, derived from elapsed time
    // so it animates in step with the steady tick without any external counter.
    let filled = (state.elapsed().as_millis() / 400) % 2 == 0;

    let overflow = total > window;
    // Newlines go *between* rows only, so the block never grows a trailing empty
    // line (which would desync indicatif's multi-line redraw).
    let mut first = true;
    for i in lo..hi {
        if !first {
            let _ = w.write_str("\n");
        }
        first = false;
        let done = i < pos;
        if done {
            fg(w, DONE_GREEN);
            let _ = write!(w, "{DONE_BULLET}  ");
        } else {
            fg(w, ACTIVE_CYAN);
            let glyph = if filled { ACTIVE_FILLED } else { ACTIVE_HOLLOW };
            let _ = write!(w, "{glyph}  ");
        }
        reset(w);
        let _ = w.write_str(&truncate_title(&titles[i]));
    }

    // When the batch overflows the window, append a summary row reusing the same
    // factual fields as the plain bar so the two layouts read alike.
    if overflow {
        if !first {
            let _ = w.write_str("\n");
        }
        let percent = (state.fraction() * 100.0).round() as u64;
        let _ = write!(w, "{message:<21}  {percent:>3}%  ");
        render_count(state, w);
        let _ = w.write_str("  ");
        render_rate(state, w);
        let _ = w.write_str("  ");
        let _ = w.write_str(&fmt_elapsed(state.elapsed()));
    }
}

pub fn new_progress(len: u64, message: String) -> Arc<ProgressBar> {
    // Every field here is a fact about work already done — elapsed time, the
    // count of patches finished/total, and the measured throughput. No "eta":
    // a guess about the future has no place in a status line.
    let style =
        ProgressStyle::with_template("{msg:<21}  {percent:>3}%  {count}  {rate}  {elapsed_c}")
            .unwrap()
            .with_key("rate", render_rate)
            .with_key("count", render_count)
            .with_key("elapsed_c", render_elapsed);

    let pb = ProgressBar::new(len)
        .with_style(style)
        .with_message(message);
    MULTI_PROGRESS.add(pb.clone());
    pb.enable_steady_tick(Duration::from_millis(80));

    Arc::new(pb)
}

/// A progress bar that lists the patch titles as they complete, keeping a
/// sliding window of the most recent `window` (so you always see *where you
/// are*). When there are more patches than fit in the window, a factual summary
/// line with the percent and `[done/total]` count is appended.
pub fn new_progress_titled(
    message: String,
    titles: Vec<String>,
    window: usize,
) -> Arc<ProgressBar> {
    let len = titles.len() as u64;
    let titles = Arc::new(titles);
    let window = window.max(1);
    let summary_message = message.clone();

    let style = ProgressStyle::with_template("{patch_window}")
        .unwrap()
        .with_key(
            "patch_window",
            move |state: &ProgressState, w: &mut dyn std::fmt::Write| {
                render_patch_window(&titles, window, &summary_message, state, w)
            },
        );

    let pb = ProgressBar::new(len)
        .with_style(style)
        .with_message(message);
    MULTI_PROGRESS.add(pb.clone());
    pb.enable_steady_tick(Duration::from_millis(80));

    Arc::new(pb)
}

impl ProgressBarTrait for Arc<ProgressBar> {
    fn inc(&self, delta: u64) {
        self.as_ref().inc(delta);
    }

    fn finish(&self) {
        if Arc::strong_count(self) == 1 {
            self.as_ref().finish();
        }
    }

    fn boxed_clone(&self) -> Box<dyn ProgressBarTrait> {
        Box::new(self.clone())
    }
}

pub fn new_spinner(message: String) -> Arc<ProgressBar> {
    let style = ProgressStyle::with_template("{spinner:.cyan}  {msg}")
        .unwrap()
        .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏", "✓"]);

    let spinner = ProgressBar::new_spinner()
        .with_style(style)
        .with_message(message);
    spinner.enable_steady_tick(Duration::from_millis(80));
    MULTI_PROGRESS.add(spinner.clone());

    Arc::new(spinner)
}

impl SpinnerTrait for Arc<ProgressBar> {
    fn finish(&self) {
        if Arc::strong_count(self) == 1 {
            self.set_style(ProgressStyle::with_template("{spinner:.green}  {msg}").unwrap());
            self.finish_with_message(format!("{}… done", self.message()));
        }
    }

    fn boxed_clone(&self) -> Box<dyn SpinnerTrait> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_tracks_the_active_patch() {
        // Nothing done yet: show just the in-flight first patch.
        assert_eq!(window_bounds(0, 5, 10), (0, 1));
        // A few done, batch fits the window: show all up to the active one.
        assert_eq!(window_bounds(3, 5, 10), (0, 4));
        // All done, batch fits: whole list stays visible.
        assert_eq!(window_bounds(5, 5, 10), (0, 5));
    }

    #[test]
    fn window_scrolls_when_batch_overflows() {
        // 100 patches, window 10, patch 20 in flight: trailing 10 ending at 20.
        assert_eq!(window_bounds(20, 100, 10), (11, 21));
        // Finished: last 10 completed.
        assert_eq!(window_bounds(100, 100, 10), (90, 100));
        // Start of a big batch: can't show more than exist yet.
        assert_eq!(window_bounds(0, 100, 10), (0, 1));
    }

    #[test]
    fn window_of_one_is_valid() {
        assert_eq!(window_bounds(5, 100, 0), (5, 6)); // clamped to >=1
        assert_eq!(window_bounds(5, 100, 1), (5, 6));
    }

    #[test]
    fn titles_are_truncated_and_flattened() {
        assert_eq!(truncate_title("short"), "short");
        assert_eq!(truncate_title("a\nb\nc"), "a b c");
        let long = "x".repeat(200);
        let t = truncate_title(&long);
        assert_eq!(t.chars().count(), MAX_TITLE);
        assert!(t.ends_with('…'));
    }

    #[test]
    fn elapsed_is_compact_and_factual() {
        assert_eq!(fmt_elapsed(Duration::from_secs(3)), "3s");
        assert_eq!(fmt_elapsed(Duration::from_secs(64)), "1m04s");
        assert_eq!(fmt_elapsed(Duration::from_secs(3 * 3600 + 7 * 60)), "3h07m");
    }
}
