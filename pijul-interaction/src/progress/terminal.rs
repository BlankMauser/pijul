use std::sync::{Arc, LazyLock};
use std::time::Duration;

use super::{ProgressBarTrait, SpinnerTrait};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

static MULTI_PROGRESS: LazyLock<MultiProgress> = LazyLock::new(MultiProgress::new);

pub fn new_progress(len: u64, message: String) -> Arc<ProgressBar> {
    let style =
        ProgressStyle::with_template("  {msg:<28} {bar:40.cyan/237} {pos:>4}/{len:<4} {eta:>4}")
            .unwrap()
            .progress_chars("━━╸");

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
    let style = ProgressStyle::with_template("  {spinner:.cyan}  {msg}")
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
            self.set_style(ProgressStyle::with_template("  {spinner:.green}  {msg}").unwrap());
            self.finish_with_message(format!("{}… done", self.message()));
        }
    }

    fn boxed_clone(&self) -> Box<dyn SpinnerTrait> {
        Box::new(self.clone())
    }
}
