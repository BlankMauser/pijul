use super::{BASE32, Base32};
pub(crate) const BLAKE3_BYTES: usize = 32;
#[cfg(feature = "git2")]
pub(crate) const GIT_SHA1_BYTES: usize = 20;
#[cfg(feature = "git2")]
pub(crate) const GIT_SHA2_BYTES: usize = 32;

/// The external hash of changes.
#[derive(Serialize, Deserialize, Clone, Copy, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub enum Hash {
    /// None is the hash of the "null change", which introduced a
    /// single root vertex at the beginning of the repository.
    None,
    Blake3([u8; BLAKE3_BYTES]),
    #[cfg(feature = "git2")]
    GitSha1([u8; 20]),
    #[cfg(feature = "git2")]
    GitSha2([u8; 32]),
}

pub enum Hasher {
    Blake3(blake3::Hasher),
}

impl Default for Hasher {
    fn default() -> Self {
        Hasher::Blake3(blake3::Hasher::new())
    }
}

impl Hasher {
    pub fn update(&mut self, bytes: &[u8]) {
        match self {
            Hasher::Blake3(h) => {
                h.update(bytes);
            }
        }
    }
    pub fn finish(&self) -> Hash {
        match self {
            Hasher::Blake3(h) => {
                let result = h.finalize();
                let mut hash = [0; BLAKE3_BYTES];
                hash.clone_from_slice(result.as_bytes());
                Hash::Blake3(hash)
            }
        }
    }
}

impl std::fmt::Debug for Hash {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(fmt, "{}", self.to_base32())
    }
}

/// Algorithm used to compute change hashes.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Ord, PartialOrd)]
#[repr(u8)]
pub enum HashAlgorithm {
    None = 0,
    Blake3 = 1,
    GitSha1 = 2,
    GitSha2 = 3,
}

impl Hash {
    pub fn to_bytes(&self) -> [u8; 1 + BLAKE3_BYTES] {
        match *self {
            Hash::None => unimplemented!(),
            Hash::Blake3(ref s) => {
                let mut out = [0; 1 + BLAKE3_BYTES];
                out[0] = HashAlgorithm::Blake3 as u8;
                out[1..].clone_from_slice(s);
                out
            }
            #[cfg(feature = "git2")]
            Hash::GitSha1(ref s) => {
                let mut out = [0; 1 + BLAKE3_BYTES];
                out[0] = HashAlgorithm::GitSha1 as u8;
                out[1..21].clone_from_slice(s);
                out
            }
            #[cfg(feature = "git2")]
            Hash::GitSha2(ref s) => {
                let mut out = [0; 1 + BLAKE3_BYTES];
                out[0] = HashAlgorithm::GitSha2 as u8;
                out[1..].clone_from_slice(s);
                out
            }
        }
    }

    #[allow(clippy::int_plus_one)]
    pub fn from_bytes(s: &[u8]) -> Option<Self> {
        if s.len() >= 1 + BLAKE3_BYTES && s[0] == HashAlgorithm::Blake3 as u8 {
            let mut out = [0; BLAKE3_BYTES];
            out.clone_from_slice(&s[1..]);
            return Some(Hash::Blake3(out));
        }

        #[cfg(feature = "git2")]
        if s.len() >= 1 + GIT_SHA1_BYTES && s[0] == HashAlgorithm::GitSha1 as u8 {
            let mut out = [0; GIT_SHA1_BYTES];
            out.clone_from_slice(&s[1..]);
            return Some(Hash::GitSha1(out));
        }

        #[cfg(feature = "git2")]
        if s.len() >= 1 + GIT_SHA2_BYTES && s[0] == HashAlgorithm::GitSha2 as u8 {
            let mut out = [0; GIT_SHA2_BYTES];
            out.clone_from_slice(&s[1..]);
            return Some(Hash::GitSha2(out));
        }

        None
    }
}

#[cfg(feature = "git2")]
impl TryInto<git2::Oid> for Hash {
    type Error = ();
    fn try_into(self) -> Result<git2::Oid, Self::Error> {
        match self {
            Hash::GitSha1(s) => Ok(git2::Oid::from_bytes(&s).unwrap()),
            Hash::GitSha2(s) => Ok(git2::Oid::from_bytes(&s).unwrap()),
            _ => Err(()),
        }
    }
}

pub fn prefix_guesses(s: &str) -> impl Iterator<Item = Hash> {
    #[cfg(not(feature = "git2"))]
    let hashes = [Hash::None, Hash::Blake3([0; BLAKE3_BYTES])];

    #[cfg(feature = "git2")]
    let hashes = [
        Hash::None,
        Hash::Blake3([0; BLAKE3_BYTES]),
        Hash::GitSha1([0; GIT_SHA1_BYTES]),
        Hash::GitSha2([0; GIT_SHA2_BYTES]),
    ];

    let from = Hash::from_base32(s.as_bytes());
    let from_is_some = from.is_some();
    let decode_len = (s.len() * 5) / 8;
    let s = s.split_at(BASE32.encode_len(decode_len)).0;
    debug!("decode_len {:?} {:?}", decode_len, from);
    from.into_iter()
        .chain(hashes.into_iter().filter_map(move |h| match h {
            _ if from_is_some => None,
            Hash::None if s == "A" || s == "AA" => Some(Hash::None),
            Hash::Blake3(mut b) if b.len() >= decode_len => {
                match BASE32.decode_mut(s.as_bytes(), &mut b[..decode_len]) {
                    Err(data_encoding::DecodePartial {
                        error:
                            data_encoding::DecodeError {
                                kind: data_encoding::DecodeKind::Symbol,
                                ..
                            },
                        ..
                    }) => None,
                    _ => Some(Hash::Blake3(b)),
                }
            }
            #[cfg(feature = "git2")]
            Hash::GitSha1(mut b) if b.len() >= decode_len => {
                match BASE32.decode_mut(s.as_bytes(), &mut b[..decode_len]) {
                    Err(data_encoding::DecodePartial {
                        error:
                            data_encoding::DecodeError {
                                kind: data_encoding::DecodeKind::Symbol,
                                ..
                            },
                        ..
                    }) => None,
                    _ => Some(Hash::GitSha1(b)),
                }
            }
            #[cfg(feature = "git2")]
            Hash::GitSha2(mut b) if b.len() >= decode_len => {
                match BASE32.decode_mut(s.as_bytes(), &mut b[..decode_len]) {
                    Err(data_encoding::DecodePartial {
                        error:
                            data_encoding::DecodeError {
                                kind: data_encoding::DecodeKind::Symbol,
                                ..
                            },
                        ..
                    }) => None,
                    _ => Some(Hash::GitSha2(b)),
                }
            }
            x => {
                debug!("none {:?}", x);
                None
            }
        }))
}

impl super::Base32 for Hash {
    /// Returns the base-32 representation of a hash.
    fn to_base32(&self) -> String {
        match *self {
            Hash::None => BASE32.encode(&[0]),
            Hash::Blake3(ref s) => {
                let mut hash = [0; 1 + BLAKE3_BYTES];
                hash[BLAKE3_BYTES] = HashAlgorithm::Blake3 as u8;
                hash[..BLAKE3_BYTES].clone_from_slice(s);
                BASE32.encode(&hash)
            }
            #[cfg(feature = "git2")]
            Hash::GitSha1(ref s) => {
                let mut hash = [0; 1 + GIT_SHA1_BYTES];
                hash[20] = HashAlgorithm::GitSha1 as u8;
                hash[..GIT_SHA1_BYTES].clone_from_slice(s);
                BASE32.encode(&hash)
            }
            #[cfg(feature = "git2")]
            Hash::GitSha2(ref s) => {
                let mut hash = [0; 1 + GIT_SHA2_BYTES];
                hash[20] = HashAlgorithm::GitSha2 as u8;
                hash[..GIT_SHA2_BYTES].clone_from_slice(s);
                BASE32.encode(&hash)
            }
        }
    }

    /// Parses a base-32 string into a hash.
    fn from_base32(s: &[u8]) -> Option<Self> {
        let bytes = if let Ok(s) = BASE32.decode(s) {
            s
        } else {
            return None;
        };
        if bytes == [0] {
            return Some(Hash::None);
        } else if bytes.len() == BLAKE3_BYTES + 1
            && bytes[BLAKE3_BYTES] == HashAlgorithm::Blake3 as u8
        {
            let mut hash = [0; BLAKE3_BYTES];
            hash.clone_from_slice(&bytes[..BLAKE3_BYTES]);
            return Some(Hash::Blake3(hash));
        }

        #[cfg(feature = "git2")]
        if bytes.len() == GIT_SHA1_BYTES + 1
            && bytes[GIT_SHA1_BYTES] == HashAlgorithm::GitSha1 as u8
        {
            let mut hash = [0; GIT_SHA1_BYTES];
            hash.clone_from_slice(&bytes[..GIT_SHA1_BYTES]);
            return Some(Hash::GitSha1(hash));
        }

        #[cfg(feature = "git2")]
        if bytes.len() == GIT_SHA2_BYTES + 1
            && bytes[GIT_SHA2_BYTES] == HashAlgorithm::GitSha2 as u8
        {
            let mut hash = [0; GIT_SHA2_BYTES];
            hash.clone_from_slice(&bytes[..GIT_SHA2_BYTES]);
            return Some(Hash::GitSha2(hash));
        }

        None
    }
}

impl std::str::FromStr for Hash {
    type Err = crate::ParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some(b) = Self::from_base32(s.as_bytes()) {
            Ok(b)
        } else {
            Err(crate::ParseError { s: s.to_string() })
        }
    }
}

#[test]
fn from_to() {
    let mut h = Hasher::default();
    h.update(b"blabla");
    let h = h.finish();
    assert_eq!(Hash::from_base32(&h.to_base32().as_bytes()), Some(h));
    let h = Hash::None;
    assert_eq!(Hash::from_base32(&h.to_base32().as_bytes()), Some(h));
    let b = BASE32.encode(&[19, 18, 17]);
    assert_eq!(Hash::from_base32(&b.as_bytes()), None);
}

#[derive(Clone, Copy)]
#[repr(C)]
pub struct SerializedHash {
    pub(crate) t: u8,
    h: H,
}

impl std::hash::Hash for SerializedHash {
    fn hash<H: std::hash::Hasher>(&self, hasher: &mut H) {
        self.t.hash(hasher);
        if self.t == HashAlgorithm::Blake3 as u8 {
            unsafe { self.h.blake3.hash(hasher) }
        }

        #[cfg(feature = "git2")]
        if self.t == HashAlgorithm::GitSha1 as u8 {
            unsafe { self.h.git_sha1.hash(hasher) }
        }

        #[cfg(feature = "git2")]
        if self.t == HashAlgorithm::GitSha2 as u8 {
            unsafe { self.h.git_sha2.hash(hasher) }
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) union H {
    none: (),
    blake3: [u8; BLAKE3_BYTES],
    #[cfg(feature = "git2")]
    git_sha1: [u8; GIT_SHA1_BYTES],
    #[cfg(feature = "git2")]
    git_sha2: [u8; GIT_SHA2_BYTES],
}

pub(crate) const HASH_NONE: SerializedHash = SerializedHash {
    t: HashAlgorithm::None as u8,
    h: H { none: () },
};

use std::cmp::Ordering;

impl PartialOrd for SerializedHash {
    fn partial_cmp(&self, b: &Self) -> Option<Ordering> {
        Some(self.cmp(b))
    }
}

impl Ord for SerializedHash {
    fn cmp(&self, b: &Self) -> Ordering {
        match self.t.cmp(&b.t) {
            Ordering::Equal => {
                if self.t == HashAlgorithm::Blake3 as u8 {
                    return unsafe { self.h.blake3.cmp(&b.h.blake3) };
                }

                #[cfg(feature = "git2")]
                if self.t == HashAlgorithm::GitSha1 as u8 {
                    return unsafe { self.h.git_sha1.cmp(&b.h.git_sha1) };
                } else if self.t == HashAlgorithm::GitSha2 as u8 {
                    return unsafe { self.h.git_sha2.cmp(&b.h.git_sha2) };
                }

                Ordering::Equal
            }
            o => o,
        }
    }
}

impl PartialEq for SerializedHash {
    fn eq(&self, b: &Self) -> bool {
        if self.t == HashAlgorithm::Blake3 as u8 && self.t == b.t {
            return unsafe { self.h.blake3 == b.h.blake3 };
        }

        #[cfg(feature = "git2")]
        if self.t == HashAlgorithm::GitSha1 as u8 && self.t == b.t {
            return unsafe { self.h.git_sha1 == b.h.git_sha1 };
        } else if self.t == HashAlgorithm::GitSha2 as u8 && self.t == b.t {
            return unsafe { self.h.git_sha2 == b.h.git_sha2 };
        }

        self.t == b.t
    }
}
impl Eq for SerializedHash {}

impl std::fmt::Debug for SerializedHash {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::fmt::Result {
        Hash::from(self).fmt(fmt)
    }
}

impl<'a> From<&'a SerializedHash> for Hash {
    fn from(s: &'a SerializedHash) -> Hash {
        if s.t == HashAlgorithm::Blake3 as u8 {
            return Hash::Blake3(unsafe { s.h.blake3 });
        }

        #[cfg(feature = "git2")]
        if s.t == HashAlgorithm::GitSha1 as u8 {
            return Hash::GitSha1(unsafe { s.h.git_sha1 });
        } else if s.t == HashAlgorithm::GitSha2 as u8 {
            return Hash::GitSha2(unsafe { s.h.git_sha2 });
        }

        if s.t == HashAlgorithm::None as u8 {
            Hash::None
        } else {
            panic!("Unknown hash algorithm {:?}", s.t)
        }
    }
}

#[cfg(feature = "git2")]
impl From<git2::Oid> for Hash {
    fn from(s: git2::Oid) -> Hash {
        let b = s.as_bytes();
        if b.len() == 20 {
            let mut h = [0; GIT_SHA1_BYTES];
            h.clone_from_slice(b);
            Hash::GitSha1(h)
        } else {
            assert_eq!(b.len(), GIT_SHA2_BYTES);
            let mut h = [0; GIT_SHA2_BYTES];
            h.clone_from_slice(b);
            Hash::GitSha2(h)
        }
    }
}

impl From<SerializedHash> for Hash {
    fn from(s: SerializedHash) -> Hash {
        (&s).into()
    }
}

impl From<Hash> for SerializedHash {
    fn from(s: Hash) -> SerializedHash {
        (&s).into()
    }
}

impl<'a> From<&'a Hash> for SerializedHash {
    fn from(s: &'a Hash) -> Self {
        match s {
            Hash::Blake3(s) => SerializedHash {
                t: HashAlgorithm::Blake3 as u8,
                h: H { blake3: *s },
            },
            #[cfg(feature = "git2")]
            Hash::GitSha1(s) => SerializedHash {
                t: HashAlgorithm::GitSha1 as u8,
                h: H { git_sha1: *s },
            },
            #[cfg(feature = "git2")]
            Hash::GitSha2(s) => SerializedHash {
                t: HashAlgorithm::GitSha2 as u8,
                h: H { git_sha2: *s },
            },
            Hash::None => SerializedHash {
                t: 0,
                h: H { none: () },
            },
        }
    }
}

impl SerializedHash {
    pub fn size(b: &[u8]) -> usize {
        if b[0] == HashAlgorithm::Blake3 as u8 {
            return 1 + BLAKE3_BYTES;
        }

        #[cfg(feature = "git2")]
        if b[0] == HashAlgorithm::GitSha1 as u8 {
            return 1 + GIT_SHA1_BYTES;
        }

        #[cfg(feature = "git2")]
        if b[0] == HashAlgorithm::GitSha2 as u8 {
            return 1 + GIT_SHA2_BYTES;
        }

        if b[0] == HashAlgorithm::None as u8 {
            1
        } else {
            panic!("Unknown hash algorithm {:?}", b[0])
        }
    }
}
