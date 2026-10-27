//! Interactive records write the change as text for the editor and read it
//! back. These tests pin which changes survive that round-trip, and that the
//! recorder flags the others (`Recorded::unprintable`), so that `pijul record`
//! asks for `-a` instead of recording them with missing contents.

use crate::change::*;
use crate::changestore::*;
use crate::pristine::*;
use crate::record::{Algorithm, Builder, Unprintable};
use crate::working_copy::*;
use crate::*;
use std::io::Write;

use super::*;

/// Binary contents of `len` bytes (the NUL byte makes `decode_file` return no
/// encoding).
fn binary_contents(len: usize, seed: u8) -> Vec<u8> {
    (0..len as u32 - 1)
        .map(|i| (i.wrapping_mul(2654435761) >> 24) as u8 ^ seed)
        .chain(std::iter::once(0))
        .collect()
}

struct Recorded {
    change: Change,
    updatables: HashMap<usize, InodeUpdate>,
    unprintable: Vec<Unprintable>,
    store: changestore::memory::Memory,
}

/// Record (without applying, as `pijul record` before opening the editor)
/// the addition of `file` with `after`, or if `before` is given, its
/// modification from `before` to `after`.
fn record(before: Option<Vec<u8>>, after: Vec<u8>) -> Result<Recorded, anyhow::Error> {
    env_logger::try_init().unwrap_or(());
    let repo = working_copy::memory::Memory::new();
    let store = changestore::memory::Memory::new();
    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    let channel = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;
    if let Some(before) = before {
        repo.add_file("file", before);
        txn.write().add_file("file", 0)?;
        record_all(&repo, &store, &txn, &channel, "")?;
        repo.write_file("file", Inode::ROOT)?.write_all(&after)?;
    } else {
        repo.add_file("file", after);
        txn.write().add_file("file", 0)?;
    }

    let mut state = Builder::new();
    state.record(
        txn.clone(),
        Algorithm::default(),
        false,
        &crate::DEFAULT_SEPARATOR,
        channel.clone(),
        &repo,
        &store,
        "",
        1,
    )?;
    let rec = state.finish();
    assert!(!rec.actions.is_empty(), "nothing to record");
    let actions = rec
        .actions
        .into_iter()
        .map(|a| a.globalize(&*txn.read()).unwrap())
        .collect();
    let change = Change::make_change(
        &*txn.read(),
        &channel,
        actions,
        std::mem::take(&mut *rec.contents.lock()),
        ChangeHeader {
            message: "test".to_string(),
            authors: vec![],
            description: None,
            timestamp: jiff::Timestamp::now(),
        },
        Vec::new(),
    )?;
    Ok(Recorded {
        change,
        updatables: rec.updatables,
        unprintable: rec.unprintable,
        store,
    })
}

/// The new vertices of a change, in order: flag and bytes. Positions inside
/// the change aren't compared, the text reader lays out contents in its own
/// order.
fn new_vertices(change: &Change) -> Vec<(EdgeFlags, &[u8])> {
    let mut v = Vec::new();
    for hunk in change.hashed.changes.iter() {
        for atom in hunk.iter() {
            if let Atom::NewVertex(n) = atom {
                v.push((n.flag, &change.contents[n.start.us()..n.end.us()]))
            }
        }
    }
    v
}

/// A printable change: nothing flagged, and the text round-trip keeps it.
fn assert_printable(rec: Recorded) -> Result<(), anyhow::Error> {
    assert_eq!(rec.unprintable, Vec::new());
    let mut text = Vec::new();
    rec.change.write(&rec.store, None, true, &mut text)?;
    let mut updatables = rec.updatables.clone();
    let parsed = Change::read(std::io::Cursor::new(&text), &mut updatables)?;
    assert_eq!(parsed.hashed.changes.len(), rec.change.hashed.changes.len());
    assert_eq!(new_vertices(&parsed), new_vertices(&rec.change));
    Ok(())
}

/// An unprintable change: flagged as binary contents too large to print.
fn assert_large_binary(rec: Recorded, expected_len: usize) {
    assert_eq!(
        rec.unprintable,
        vec![Unprintable::LargeBinary {
            path: "file".to_string(),
            len: expected_len
        }]
    );
}

#[test]
fn text_modification_is_printable() -> Result<(), anyhow::Error> {
    assert_printable(record(
        Some(b"a\nb\nc\n".to_vec()),
        b"a\nx\nc\ny\n".to_vec(),
    )?)
}

#[test]
fn text_addition_is_printable() -> Result<(), anyhow::Error> {
    assert_printable(record(None, b"a\nb\nc\n".to_vec())?)
}

#[test]
fn small_binary_modification_is_printable() -> Result<(), anyhow::Error> {
    assert_printable(record(
        Some(binary_contents(3000, 0)),
        binary_contents(3000, 0x5a),
    )?)
}

#[test]
fn small_binary_addition_is_printable() -> Result<(), anyhow::Error> {
    assert_printable(record(None, binary_contents(3000, 0))?)
}

#[test]
fn binary_at_the_limit_is_printable() -> Result<(), anyhow::Error> {
    assert_printable(record(None, binary_contents(MAX_PRINTED_BINARY, 0))?)?;
    assert_printable(record(
        Some(binary_contents(3000, 0)),
        binary_contents(MAX_PRINTED_BINARY, 0x5a),
    )?)
}

#[test]
fn binary_over_the_limit_is_unprintable() -> Result<(), anyhow::Error> {
    let len = MAX_PRINTED_BINARY + 1;
    assert_large_binary(record(None, binary_contents(len, 0))?, len);
    Ok(())
}

#[test]
fn large_binary_modification_is_unprintable() -> Result<(), anyhow::Error> {
    assert_large_binary(
        record(
            Some(binary_contents(13_000, 0)),
            binary_contents(13_000, 0x5a),
        )?,
        13_000,
    );
    Ok(())
}

#[test]
fn large_binary_addition_is_unprintable() -> Result<(), anyhow::Error> {
    assert_large_binary(record(None, binary_contents(13_000, 0))?, 13_000);
    Ok(())
}

/// Reading back a binary insertion whose bytes the text omits fails, instead
/// of building an empty vertex (which is how `pijul record` used to lose the
/// new contents of a modified binary file).
#[test]
fn reading_missing_binary_contents_fails() -> Result<(), anyhow::Error> {
    let rec = record(
        Some(binary_contents(13_000, 0)),
        binary_contents(13_000, 0x5a),
    )?;
    let mut text = Vec::new();
    rec.change.write(&rec.store, None, true, &mut text)?;
    let mut updatables = rec.updatables.clone();
    match Change::read(std::io::Cursor::new(&text), &mut updatables) {
        Err(TextDeError::MissingBinaryContents(path)) => assert_eq!(path, "file"),
        Err(e) => panic!("unexpected error: {:?}", e),
        Ok(_) => panic!("read back a binary insertion without its contents"),
    }
    Ok(())
}
