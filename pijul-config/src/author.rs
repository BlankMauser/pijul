use std::path::PathBuf;

use serde::*;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Author {
    // Older versions called this 'name', but 'username' is more descriptive
    #[serde(alias = "name", default, skip_serializing_if = "String::is_empty")]
    pub username: String,
    #[serde(alias = "full_name", default, skip_serializing_if = "String::is_empty")]
    pub display_name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub email: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub origin: String,
    // This has been moved to identity::Config, but we should still be able to read the values
    #[serde(default, skip_serializing)]
    pub key_path: Option<PathBuf>,
}

impl Default for Author {
    fn default() -> Self {
        Self {
            username: String::new(),
            email: String::new(),
            display_name: whoami::realname().unwrap_or(String::new()),
            origin: String::new(),
            key_path: None,
        }
    }
}
