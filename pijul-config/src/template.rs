use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Template {
    pub message: Option<PathBuf>,
    pub description: Option<PathBuf>,
}
