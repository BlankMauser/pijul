use thiserror::Error;

#[derive(Error)]
pub enum Error<T: pijul_core::GraphTxnT + pijul_core::TreeTxnT> {
    #[error(transparent)]
    Remote(#[from] RemoteError),
    #[error(transparent)]
    Interaction(#[from] pijul_interaction::InteractionError),
    #[error("{0}")]
    Graph(<T as pijul_core::GraphTxnT>::GraphError),
    #[error(transparent)]
    TxnErrGraph(pijul_core::pristine::TxnErr<<T as pijul_core::GraphTxnT>::GraphError>),
    #[error(transparent)]
    SmallString(#[from] pijul_core::small_string::Error),
    #[error(transparent)]
    Sanakirja(#[from] pijul_core::pristine::sanakirja::SanakirjaError),
    #[error(transparent)]
    TxnErrSanakirja(
        #[from] pijul_core::pristine::TxnErr<pijul_core::pristine::sanakirja::SanakirjaError>,
    ),
    #[error(transparent)]
    Archive(
        #[from]
        pijul_core::output::ArchiveError<
            pijul_core::changestore::filesystem::Error,
            T,
            std::io::Error,
        >,
    ),
    #[error("Concurrency")]
    Concurrency,
    #[error(transparent)]
    Apply(#[from] pijul_core::ApplyError<pijul_core::changestore::filesystem::Error, T>),
    #[error(transparent)]
    FsError(#[from] pijul_core::fs::FsErrorC<pijul_core::changestore::filesystem::Error, T>),
}

impl<T: pijul_core::GraphTxnT + pijul_core::TreeTxnT> std::fmt::Debug for Error<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Error::Remote(e) => std::fmt::Debug::fmt(e, f),
            Error::Interaction(e) => std::fmt::Debug::fmt(e, f),
            Error::Graph(e) => std::fmt::Debug::fmt(e, f),
            Error::TxnErrGraph(e) => std::fmt::Debug::fmt(e, f),
            Error::SmallString(e) => std::fmt::Debug::fmt(e, f),
            Error::Sanakirja(e) => std::fmt::Debug::fmt(e, f),
            Error::TxnErrSanakirja(e) => std::fmt::Debug::fmt(e, f),
            Error::Archive(e) => std::fmt::Debug::fmt(e, f),
            Error::Concurrency => write!(f, "Concurrency"),
            Error::Apply(e) => std::fmt::Debug::fmt(e, f),
            Error::FsError(e) => std::fmt::Debug::fmt(e, f),
        }
    }
}

#[derive(Debug, Error)]
pub enum RemoteError {
    // Standard transparent wrappers
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Reqwest(#[from] reqwest::Error),
    #[error(transparent)]
    Thrussh(#[from] thrussh::Error),
    #[error(transparent)]
    ThrusshKeys(#[from] thrussh_keys::Error),
    #[error(transparent)]
    SerdeJson(#[from] serde_json::Error),
    #[error(transparent)]
    FromUtf8(#[from] std::string::FromUtf8Error),
    #[error(transparent)]
    Utf8(#[from] std::str::Utf8Error),
    #[error(transparent)]
    Sanakirja(#[from] pijul_core::pristine::sanakirja::SanakirjaError),
    #[error(transparent)]
    SmallString(#[from] pijul_core::small_string::Error),
    #[error(transparent)]
    HashPrefix(
        #[from]
        pijul_core::pristine::HashPrefixError<pijul_core::pristine::sanakirja::SanakirjaError>,
    ),
    #[error(transparent)]
    TxnSanakirja(
        #[from] pijul_core::pristine::TxnErr<pijul_core::pristine::sanakirja::SanakirjaError>,
    ),
    #[error(transparent)]
    TreeSanakirja(
        #[from] pijul_core::pristine::TreeErr<pijul_core::pristine::sanakirja::SanakirjaError>,
    ),
    #[error(transparent)]
    Config(#[from] pijul_config::ConfigError),
    #[error(transparent)]
    Identity(#[from] pijul_identity::IdentityError),
    #[error(transparent)]
    Repository(#[from] pijul_repository::RepositoryError),
    #[error(transparent)]
    CoreRemote(#[from] pijul_core::RemoteError),
    #[error(transparent)]
    Url(#[from] url::ParseError),
    #[error(transparent)]
    JoinError(#[from] tokio::task::JoinError),
    #[error(transparent)]
    Interaction(#[from] pijul_interaction::InteractionError),
    #[error(transparent)]
    ChangeFile(#[from] pijul_core::change::ChangeError),
    #[error(transparent)]
    Filesystem(#[from] pijul_core::changestore::filesystem::Error),
    #[error("Concurrency")]
    Concurrency,

    // Apply/unrecord semantic errors (no allocation)
    #[error("Dependency missing: {hash:?}")]
    DependencyMissing { hash: pijul_core::pristine::Hash },
    #[error("Change already on channel: {hash:?}")]
    ChangeAlreadyOnChannel { hash: pijul_core::pristine::Hash },
    #[error("Invalid change")]
    InvalidChange,
    #[error("Corruption detected")]
    Corruption,

    // Protocol-level errors
    #[error("{0}")]
    Local(<crate::local::LocalTxn as pijul_core::GraphTxnT>::GraphError),
    #[error("{0}")]
    LocalTxn(
        pijul_core::pristine::TxnErr<<crate::local::LocalTxn as pijul_core::GraphTxnT>::GraphError>,
    ),
    #[error(transparent)]
    Apply(
        #[from]
        pijul_core::ApplyError<pijul_core::changestore::filesystem::Error, crate::local::LocalTxn>,
    ),
    #[error("Remote not found: {0:?}")]
    RemoteNotFound(String),
    #[error("Ambiguous path: {0:?}")]
    AmbiguousPath(String),
    #[error("Remote scheme not supported: {0:?}")]
    UnsupportedScheme(String),
    #[error("Channel not found")]
    ChannelNotFound,
    #[error("Channel {0} does not exist in repository {1}")]
    LocalChannelNotFound(String, String),
    #[error("State not found: {0:?}")]
    StateNotFound(pijul_core::pristine::Merkle),
    #[error("Cannot apply tag {tag:?}: channel does not have that state")]
    TagNotInChannel { tag: pijul_core::pristine::Merkle },
    #[error("Not authenticated. Please check your credentials and try again.")]
    SshAuthFailed,
    #[error("Remote exited with status {0:?}")]
    RemoteExited(u32),
    #[error("HTTP error: {0}")]
    HttpError(reqwest::StatusCode),
    #[error("HTTP server returned an error: {0}")]
    HttpServerError(String),
    #[error("Repository `{0}` not found (404)")]
    HttpNotFound(String),
    #[error("Tag already downloaded: {0:?}")]
    TagAlreadyDownloaded(pijul_core::pristine::Merkle),
    #[error("Protocol error")]
    ProtocolError,
    #[error("{0}")]
    SshRemoteError(String),
    #[error("Failed to decode local repository name")]
    InvalidRepositoryName,
    #[error("No parent path")]
    NoPathParent,
    #[error("Internal channel closed")]
    ChannelClosed,
    #[error("Unsupported operation: {0}")]
    UnsupportedOperation(&'static str),
    #[error("Remote sent an error")]
    RemoteSentAnError,
    #[error("{0}")]
    RemoteError(String),
}
