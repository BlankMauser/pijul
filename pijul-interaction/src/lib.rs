//! Progress indicators for Pijul operations.

mod progress;

use progress::{ProgressBarTrait, SpinnerTrait};
use std::sync::OnceLock;

pub const DOWNLOAD_MESSAGE: &str = "Downloading changes";
pub const APPLY_MESSAGE: &str = "Applying changes";
pub const UPLOAD_MESSAGE: &str = "Uploading changes";
pub const COMPLETE_MESSAGE: &str = "Completing changes";
pub const OUTPUT_MESSAGE: &str = "Outputting repository";

static INTERACTIVE_CONTEXT: OnceLock<InteractiveContext> = OnceLock::new();

#[derive(thiserror::Error, Debug)]
#[non_exhaustive]
pub enum InteractionError {
    #[error("mode of interactivity not set")]
    NoContext,
}

pub fn get_context() -> Result<InteractiveContext, InteractionError> {
    INTERACTIVE_CONTEXT
        .get()
        .copied()
        .ok_or(InteractionError::NoContext)
}

pub fn set_context(value: InteractiveContext) {
    INTERACTIVE_CONTEXT
        .set(value)
        .expect("Interactive context is already set!");
}

#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub enum InteractiveContext {
    Terminal,
    NotInteractive,
}

/// A progress bar controlled by code.
pub struct ProgressBar(Box<dyn ProgressBarTrait>);

/// An animated spinner to indicate activity.
pub struct Spinner(Box<dyn SpinnerTrait>);
