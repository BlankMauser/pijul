mod terminal;

use super::{ProgressBar, Spinner};
use crate::{InteractionError, InteractiveContext};

pub trait ProgressBarTrait: Send {
    fn inc(&self, delta: u64);
    fn finish(&self);
    fn boxed_clone(&self) -> Box<dyn ProgressBarTrait>;
}

/// A no-op progress bar used when not attached to a terminal.
struct HiddenProgressBar;

impl ProgressBarTrait for HiddenProgressBar {
    fn inc(&self, _delta: u64) {}
    fn finish(&self) {}
    fn boxed_clone(&self) -> Box<dyn ProgressBarTrait> {
        Box::new(HiddenProgressBar)
    }
}

impl ProgressBar {
    pub fn new<S: ToString>(len: u64, message: S) -> Result<ProgressBar, InteractionError> {
        Ok(Self(match crate::get_context()? {
            InteractiveContext::Terminal => {
                Box::new(terminal::new_progress(len, message.to_string()))
            }
            InteractiveContext::NotInteractive => Box::new(HiddenProgressBar),
        }))
    }

    /// A progress bar that shows patch titles as they complete, keeping the most
    /// recent [`crate::progress_window`] on screen. `titles` must be in the order
    /// the patches are processed (so `inc(1)` advances through them). Falls back
    /// to a hidden bar when not attached to a terminal.
    pub fn with_titles<S: ToString>(
        titles: Vec<String>,
        message: S,
    ) -> Result<ProgressBar, InteractionError> {
        Ok(Self(match crate::get_context()? {
            InteractiveContext::Terminal => Box::new(terminal::new_progress_titled(
                message.to_string(),
                titles,
                crate::progress_window(),
            )),
            InteractiveContext::NotInteractive => Box::new(HiddenProgressBar),
        }))
    }

    pub fn inc(&self, delta: u64) {
        self.0.inc(delta);
    }

    fn finish(&self) {
        self.0.finish()
    }
}

impl Drop for ProgressBar {
    fn drop(&mut self) {
        self.finish();
    }
}

impl Clone for ProgressBar {
    fn clone(&self) -> Self {
        Self(self.0.boxed_clone())
    }
}

pub trait SpinnerTrait: Send {
    fn finish(&self);
    fn boxed_clone(&self) -> Box<dyn SpinnerTrait>;
}

/// A no-op spinner used when not attached to a terminal.
struct HiddenSpinner;

impl SpinnerTrait for HiddenSpinner {
    fn finish(&self) {}
    fn boxed_clone(&self) -> Box<dyn SpinnerTrait> {
        Box::new(HiddenSpinner)
    }
}

impl Spinner {
    pub fn new<S: ToString>(message: S) -> Result<Spinner, InteractionError> {
        Ok(Self(match crate::get_context()? {
            InteractiveContext::Terminal => Box::new(terminal::new_spinner(message.to_string())),
            InteractiveContext::NotInteractive => Box::new(HiddenSpinner),
        }))
    }

    fn finish(&self) {
        self.0.finish();
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        self.finish();
    }
}

impl Clone for Spinner {
    fn clone(&self) -> Self {
        Self(self.0.boxed_clone())
    }
}
