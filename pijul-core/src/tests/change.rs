use crate::change::*;
use crate::changestore::*;
use crate::pristine::*;
use crate::record::*;
use crate::working_copy::*;
use crate::*;
use std::io::Write;

use super::*;

fn hash_mismatch(change: &mut Change) -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());
    use crate::change::*;
    let mut buf = tempfile::NamedTempFile::new()?;
    let mut h = change.serialize(&mut buf, |_, _| Ok::<_, anyhow::Error>(()))?;
    match h {
        crate::pristine::Hash::Blake3(ref mut h) => h[0] = h[0].wrapping_add(1),
        _ => unreachable!(),
    }
    match Change::deserialize(buf.path().to_str().unwrap(), Some(&h)) {
        Err(ChangeError::ChangeHashMismatch { .. }) => {}
        _ => unreachable!(),
    }

    let f = ChangeFile::open(h, buf.path().to_str().unwrap())?;
    assert_eq!(f.hashed(), &change.hashed);
    Ok(())
}

#[test]
fn hash_mism() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let contents = b"a\nb\nc\nd\ne\nf\n";
    let repo = working_copy::memory::Memory::new();
    let store = changestore::memory::Memory::new();
    repo.add_file("file", contents.to_vec());
    repo.add_file("file2", contents.to_vec());

    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    let mut channel = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;
    txn.write().add_file("file", 0)?;
    txn.write().add_file("file2", 0)?;

    let mut state = Builder::new();
    state
        .record(
            txn.clone(),
            Algorithm::Myers,
            false,
            &crate::DEFAULT_SEPARATOR,
            channel.clone(),
            &repo,
            &store,
            "",
            0,
        )
        .unwrap();
    let rec = state.finish();
    let changes: Vec<_> = rec
        .actions
        .into_iter()
        .map(|rec| rec.globalize(&*txn.read()).unwrap())
        .collect();
    info!("changes = {:?}", changes);
    let mut change0 = crate::change::Change::make_change(
        &*txn.read(),
        &channel,
        changes,
        std::mem::take(&mut *rec.contents.lock()),
        crate::change::ChangeHeader {
            message: "test".to_string(),
            authors: vec![],
            description: None,
            timestamp: jiff::Timestamp::now(),
        },
        Vec::new(),
    )
    .unwrap();
    let hash0 = store.save_change(&mut change0, |_, _| Ok::<_, anyhow::Error>(()))?;
    apply::apply_local_change(
        &mut *txn.write(),
        &mut channel,
        &change0,
        &hash0,
        &rec.updatables,
    )?;

    hash_mismatch(&mut change0)?;

    Ok(())
}

#[cfg(feature = "text-changes")]
#[test]
#[ignore]
fn text() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let contents = b"a\nb\nc\nd\ne\nf\n";
    let repo = working_copy::memory::Memory::new();
    let store = changestore::memory::Memory::new();
    repo.add_file("file", contents.to_vec());
    repo.add_file("file2", contents.to_vec());

    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    let channel = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;
    txn.write().add_file("file", 0)?;
    txn.write().add_file("file2", 0)?;
    let h0 = record_all(&repo, &store, &txn, &channel, "")?;
    let change0 = store.get_change(&h0).unwrap();
    text_test(&store, &change0, h0);

    write!(repo.write_file("file", Inode::ROOT)?, "a\nx\nc\ne\ny\nf\n")?;

    let h1 = record_all(&repo, &store, &txn, &channel, "")?;
    let change1 = store.get_change(&h1).unwrap();
    text_test(&store, &change1, h1);

    repo.remove_path("file2", false)?;
    let h2 = record_all(&repo, &store, &txn, &channel, "")?;
    let change2 = store.get_change(&h2).unwrap();
    text_test(&store, &change2, h2);

    repo.rename("file", "file3")?;
    txn.write().move_file("file", "file3", 0)?;
    let h3 = record_all(&repo, &store, &txn, &channel, "")?;
    let change3 = store.get_change(&h3).unwrap();
    text_test(&store, &change3, h3);

    // name conflicts
    let env2 = pristine::sanakirja::Pristine::new_anon()?;
    let txn2 = env2.arc_txn_begin().unwrap();
    let channel2 = txn2
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;
    let repo2 = working_copy::memory::Memory::new();
    apply::apply_change(&store, &mut *txn2.write(), &mut *channel2.write(), &h0)?;
    apply::apply_change(&store, &mut *txn2.write(), &mut *channel2.write(), &h1)?;
    apply::apply_change(&store, &mut *txn2.write(), &mut *channel2.write(), &h2)?;
    output::output_repository_no_pending(&repo2, &store, &txn2, &channel2, "", true, None, 1, 0)?;
    repo2.rename("file", "file4")?;
    txn2.write().move_file("file", "file4", 0)?;
    record_all(&repo2, &store, &txn2, &channel2, "")?;

    apply::apply_change(&store, &mut *txn2.write(), &mut *channel2.write(), &h3)?;
    output::output_repository_no_pending(&repo2, &store, &txn2, &channel2, "", true, None, 1, 0)?;
    let h = record_all(&repo2, &store, &txn2, &channel2, "")?;
    let solution = store.get_change(&h).unwrap();
    text_test(&store, &solution, h);

    Ok(())
}

fn text_test<C: ChangeStore>(c: &C, change0: &Change, h: Hash) {
    let mut v = Vec::new();
    // let channel = channel.borrow();
    change0.write(c, Some(h), true, &mut v).unwrap();
    for i in std::str::from_utf8(&v).unwrap().lines() {
        debug!("{}", i);
    }
    let change1 = Change::read(std::io::Cursor::new(&v[..]), &mut HashMap::default()).unwrap();
    if change0.header != change1.header {
        error!("header: {:#?} != {:#?}", change0.header, change1.header);
    }
    if change0.dependencies != change1.dependencies {
        error!(
            "deps: {:#?} != {:#?}",
            change0.dependencies, change1.dependencies
        );
    }
    if change0.extra_known != change1.extra_known {
        error!(
            "extra: {:#?} != {:#?}",
            change0.extra_known, change1.extra_known
        );
    }
    if change0.metadata != change1.metadata {
        error!("meta: {:#?} != {:#?}", change0.metadata, change1.metadata);
    }
    if change0.changes != change1.changes {
        if change0.changes.len() != change1.changes.len() {
            trace!("change0.changes = {:#?}", change0.changes);
            trace!("change1.changes = {:#?}", change1.changes);
        } else {
            for (a, b) in change0.changes.iter().zip(change1.changes.iter()) {
                trace!("change0: {:#?}", a);
                trace!("change1: {:#?}", b);
                for (a, b) in a.iter().zip(b.iter()) {
                    if a != b {
                        error!("change0 -> {:#?}", a);
                        error!("change1 -> {:#?}", b);
                    }
                }
            }
        }
    }
    if change0.contents != change1.contents {
        error!("change0.contents = {:?}", change0.contents);
        error!("change1.contents = {:?}", change1.contents);
    }
    assert_eq!(change0, &change1);
}

/// Replacing the unhashed section of a serialized change keeps its hash and
/// contents intact — including when the parsed change would *not* re-encode
/// to the same bytes (here: a chrono-style timestamp with nine fractional
/// digits, which jiff trims on re-serialization), so `save_change` would
/// have moved the change to a new hash.
#[test]
fn replace_unhashed_keeps_hash() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());
    let mut change = Change::default();
    change.hashed.header.message = "hello".to_string();
    change.hashed.header.timestamp = jiff::Timestamp::from_second(1628083743)?;
    change.contents = b"contents".to_vec();
    let mut hasher = crate::pristine::Hasher::default();
    hasher.update(&change.contents);
    change.hashed.contents_hash = hasher.finish();

    // Hand-craft the hashed bytes with the timestamp as older versions of
    // pijul wrote it (bincode: u64 length prefix, then the string).
    let mut hashed = bincode::serialize(&change.hashed)?;
    let canon = change.hashed.header.timestamp.to_string();
    assert_eq!(canon, "2021-08-04T13:29:03Z");
    let noncanon = "2021-08-04T13:29:03.000000000Z";
    let pos = hashed
        .windows(canon.len())
        .position(|w| w == canon.as_bytes())
        .unwrap();
    hashed.splice(pos..pos + canon.len(), noncanon.bytes());
    hashed.splice(pos - 8..pos, (noncanon.len() as u64).to_le_bytes());
    let mut hasher = crate::pristine::Hasher::default();
    hasher.update(&hashed);
    let hash = hasher.finish();
    assert_ne!(hash, change.hash()?);

    // Lay the file out as `serialize` does: offsets, then the three
    // compressed sections.
    let mut hashed_comp = Vec::new();
    compress(&hashed, &mut hashed_comp)?;
    // (an absent unhashed part is still a compressed empty section)
    let mut unhashed_comp = Vec::new();
    compress(&[], &mut unhashed_comp)?;
    let mut contents_comp = Vec::new();
    compress(&change.contents, &mut contents_comp)?;
    let unhashed_off = Change::OFFSETS_SIZE + hashed_comp.len() as u64;
    let contents_off = unhashed_off + unhashed_comp.len() as u64;
    let offsets = Offsets {
        version: VERSION,
        hashed_len: hashed.len() as u64,
        unhashed_off,
        unhashed_len: 0,
        contents_off,
        contents_len: change.contents.len() as u64,
        total: contents_off + contents_comp.len() as u64,
    };
    let mut buf = bincode::serialize(&offsets)?;
    buf.extend_from_slice(&hashed_comp);
    buf.extend_from_slice(&unhashed_comp);
    buf.extend_from_slice(&contents_comp);

    let dir = tempfile::tempdir()?;
    let path = dir.path().join("c.change");
    let path = path.to_str().unwrap();
    std::fs::write(path, &buf)?;
    let back = Change::deserialize(path, Some(&hash))?;
    assert_eq!(back.unhashed, None);
    assert_eq!(back.hashed, change.hashed);
    // The parsed change does not reproduce the file's hash: this is the case
    // an in-place signature must survive.
    assert_ne!(back.hash()?, hash);

    let sig = serde_json::json!({ "signature": "sig" });
    let signed = Change::replace_unhashed_in_buffer(&buf, Some(&sig))?;
    Change::check_from_buffer(&signed, &hash)?;
    std::fs::write(path, &signed)?;
    let back = Change::deserialize(path, Some(&hash))?;
    assert_eq!(back.unhashed, Some(sig));
    assert_eq!(back.hashed, change.hashed);
    assert_eq!(back.contents, change.contents);

    // Removing it again restores the original bytes exactly.
    let unsigned = Change::replace_unhashed_in_buffer(&signed, None)?;
    assert_eq!(unsigned, buf);
    Ok(())
}
