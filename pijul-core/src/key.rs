use data_encoding::{BASE64_MIME, BASE64_NOPAD};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PublicKey {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<jiff::Timestamp>,
    pub key: String,
}

impl PublicKey {
    /// SSH fingerprint in the form `SHA256:<base64>`.
    pub fn fingerprint(&self) -> String {
        let wire = BASE64_MIME.decode(self.key.as_bytes()).unwrap_or_default();
        format!("SHA256:{}", BASE64_NOPAD.encode(&Sha256::digest(&wire)))
    }
}

/// Identifies which SSH-agent key to use for signing; carries no private material.
pub struct SKey {
    pub public_key: PublicKey,
}

impl SKey {
    pub fn new(public_key: PublicKey) -> Self {
        SKey { public_key }
    }
}

/// A change signature produced via sshsig.
#[derive(Debug, Serialize, Deserialize)]
pub struct Signature {
    pub version: u64,
    pub key: PublicKey,
    /// PEM-armoured sshsig blob.
    pub signature: String,
    pub date: jiff::Timestamp,
}
