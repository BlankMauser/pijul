use crate::working_copy::WorkingCopyRead;

use super::*;
use std::io::Write;

#[test]
fn solve_order_conflict() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let contents = b"a\nb\n";
    let alice = b"a\nx\ny\nz\nb\n";
    let bob = b"a\nu\nv\nw\nb\n";

    let repo_alice = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();
    repo_alice.add_file("file", contents.to_vec());

    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    let channel_alice = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("alice"))?;
    txn.write().add_file("file", 0)?;
    let init_h = record_all(&repo_alice, &changes, &txn, &channel_alice, "")?;

    // Bob clones
    let repo_bob = working_copy::memory::Memory::new();
    let channel_bob = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("bob"))?;
    apply::apply_change(
        &changes,
        &mut *txn.write(),
        &mut *channel_bob.write(),
        &init_h,
    )?;
    output::output_repository_no_pending(
        &repo_bob,
        &changes,
        &txn,
        &channel_bob,
        "",
        true,
        None,
        1,
        0,
    )?;
    info!("Done outputting Bob's working_copy");
    {
        let mut buf = Vec::new();
        repo_bob.read_file("file", &mut buf).unwrap();
        info!("Bob = {:?}", std::str::from_utf8(&buf));
    }

    // Bob edits and records
    repo_bob
        .write_file("file", Inode::ROOT)
        .unwrap()
        .write_all(bob)
        .unwrap();
    let bob_h = record_all(&repo_bob, &changes, &txn, &channel_bob, "")?;

    // Alice edits and records
    repo_alice
        .write_file("file", Inode::ROOT)
        .unwrap()
        .write_all(alice)
        .unwrap();
    let alice_h = record_all(&repo_alice, &changes, &txn, &channel_alice, "")?;

    // Alice applies Bob's change
    apply::apply_change(
        &changes,
        &mut *txn.write(),
        &mut *channel_alice.write(),
        &bob_h,
    )?;
    output::output_repository_no_pending(
        &repo_alice,
        &changes,
        &txn,
        &channel_alice,
        "",
        true,
        None,
        1,
        0,
    )?;
    let mut buf = Vec::new();
    repo_alice.read_file("file", &mut buf)?;

    let check_conflict = |buf: &[u8]| -> Result<(), anyhow::Error> {
        let re = regex::bytes::Regex::new(r#" \[[^\]]*\]"#).unwrap();
        let buf_ = re.replace_all(&buf, &[][..]);
        let conflict: Vec<_> = std::str::from_utf8(&buf_)?.lines().collect();
        debug!("{:?}", conflict);
        {
            let mut conflict = conflict.clone();
            (&mut conflict[2..9]).sort_unstable();
            assert_eq!(
                conflict,
                vec![
                    "a",
                    ">>>>>>> 1",
                    "======= 1",
                    "u",
                    "v",
                    "w",
                    "x",
                    "y",
                    "z",
                    "<<<<<<< 1",
                    "b"
                ]
            );
        }
        Ok(())
    };

    // Alice solves the conflict.
    let conflict: Vec<_> = std::str::from_utf8(&buf)?.lines().collect();
    {
        let mut w = repo_alice.write_file("file", Inode::ROOT).unwrap();
        for (n, l) in conflict.iter().enumerate() {
            if n == 0 || n == 2 || n == 3 || n == 7 || n == 8 || n == 10 {
                writeln!(w, "{}", l)?
            } else if n == 4 {
                writeln!(w, "{}\nbla!", l)?
            } else if n == 6 {
                writeln!(w, "{}", l)?
            }
        }
    }
    info!("resolving");
    let resolution = record_all(&repo_alice, &changes, &txn, &channel_alice, "")?;

    // Bob applies Alice's change
    apply::apply_change(
        &changes,
        &mut *txn.write(),
        &mut *channel_bob.write(),
        &alice_h,
    )
    .unwrap();
    output::output_repository_no_pending(
        &repo_bob,
        &changes,
        &txn,
        &channel_bob,
        "",
        true,
        None,
        1,
        0,
    )?;
    buf.clear();
    repo_bob.read_file("file", &mut buf)?;
    check_conflict(&buf)?;

    // Bob applies Alice's resolution
    apply::apply_change(
        &changes,
        &mut *txn.write(),
        &mut *channel_bob.write(),
        &resolution,
    )
    .unwrap();
    output::output_repository_no_pending(
        &repo_bob,
        &changes,
        &txn,
        &channel_bob,
        "",
        true,
        None,
        1,
        0,
    )?;
    buf.clear();
    repo_bob.read_file("file", &mut buf)?;
    assert!(std::str::from_utf8(&buf)?.lines().all(|l| l.len() < 10));

    crate::unrecord::unrecord(
        &mut *txn.write(),
        &channel_bob,
        &changes,
        &resolution,
        0,
        &mut Default::default(),
    )
    .unwrap();
    output::output_repository_no_pending(
        &repo_bob,
        &changes,
        &txn,
        &channel_bob,
        "",
        true,
        None,
        1,
        0,
    )?;
    buf.clear();
    repo_bob.read_file("file", &mut buf)?;
    check_conflict(&buf)?;
    Ok(())
}
#[test]
fn order_conflict_simple() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let contents = b"a\nb\n";
    let alice = b"a\nx\nb\n";
    let bob = b"a\ny\nb\n";
    let charlie = b"a\nz\nb\n";

    let repo_alice = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();
    repo_alice.add_file("file", contents.to_vec());

    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    let channel_alice = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("alice"))?;
    txn.write().add_file("file", 0)?;
    let init_h = record_all(&repo_alice, &changes, &txn, &channel_alice, "")?;

    // Bob clones
    let mut repo_bob = working_copy::memory::Memory::new();
    let channel_bob = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("bob"))?;
    apply::apply_change(
        &changes,
        &mut *txn.write(),
        &mut *channel_bob.write(),
        &init_h,
    )?;
    output::output_repository_no_pending(
        &repo_bob,
        &changes,
        &txn,
        &channel_bob,
        "",
        true,
        None,
        1,
        0,
    )?;
    info!("Done outputting Bob's working_copy");
    {
        let mut buf = Vec::new();
        repo_bob.read_file("file", &mut buf).unwrap();
        info!("Bob = {:?}", std::str::from_utf8(&buf));
    }

    // Charlie clones
    let repo_charlie = working_copy::memory::Memory::new();
    let channel_charlie = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("charlie"))?;
    apply::apply_change(
        &changes,
        &mut *txn.write(),
        &mut *channel_charlie.write(),
        &init_h,
    )?;
    output::output_repository_no_pending(
        &repo_charlie,
        &changes,
        &txn,
        &channel_charlie,
        "",
        true,
        None,
        1,
        0,
    )?;

    // Bob edits and records
    repo_bob
        .write_file("file", Inode::ROOT)
        .unwrap()
        .write_all(bob)
        .unwrap();
    let bob_h = record_all(&repo_bob, &changes, &txn, &channel_bob, "")?;

    // Charlie edits and records
    repo_charlie
        .write_file("file", Inode::ROOT)
        .unwrap()
        .write_all(charlie)
        .unwrap();
    let charlie_h = record_all(&repo_charlie, &changes, &txn, &channel_charlie, "")?;

    // Alice edits and records
    repo_alice
        .write_file("file", Inode::ROOT)
        .unwrap()
        .write_all(alice)
        .unwrap();
    let alice_h = record_all(&repo_alice, &changes, &txn, &channel_alice, "")?;

    // Alice applies Bob's change
    apply::apply_change(
        &changes,
        &mut *txn.write(),
        &mut *channel_alice.write(),
        &bob_h,
    )?;
    output::output_repository_no_pending(
        &repo_alice,
        &changes,
        &txn,
        &channel_alice,
        "",
        true,
        None,
        1,
        0,
    )?;
    let mut buf = Vec::new();
    repo_alice.read_file("file", &mut buf)?;

    let check_conflict = |buf: &[u8]| -> Result<(), anyhow::Error> {
        let re = regex::bytes::Regex::new(r#" \[[^\]]*\]"#).unwrap();
        let buf_ = re.replace_all(&buf, &[][..]);

        let conflict: Vec<_> = std::str::from_utf8(&buf_)?.lines().collect();
        debug!("{:?}", conflict);
        {
            let mut conflict = conflict.clone();
            (&mut conflict[2..7]).sort_unstable();
            assert_eq!(
                conflict,
                vec![
                    "a",
                    ">>>>>>> 1",
                    "======= 1",
                    "======= 1",
                    "x",
                    "y",
                    "z",
                    "<<<<<<< 1",
                    "b"
                ]
            );
        }
        Ok(())
    };
    // check_conflict(&buf)?;

    // Alice solves the conflict.
    let conflict: Vec<_> = std::str::from_utf8(&buf)?.lines().collect();
    {
        let mut w = repo_alice.write_file("file", Inode::ROOT).unwrap();
        for l in conflict.iter().filter(|l| l.len() == 1) {
            writeln!(w, "{}", l)?
        }
    }
    let mut alice_resolution = Vec::new();
    repo_alice.read_file("file", &mut alice_resolution)?;
    info!("resolving");
    let resolution = record_all(&repo_alice, &changes, &txn, &channel_alice, "")?;

    // Bob applies Alice's change
    apply::apply_change(
        &changes,
        &mut *txn.write(),
        &mut *channel_bob.write(),
        &alice_h,
    )
    .unwrap();
    apply::apply_change(
        &changes,
        &mut *txn.write(),
        &mut *channel_bob.write(),
        &charlie_h,
    )
    .unwrap();
    output::output_repository_no_pending(
        &repo_bob,
        &changes,
        &txn,
        &channel_bob,
        "",
        true,
        None,
        1,
        0,
    )?;
    buf.clear();
    repo_bob.read_file("file", &mut buf)?;
    check_conflict(&buf)?;

    apply::apply_change(
        &changes,
        &mut *txn.write(),
        &mut *channel_bob.write(),
        &resolution,
    )
    .unwrap();
    output::output_repository_no_pending(
        &repo_bob,
        &changes,
        &txn,
        &channel_bob,
        "",
        true,
        None,
        1,
        0,
    )?;
    buf.clear();
    repo_bob.read_file("file", &mut buf)?;
    {
        let re = regex::bytes::Regex::new(r#" \[[^\]]*\]"#).unwrap();
        let buf_ = re.replace_all(&buf, &[][..]);
        let mut conflict: Vec<_> = std::str::from_utf8(&buf_)?.lines().collect();
        (&mut conflict[2..6]).sort_unstable();
        assert_eq!(
            conflict,
            vec![
                "a",
                ">>>>>>> 1",
                "======= 1",
                "x",
                "y",
                "z",
                "<<<<<<< 1",
                "b"
            ]
        )
    }
    let conflict: Vec<_> = std::str::from_utf8(&buf)?.lines().collect();
    {
        let mut w = repo_bob.write_file("file", Inode::ROOT).unwrap();
        for l in conflict.iter().filter(|l| l.len() == 1) {
            writeln!(w, "{}", l)?
        }
    }
    let mut bob_resolution = Vec::new();
    repo_bob.read_file("file", &mut bob_resolution)?;
    info!("resolving");
    let resolution2 = record_all(&mut repo_bob, &changes, &txn, &channel_bob, "")?;

    // Charlie applies Alice's change
    apply::apply_change_arc(&changes, &txn, &channel_charlie, &alice_h).unwrap();
    apply::apply_change_arc(&changes, &txn, &channel_charlie, &bob_h).unwrap();
    output::output_repository_no_pending(
        &repo_charlie,
        &changes,
        &txn,
        &channel_charlie,
        "",
        true,
        None,
        1,
        0,
    )?;
    buf.clear();
    repo_charlie.read_file("file", &mut buf)?;
    check_conflict(&buf)?;

    apply::apply_change_arc(&changes, &txn, &channel_charlie, &resolution).unwrap();
    apply::apply_change_arc(&changes, &txn, &channel_charlie, &resolution2).unwrap();
    output::output_repository_no_pending(
        &repo_charlie,
        &changes,
        &txn,
        &channel_charlie,
        "",
        true,
        None,
        1,
        0,
    )?;
    buf.clear();
    repo_charlie.read_file("file", &mut buf)?;
    assert_eq!(
        std::str::from_utf8(&bob_resolution),
        std::str::from_utf8(&buf)
    );

    Ok(())
}

#[test]
fn order_conflict_edit() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let contents = b"a\nb\n";
    let alice = b"a\nx\ny\nb\n";
    let bob = b"a\nu\nv\nb\n";

    let mut repo_alice = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();
    repo_alice.add_file("file", contents.to_vec());

    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    let channel_alice = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("alice"))?;
    txn.write().add_file("file", 0)?;
    let init_h = record_all(&repo_alice, &changes, &txn, &channel_alice, "")?;

    // Bob clones
    let repo_bob = working_copy::memory::Memory::new();
    let channel_bob = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("bob"))?;
    apply::apply_change_arc(&changes, &txn, &channel_bob, &init_h)?;
    output::output_repository_no_pending(
        &repo_bob,
        &changes,
        &txn,
        &channel_bob,
        "",
        true,
        None,
        1,
        0,
    )?;
    info!("Done outputting Bob's working_copy");

    // Bob edits and records
    repo_bob
        .write_file("file", Inode::ROOT)
        .unwrap()
        .write_all(bob)
        .unwrap();
    let bob_h = record_all(&repo_bob, &changes, &txn, &channel_bob, "")?;

    // Alice edits and records
    repo_alice
        .write_file("file", Inode::ROOT)
        .unwrap()
        .write_all(alice)
        .unwrap();
    let alice_h = record_all(&repo_alice, &changes, &txn, &channel_alice, "")?;

    // Alice applies Bob's change
    apply::apply_change_arc(&changes, &txn, &channel_alice, &bob_h)?;
    output::output_repository_no_pending(
        &repo_alice,
        &changes,
        &txn,
        &channel_alice,
        "",
        true,
        None,
        1,
        0,
    )?;

    // Alice solves the conflict.
    let mut buf = Vec::new();
    repo_alice.read_file("file", &mut buf)?;
    let conflict: Vec<_> = std::str::from_utf8(&buf)?.lines().collect();
    let mut is_conflict = 0;
    {
        let mut w = repo_alice.write_file("file", Inode::ROOT).unwrap();
        for l in conflict.iter() {
            if l.len() == 1 {
                if is_conflict < 2 {
                    writeln!(w, "{}", l)?
                }
                is_conflict += 1
            } else if l.as_bytes()[0] == b'<' {
                is_conflict = 0
            } else {
                // === or >>>
                is_conflict = 1
            }
        }
    }
    let mut alice_resolution = Vec::new();
    repo_alice.read_file("file", &mut alice_resolution)?;
    info!("resolving {:?}", std::str::from_utf8(&alice_resolution));
    let resolution = record_all(&mut repo_alice, &changes, &txn, &channel_alice, "")?;

    // Bob applies Alice's change
    apply::apply_change_arc(&changes, &txn, &channel_bob, &alice_h).unwrap();
    output::output_repository_no_pending(
        &repo_bob,
        &changes,
        &txn,
        &channel_bob,
        "",
        true,
        None,
        1,
        0,
    )?;
    let mut buf = Vec::new();
    repo_bob.read_file("file", &mut buf)?;

    apply::apply_change_arc(&changes, &txn, &channel_bob, &resolution).unwrap();
    output::output_repository_no_pending(
        &repo_bob,
        &changes,
        &txn,
        &channel_bob,
        "",
        true,
        None,
        1,
        0,
    )?;
    buf.clear();
    repo_bob.read_file("file", &mut buf)?;
    assert_eq!(alice_resolution, buf);
    Ok(())
}

#[test]
fn edit_conflict_sides() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let contents = b"a\nb\nc\n";
    let alice = b"a\nx\nb\nc\n";
    let bob = b"a\ny\nb\nc\n";

    let mut repo = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();
    repo.add_file("file", contents.to_vec());

    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    let channel_alice = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("alice"))?;
    txn.write().add_file("file", 0)?;
    let init_h = record_all(&mut repo, &changes, &txn, &channel_alice, "")?;

    // Bob clones
    let channel_bob = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("bob"))?;
    apply::apply_change_arc(&changes, &txn, &channel_bob, &init_h)?;
    output::output_repository_no_pending(
        &repo,
        &changes,
        &txn,
        &channel_bob,
        "",
        true,
        None,
        1,
        0,
    )?;
    info!("Done outputting Bob's working_copy");

    // Bob edits and records
    repo.write_file("file", Inode::ROOT)
        .unwrap()
        .write_all(bob)
        .unwrap();
    let bob_h = record_all(&repo, &changes, &txn, &channel_bob, "")?;

    // Alice edits and records
    repo.write_file("file", Inode::ROOT)
        .unwrap()
        .write_all(alice)
        .unwrap();
    let alice_h = record_all(&repo, &changes, &txn, &channel_alice, "")?;

    // Alice applies Bob's change
    apply::apply_change_arc(&changes, &txn, &channel_alice, &bob_h)?;
    output::output_repository_no_pending(
        &repo,
        &changes,
        &txn,
        &channel_alice,
        "",
        true,
        None,
        1,
        0,
    )?;
    let mut buf = Vec::new();
    repo.read_file("file", &mut buf)?;

    // Alice edits sides of the conflict.
    let conflict: Vec<_> = std::str::from_utf8(&buf)?.lines().collect();
    {
        let mut w = repo.write_file("file", Inode::ROOT).unwrap();
        let mut ended = false;
        let mut n = 0;
        for l in conflict.iter() {
            debug!("line: {:?}", l);
            if l.len() > 5 {
                if l.as_bytes()[0] == b'<' {
                    ended = true
                }
                if true {
                    writeln!(w, "pre{}\n{}\npost{}", n, l, n)?;
                } else {
                    writeln!(w, "{}", l)?;
                }
                n += 1
            } else if !ended {
                writeln!(w, "{}", l)?
            } else {
                debug!("writing c: {:?}", l);
                writeln!(w, "c")?
            }
        }
    }
    let mut buf = Vec::new();
    repo.read_file("file", &mut buf)?;
    info!("resolving");
    let resolution = record_all(&repo, &changes, &txn, &channel_alice, "")?;
    output::output_repository_no_pending(
        &repo,
        &changes,
        &txn,
        &channel_alice,
        "",
        true,
        None,
        1,
        0,
    )?;
    let mut buf2 = Vec::new();
    repo.read_file("file", &mut buf2)?;
    info!("{:?}", std::str::from_utf8(&buf2).unwrap());
    let re = regex::bytes::Regex::new(r#"\[[^\]]*\]"#).unwrap();
    let buf_ = re.replace_all(&buf, &[][..]);
    let buf2_ = re.replace_all(&buf2, &[][..]);

    assert_eq!(std::str::from_utf8(&buf_), std::str::from_utf8(&buf2_));

    // Bob applies
    apply::apply_change_arc(&changes, &txn, &channel_bob, &alice_h)?;
    apply::apply_change_arc(&changes, &txn, &channel_bob, &resolution)?;
    output::output_repository_no_pending(
        &repo,
        &changes,
        &txn,
        &channel_bob,
        "",
        true,
        None,
        1,
        0,
    )?;
    let mut buf3 = Vec::new();
    repo.read_file("file", &mut buf3)?;
    let mut lines2: Vec<_> = std::str::from_utf8(&buf2).unwrap().lines().collect();
    lines2.sort_unstable();
    let mut lines3: Vec<_> = std::str::from_utf8(&buf3).unwrap().lines().collect();
    lines3.sort_unstable();
    assert_eq!(lines2, lines3);
    Ok(())
}

#[test]
fn edit_after_conflict() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let contents = b"a\nb\nc\n";
    let alice = b"a\nx\ny\nb\nc\n";
    let bob = b"a\nx\ny\nb\nc\n";

    let repo = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();
    repo.add_file("file", contents.to_vec());

    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    let channel_alice = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("alice"))?;
    txn.write().add_file("file", 0)?;
    let init_h = record_all(&repo, &changes, &txn, &channel_alice, "")?;

    // Bob clones
    let channel_bob = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("bob"))?;
    apply::apply_change_arc(&changes, &txn, &channel_bob, &init_h)?;
    output::output_repository_no_pending(
        &repo,
        &changes,
        &txn,
        &channel_bob,
        "",
        true,
        None,
        1,
        0,
    )?;
    info!("Done outputting Bob's working_copy");

    // Bob inserts xy between a and b
    repo.write_file("file", Inode::ROOT)
        .unwrap()
        .write_all(bob)
        .unwrap();
    let bob_h = record_all(&repo, &changes, &txn, &channel_bob, "")?;

    // Alice inserts xy between a and b (same as Bob)
    repo.write_file("file", Inode::ROOT)
        .unwrap()
        .write_all(alice)
        .unwrap();
    let alice_h = record_all(&repo, &changes, &txn, &channel_alice, "")?;

    crate::pristine::debug(
        &*txn.read(),
        &*channel_alice.read(),
        std::fs::File::create("alice0.dot").unwrap(),
    )?;

    // Alice applies Bob's change
    apply::apply_change_arc(&changes, &txn, &channel_alice, &bob_h)?;
    crate::pristine::debug(
        &*txn.read(),
        &*channel_alice.read(),
        std::fs::File::create("alice1.dot").unwrap(),
    )?;
    output::output_repository_no_pending(
        &repo,
        &changes,
        &txn,
        &channel_alice,
        "",
        true,
        None,
        1,
        0,
    )?;
    let mut buf = Vec::new();
    repo.read_file("file", &mut buf)?;

    // Alice edits sides of the conflict.
    let conflict: Vec<_> = std::str::from_utf8(&buf)?.lines().collect();
    {
        // Delete b and x, insert pre/post before/after conflict
        // markers (except the closing marker).
        let mut w = repo.write_file("file", Inode::ROOT).unwrap();
        for l in conflict.iter() {
            debug!("line: {:?}", l);
            if l.len() > 5 && l.as_bytes()[0] != b'<' {
                writeln!(w, "pre\n{}\npost", l)?;
            } else if *l != "b" && *l != "x" {
                writeln!(w, "{}", l)?
            }
        }
    }
    let mut buf = Vec::new();
    repo.read_file("file", &mut buf)?;
    debug!("{}", std::str::from_utf8(&buf).unwrap());

    info!("resolving");
    let resolution = record_all(&repo, &changes, &txn, &channel_alice, "")?;

    crate::pristine::debug(
        &*txn.read(),
        &*channel_alice.read(),
        std::fs::File::create("alice2.dot").unwrap(),
    )?;
    crate::pristine::debug(
        &*txn.read(),
        &*channel_bob.read(),
        std::fs::File::create("bob.dot").unwrap(),
    )?;

    output::output_repository_no_pending(
        &repo,
        &changes,
        &txn,
        &channel_alice,
        "",
        true,
        None,
        1,
        0,
    )?;
    let mut buf2 = Vec::new();
    repo.read_file("file", &mut buf2)?;
    info!("{:?}", std::str::from_utf8(&buf2).unwrap());
    let re = regex::bytes::Regex::new(r#"\[[^\]]*\]"#).unwrap();
    let buf_ = re.replace_all(&buf, &[][..]);
    let buf2_ = re.replace_all(&buf2, &[][..]);
    assert_eq!(std::str::from_utf8(&buf_), std::str::from_utf8(&buf2_));

    apply::apply_change_arc(&changes, &txn, &channel_bob, &alice_h)?;
    apply::apply_change_arc(&changes, &txn, &channel_bob, &resolution)?;
    output::output_repository_no_pending(
        &repo,
        &changes,
        &txn,
        &channel_bob,
        "",
        true,
        None,
        1,
        0,
    )?;

    let mut buf3 = Vec::new();
    repo.read_file("file", &mut buf3)?;

    let mut lines2: Vec<_> = std::str::from_utf8(&buf2)?.lines().collect();
    lines2.sort_unstable();
    let mut lines3: Vec<_> = std::str::from_utf8(&buf3)?.lines().collect();
    lines3.sort_unstable();
    assert_eq!(lines2, lines3);
    Ok(())
}

#[test]
fn delete_before_marker() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let contents = b"a\nb\nc\n";
    let alice0 = b"a\nx\ny\nb\nc\n";
    let alice1 = b"a\nx\ny\nz\nb\nc\n";
    let bob0 = b"a\nu\nv\nb\nc\n";
    let bob1 = b"a\nu\nv\nw\nb\nc\n";

    let mut repo = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();
    repo.add_file("file", contents.to_vec());

    let env = pristine::sanakirja::Pristine::new_anon()?;
    let mut txn = env.arc_txn_begin().unwrap();
    let channel_alice = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("alice"))?;
    txn.write().add_file("file", 0)?;
    let init_h = record_all(&repo, &changes, &txn, &channel_alice, "")?;

    // Bob clones
    let mut channel_bob = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("bob"))?;
    apply::apply_change_arc(&changes, &txn, &channel_bob, &init_h)?;
    output::output_repository_no_pending(
        &repo,
        &changes,
        &txn,
        &channel_bob,
        "",
        true,
        None,
        1,
        0,
    )?;
    info!("Done outputting Bob's working_copy");

    // Bob edits and records
    let bob_edits: &[&[u8]] = &[bob0, bob1];
    let bob_changes: Vec<_> = bob_edits
        .iter()
        .map(|bob| {
            repo.write_file("file", Inode::ROOT)
                .unwrap()
                .write_all(bob)
                .unwrap();
            record_all(&repo, &changes, &txn, &channel_bob, "").unwrap()
        })
        .collect();

    // Alice edits and records
    let alice_edits: &[&[u8]] = &[alice0, alice1];
    let alice_changes: Vec<_> = alice_edits
        .iter()
        .map(|alice| {
            repo.write_file("file", Inode::ROOT)
                .unwrap()
                .write_all(alice)
                .unwrap();
            record_all(&repo, &changes, &txn, &channel_alice, "").unwrap()
        })
        .collect();

    // Alice applies Bob's changes
    for bob_h in bob_changes.iter() {
        apply::apply_change_arc(&changes, &txn, &channel_alice, bob_h)?;
    }
    output::output_repository_no_pending(
        &repo,
        &changes,
        &txn,
        &channel_alice,
        "",
        true,
        None,
        1,
        0,
    )?;
    let mut buf = Vec::new();
    repo.read_file("file", &mut buf)?;

    // Alice edits sides of the conflict.
    let conflict: Vec<_> = std::str::from_utf8(&buf)?.lines().collect();
    {
        let mut w = repo.write_file("file", Inode::ROOT).unwrap();
        let mut ended = false;
        for l in conflict.iter() {
            debug!("line: {:?}", l);
            if *l == "z" || *l == "w" {
            } else if l.starts_with("<<<") {
                writeln!(w, "{}", l)?;
                ended = true
            } else if ended {
                writeln!(w, "end\n{}", l)?;
                ended = false
            } else {
                writeln!(w, "{}", l)?;
            }
        }
    }
    let mut buf = Vec::new();
    repo.read_file("file", &mut buf)?;

    info!("resolving");
    let conflict_edits = record_all(&repo, &changes, &txn, &channel_alice, "")?;
    output::output_repository_no_pending(
        &repo,
        &changes,
        &txn,
        &channel_alice,
        "",
        true,
        None,
        1,
        0,
    )?;
    let mut buf2 = Vec::new();
    repo.read_file("file", &mut buf2)?;
    info!("{:?}", std::str::from_utf8(&buf2).unwrap());
    assert_eq!(std::str::from_utf8(&buf), std::str::from_utf8(&buf2));

    // Bob pulls
    for alice_h in alice_changes.iter() {
        apply::apply_change_arc(&changes, &txn, &channel_bob, &*alice_h)?;
    }
    apply::apply_change_arc(&changes, &txn, &channel_bob, &conflict_edits)?;
    output::output_repository_no_pending(
        &mut repo,
        &changes,
        &mut txn,
        &mut channel_bob,
        "",
        true,
        None,
        1,
        0,
    )?;
    buf2.clear();
    repo.read_file("file", &mut buf2)?;
    let mut lines: Vec<_> = std::str::from_utf8(&buf).unwrap().lines().collect();
    lines.sort_unstable();
    let mut lines2: Vec<_> = std::str::from_utf8(&buf2).unwrap().lines().collect();
    lines2.sort_unstable();
    assert_eq!(&lines[3..], &lines2[3..]);

    Ok(())
}

#[test]
fn conflict_last_line() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let contents = b"a\n";
    let alice = b"a\nx";
    let bob = b"a\ny";

    let mut repo_alice = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();
    repo_alice.add_file("file", contents.to_vec());

    let env = pristine::sanakirja::Pristine::new_anon()?;
    let mut txn = env.arc_txn_begin().unwrap();
    let mut channel_alice = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("alice"))?;
    txn.write().add_file("file", 0)?;
    let init_h = record_all(&repo_alice, &changes, &txn, &channel_alice, "")?;

    // Bob clones
    let repo_bob = working_copy::memory::Memory::new();
    let channel_bob = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("bob"))?;
    apply::apply_change_arc(&changes, &txn, &channel_bob, &init_h)?;
    output::output_repository_no_pending(
        &repo_bob,
        &changes,
        &txn,
        &channel_bob,
        "",
        true,
        None,
        1,
        0,
    )?;
    info!("Done outputting Bob's working_copy");
    {
        let mut buf = Vec::new();
        repo_bob.read_file("file", &mut buf).unwrap();
        info!("Bob = {:?}", std::str::from_utf8(&buf));
    }
    // Bob edits and records
    repo_bob
        .write_file("file", Inode::ROOT)
        .unwrap()
        .write_all(bob)
        .unwrap();
    let bob_h = record_all(&repo_bob, &changes, &txn, &channel_bob, "")?;

    // Alice edits and records
    repo_alice
        .write_file("file", Inode::ROOT)
        .unwrap()
        .write_all(alice)
        .unwrap();
    let alice_h = record_all(&repo_alice, &changes, &txn, &channel_alice, "")?;

    // Alice applies Bob's change
    apply::apply_change_arc(&changes, &txn, &channel_alice, &bob_h)?;
    output::output_repository_no_pending(
        &repo_alice,
        &changes,
        &txn,
        &channel_alice,
        "",
        true,
        None,
        1,
        0,
    )?;
    let mut buf = Vec::new();
    repo_alice.read_file("file", &mut buf)?;

    let check_conflict = |buf: &[u8]| -> Result<(), anyhow::Error> {
        let conflict: Vec<_> = std::str::from_utf8(buf)?.lines().collect();
        debug!("{:?}", conflict);
        {
            let mut conflict = conflict.clone();
            conflict.sort_unstable();
            assert_eq!(&conflict[3..], ["a", "x", "y",]);
            assert_eq!(conflict[0], "<<<<<<< 1",);
        }
        Ok(())
    };

    info!("resolving {:?}", std::time::SystemTime::now());

    // Alice solves the conflict.
    let conflict: Vec<_> = std::str::from_utf8(&buf)?.lines().collect();
    {
        let mut w = repo_alice.write_file("file", Inode::ROOT).unwrap();
        for l in conflict.iter().filter(|l| l.len() <= 2) {
            writeln!(w, "{}", l)?
        }
    }
    let mut buf_alice = Vec::new();
    repo_alice.read_file("file", &mut buf_alice)?;

    let resolution = record_all(&mut repo_alice, &changes, &mut txn, &mut channel_alice, "")?;

    // Bob applies Alice's change
    apply::apply_change_arc(&changes, &txn, &channel_bob, &alice_h).unwrap();
    output::output_repository_no_pending(
        &repo_bob,
        &changes,
        &txn,
        &channel_bob,
        "",
        true,
        None,
        1,
        0,
    )?;
    buf.clear();
    repo_bob.read_file("file", &mut buf)?;
    check_conflict(&buf)?;

    apply::apply_change_arc(&changes, &txn, &channel_bob, &resolution).unwrap();
    output::output_repository_no_pending(
        &repo_bob,
        &changes,
        &txn,
        &channel_bob,
        "",
        true,
        None,
        1,
        0,
    )?;
    buf.clear();
    repo_bob.read_file("file", &mut buf)?;

    crate::pristine::debug(
        &*txn.read(),
        &*channel_alice.read(),
        std::fs::File::create("alice.dot").unwrap(),
    )?;
    crate::pristine::debug(
        &*txn.read(),
        &*channel_bob.read(),
        std::fs::File::create("bob.dot").unwrap(),
    )?;

    assert_eq!(std::str::from_utf8(&buf), std::str::from_utf8(&buf_alice));
    Ok(())
}

#[test]
fn zombie_last_line() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let contents = b"a\nb";
    let alice = b"a\nx";
    let bob = b"";

    let repo_alice = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();
    repo_alice.add_file("file", contents.to_vec());

    let env = pristine::sanakirja::Pristine::new_anon()?;
    let mut txn = env.arc_txn_begin().unwrap();
    let channel_alice = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("alice"))?;
    txn.write().add_file("file", 0)?;
    let init_h = record_all(&repo_alice, &changes, &txn, &channel_alice, "")?;

    // Bob clones
    let mut repo_bob = working_copy::memory::Memory::new();
    let mut channel_bob = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("bob"))?;
    apply::apply_change_arc(&changes, &txn, &channel_bob, &init_h)?;
    output::output_repository_no_pending(
        &repo_bob,
        &changes,
        &txn,
        &channel_bob,
        "",
        true,
        None,
        1,
        0,
    )?;
    info!("Done outputting Bob's working_copy");
    {
        let mut buf = Vec::new();
        repo_bob.read_file("file", &mut buf).unwrap();
        info!("Bob = {:?}", std::str::from_utf8(&buf));
    }
    // Bob edits and records
    repo_bob
        .write_file("file", Inode::ROOT)
        .unwrap()
        .write_all(bob)
        .unwrap();
    let bob_h = record_all(&repo_bob, &changes, &txn, &channel_bob, "")?;

    // Alice edits and records
    repo_alice
        .write_file("file", Inode::ROOT)
        .unwrap()
        .write_all(alice)
        .unwrap();
    let alice_h = record_all(&repo_alice, &changes, &txn, &channel_alice, "")?;

    // Alice applies Bob's change
    debug!("apply Bob's change");
    apply::apply_change_arc(&changes, &txn, &channel_alice, &bob_h)?;
    crate::pristine::debug(
        &*txn.read(),
        &*channel_alice.read(),
        std::fs::File::create("alice.dot").unwrap(),
    )?;
    output::output_repository_no_pending(
        &repo_alice,
        &changes,
        &txn,
        &channel_alice,
        "",
        true,
        None,
        1,
        0,
    )?;
    let mut buf = Vec::new();
    repo_alice.read_file("file", &mut buf)?;

    let check_conflict = |buf: &[u8]| -> Result<(), anyhow::Error> {
        let re = regex::bytes::Regex::new(r#" \[[^\]]*\]"#).unwrap();
        let buf_ = re.replace_all(&buf, &[][..]);
        let conflict: Vec<_> = std::str::from_utf8(&buf_)?.lines().collect();
        assert_eq!(conflict, vec![">>>>>>> 1", "x", "<<<<<<< 1"]);
        Ok(())
    };
    check_conflict(&buf)?;

    {
        let mut state = Builder::new();
        state
            .record(
                txn.clone(),
                Algorithm::default(),
                false,
                &crate::DEFAULT_SEPARATOR,
                channel_alice.clone(),
                &repo_alice,
                &changes,
                "",
                1,
            )
            .unwrap();
        let rec = state.finish();
        assert!(rec.actions.is_empty())
    }

    // Alice solves the conflict.
    repo_alice
        .write_file("file", Inode::ROOT)
        .unwrap()
        .write_all(b"x")?;
    info!("resolving");
    let mut buf_alice = Vec::new();
    repo_alice.read_file("file", &mut buf_alice)?;

    let resolution = record_all(&repo_alice, &changes, &txn, &channel_alice, "")?;

    crate::pristine::debug(
        &*txn.read(),
        &*channel_alice.read(),
        std::fs::File::create("alice.dot").unwrap(),
    )?;

    // Bob applies Alice's change
    apply::apply_change_arc(&changes, &txn, &channel_bob, &alice_h).unwrap();
    output::output_repository_no_pending(
        &mut repo_bob,
        &changes,
        &mut txn,
        &mut channel_bob,
        "",
        true,
        None,
        1,
        0,
    )?;
    buf.clear();
    repo_bob.read_file("file", &mut buf)?;
    check_conflict(&buf)?;

    apply::apply_change_arc(&changes, &txn, &channel_bob, &resolution).unwrap();

    crate::pristine::debug(
        &*txn.read(),
        &*channel_bob.read(),
        std::fs::File::create("bob.dot").unwrap(),
    )?;

    output::output_repository_no_pending(
        &mut repo_bob,
        &changes,
        &mut txn,
        &mut channel_bob,
        "",
        true,
        None,
        1,
        0,
    )?;
    buf.clear();
    repo_bob.read_file("file", &mut buf)?;
    assert_eq!(std::str::from_utf8(&buf), std::str::from_utf8(&buf_alice));
    Ok(())
}

#[test]
fn edit_post_conflict() -> Result<(), anyhow::Error> {
    edit_post_conflict_(
        |buf| {
            let re = regex::bytes::Regex::new(r#" \[[^\]]*\]"#).unwrap();
            let buf_ = re.replace_all(&buf, &[][..]);
            let buf: Vec<_> = std::str::from_utf8(&buf_).unwrap().lines().collect();
            assert!(
                buf == [
                    "a",
                    ">>>>>>> 1",
                    "0",
                    "1",
                    "2",
                    "======= 1",
                    "3",
                    "4",
                    "5",
                    "<<<<<<< 1",
                    "b",
                ] || buf
                    == [
                        "a",
                        ">>>>>>> 1",
                        "3",
                        "4",
                        "5",
                        "======= 1",
                        "0",
                        "1",
                        "2",
                        "<<<<<<< 1",
                        "b",
                    ]
            )
        },
        |buf, w| {
            let conflict: Vec<_> = std::str::from_utf8(&buf).unwrap().lines().collect();
            for l in conflict.iter() {
                if *l == "a" {
                    writeln!(w, "a\na'")?
                } else if l.len() == 1 && *l != "0" && *l != "3" {
                    writeln!(w, "{}", l)?
                }
            }
            Ok(())
        },
    )
}

#[test]
fn edit_around_conflict() -> Result<(), anyhow::Error> {
    edit_post_conflict_(
        |buf| {
            let re = regex::bytes::Regex::new(r#" \[[^\]]*\]"#).unwrap();
            let buf_ = re.replace_all(&buf, &[][..]);
            let buf: Vec<_> = std::str::from_utf8(&buf_).unwrap().lines().collect();
            assert!(
                buf == [
                    "a",
                    ">>>>>>> 1",
                    "0",
                    "1",
                    "2",
                    "======= 1",
                    "3",
                    "4",
                    "5",
                    "<<<<<<< 1",
                    "b",
                ] || buf
                    == [
                        "a",
                        ">>>>>>> 1",
                        "3",
                        "4",
                        "5",
                        "======= 1",
                        "0",
                        "1",
                        "2",
                        "<<<<<<< 1",
                        "b",
                    ]
            )
        },
        |buf, w| {
            let conflict: Vec<_> = std::str::from_utf8(&buf).unwrap().lines().collect();
            for l in conflict.iter() {
                if *l == "a" {
                    writeln!(w, "a\na'")?
                } else if *l == "b" {
                    writeln!(w, "c")?
                } else {
                    writeln!(w, "{}", l)?
                }
            }
            Ok(())
        },
    )
}

fn edit_post_conflict_<
    Check: FnMut(&[u8]),
    Resolve: FnOnce(&[u8], &mut dyn std::io::Write) -> Result<(), std::io::Error>,
>(
    mut check: Check,
    resolve: Resolve,
) -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let contents = b"a\nb\n";
    let alice = b"a\n0\n1\n2\nb\n";
    let bob = b"a\n3\n4\n5\nb\n";

    let repo_alice = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();
    repo_alice.add_file("file", contents.to_vec());

    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    let channel_alice = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("alice"))?;
    txn.write().add_file("file", 0)?;
    let init_h = record_all(&repo_alice, &changes, &txn, &channel_alice, "")?;

    // Bob clones
    let repo_bob = working_copy::memory::Memory::new();
    let channel_bob = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("bob"))?;
    apply::apply_change_arc(&changes, &txn, &channel_bob, &init_h)?;
    output::output_repository_no_pending(
        &repo_bob,
        &changes,
        &txn,
        &channel_bob,
        "",
        true,
        None,
        1,
        0,
    )?;
    info!("Done outputting Bob's working_copy");
    {
        let mut buf = Vec::new();
        repo_bob.read_file("file", &mut buf).unwrap();
        info!("Bob = {:?}", std::str::from_utf8(&buf));
    }
    // Bob edits and records
    repo_bob
        .write_file("file", Inode::ROOT)
        .unwrap()
        .write_all(bob)
        .unwrap();
    let bob_h = record_all(&repo_bob, &changes, &txn, &channel_bob, "")?;

    // Alice edits and records
    repo_alice
        .write_file("file", Inode::ROOT)
        .unwrap()
        .write_all(alice)
        .unwrap();
    let alice_h = record_all(&repo_alice, &changes, &txn, &channel_alice, "")?;

    // Alice applies Bob's change
    apply::apply_change_arc(&changes, &txn, &channel_alice, &bob_h)?;
    output::output_repository_no_pending(
        &repo_alice,
        &changes,
        &txn,
        &channel_alice,
        "",
        true,
        None,
        1,
        0,
    )?;
    let mut buf = Vec::new();
    repo_alice.read_file("file", &mut buf)?;

    check(&buf);

    // Alice solves the conflict.
    {
        let mut w = repo_alice.write_file("file", Inode::ROOT).unwrap();
        resolve(&buf, &mut w)?;
    }
    info!("resolving");
    let resolution = record_all(&repo_alice, &changes, &txn, &channel_alice, "")?;

    // Bob applies Alice's change
    apply::apply_change_arc(&changes, &txn, &channel_bob, &alice_h).unwrap();
    output::output_repository_no_pending(
        &repo_bob,
        &changes,
        &txn,
        &channel_bob,
        "",
        true,
        None,
        1,
        0,
    )?;
    buf.clear();
    repo_bob.read_file("file", &mut buf)?;
    check(&buf);

    // Bob applies Alice's solution.
    apply::apply_change_arc(&changes, &txn, &channel_bob, &resolution).unwrap();
    output::output_repository_no_pending(
        &repo_bob,
        &changes,
        &txn,
        &channel_bob,
        "",
        true,
        None,
        1,
        0,
    )?;

    let mut buf2 = Vec::new();
    repo_bob.read_file("file", &mut buf2)?;
    buf.clear();
    repo_alice.read_file("file", &mut buf)?;

    let re = regex::bytes::Regex::new(r#"\[[^\]]*\]"#).unwrap();
    let buf_ = re.replace_all(&buf, &[][..]);
    let buf2_ = re.replace_all(&buf2, &[][..]);

    let mut lines: Vec<_> = std::str::from_utf8(&buf_).unwrap().lines().collect();
    lines.sort_unstable();
    let mut lines2: Vec<_> = std::str::from_utf8(&buf2_).unwrap().lines().collect();
    lines2.sort_unstable();
    assert_eq!(lines, lines2);

    Ok(())
}

#[test]
fn nested_conflict() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let contents = b"a\nb\n";
    let alice = b"a\nx\nb\n";
    let bob = b"a\ny\nb\n";

    let repo_alice = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();
    repo_alice.add_file("file", contents.to_vec());

    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    let channel_alice = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("alice"))?;
    txn.write().add_file("file", 0)?;
    let init_h = record_all(&repo_alice, &changes, &txn, &channel_alice, "")?;

    // Bob clones
    let repo_bob = working_copy::memory::Memory::new();
    let channel_bob = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("bob"))?;
    apply::apply_change_arc(&changes, &txn, &channel_bob, &init_h)?;
    output::output_repository_no_pending(
        &repo_bob,
        &changes,
        &txn,
        &channel_bob,
        "",
        true,
        None,
        1,
        0,
    )?;
    info!("Done outputting Bob's working_copy");
    {
        let mut buf = Vec::new();
        repo_bob.read_file("file", &mut buf).unwrap();
        info!("Bob = {:?}", std::str::from_utf8(&buf));
    }

    // Bob edits and records
    repo_bob
        .write_file("file", Inode::ROOT)
        .unwrap()
        .write_all(bob)
        .unwrap();
    let bob_h = record_all(&repo_bob, &changes, &txn, &channel_bob, "")?;

    // Alice edits and records
    debug!("Alice records");
    repo_alice
        .write_file("file", Inode::ROOT)
        .unwrap()
        .write_all(alice)
        .unwrap();
    let alice_h = record_all(&repo_alice, &changes, &txn, &channel_alice, "")?;

    // Alice applies Bob's change
    debug!("Alice applies");
    apply::apply_change_arc(&changes, &txn, &channel_alice, &bob_h)?;
    output::output_repository_no_pending(
        &repo_alice,
        &changes,
        &txn,
        &channel_alice,
        "",
        true,
        None,
        1,
        0,
    )?;

    // Alice solves the conflict.
    let mut buf = Vec::new();
    repo_alice.read_file("file", &mut buf)?;
    {
        let mut w = repo_alice.write_file("file", Inode::ROOT).unwrap();
        let buf = std::str::from_utf8(&buf).unwrap();
        w.write_all(buf.replace("x\n", "u\nx\n").as_bytes())?;
    }
    info!("resolving");
    let resolution_alice = record_all(&repo_alice, &changes, &txn, &channel_alice, "")?;

    // Bob applies Alice's change
    apply::apply_change_arc(&changes, &txn, &channel_bob, &alice_h).unwrap();
    output::output_repository_no_pending(
        &repo_bob,
        &changes,
        &txn,
        &channel_bob,
        "",
        true,
        None,
        1,
        0,
    )?;

    // Bob resolves.
    buf.clear();
    repo_bob.read_file("file", &mut buf)?;
    {
        let mut w = repo_bob.write_file("file", Inode::ROOT).unwrap();
        let buf = std::str::from_utf8(&buf).unwrap();
        w.write_all(buf.replace("x\n", "i\nx\n").as_bytes())?;
    }
    info!("resolving");
    let resolution_bob = record_all(&repo_bob, &changes, &txn, &channel_bob, "")?;

    buf.clear();
    repo_bob.read_file("file", &mut buf)?;

    // Alice applies Bob's resolution.
    apply::apply_change_arc(&changes, &txn, &channel_alice, &resolution_bob).unwrap();
    output::output_repository_no_pending(
        &repo_alice,
        &changes,
        &txn,
        &channel_alice,
        "",
        true,
        None,
        1,
        0,
    )?;
    buf.clear();
    repo_alice.read_file("file", &mut buf)?;
    debug!("{}", std::str::from_utf8(&buf).unwrap());

    // Bob applies Alice's resolution
    apply::apply_change_arc(&changes, &txn, &channel_bob, &resolution_alice).unwrap();
    output::output_repository_no_pending(
        &repo_bob,
        &changes,
        &txn,
        &channel_bob,
        "",
        true,
        None,
        1,
        0,
    )?;
    let mut buf2 = Vec::new();
    repo_bob.read_file("file", &mut buf2)?;

    let re = regex::bytes::Regex::new(r#" \[[^\]]*\]"#).unwrap();
    let buf_ = re.replace_all(&buf, &[][..]);
    let buf2_ = re.replace_all(&buf2, &[][..]);

    let mut lines: Vec<_> = std::str::from_utf8(&buf_).unwrap().lines().collect();
    lines.sort_unstable();
    let mut lines2: Vec<_> = std::str::from_utf8(&buf2_).unwrap().lines().collect();
    lines2.sort_unstable();
    assert_eq!(lines, lines2);

    Ok(())
}
#[test]
fn zombie_context_resolution() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let repo_alice = working_copy::memory::Memory::new();
    let repo_bob = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();

    let env_alice = pristine::sanakirja::Pristine::new_anon()?;
    let txn_alice = env_alice.arc_txn_begin().unwrap();
    let env_bob = pristine::sanakirja::Pristine::new_anon()?;
    let txn_bob = env_bob.arc_txn_begin().unwrap();

    let channel_alice = txn_alice
        .write()
        .open_or_create_channel(&SmallString::from_str("alice"))
        .unwrap();

    // Alice records
    txn_alice.write().add_file("file", 0).unwrap();
    repo_alice.add_file("file", b"".to_vec());
    let x: &[&[u8]] = &[b"c\n", b"a\nc\n", b"a\nb\nc\n", b"a\n", b""];
    let p_alice: Vec<_> = x
        .iter()
        .map(|c| {
            repo_alice
                .write_file("file", Inode::ROOT)
                .unwrap()
                .write_all(c)
                .unwrap();
            record_all(&repo_alice, &changes, &txn_alice, &channel_alice, "").unwrap()
        })
        .collect();

    // Bob clones
    let channel_bob = txn_bob
        .write()
        .open_or_create_channel(&SmallString::from_str("bob"))
        .unwrap();
    apply::apply_change_arc(&changes, &txn_bob, &channel_bob, &p_alice[0]).unwrap();
    output::output_repository_no_pending(
        &repo_bob,
        &changes,
        &txn_bob,
        &channel_bob,
        "",
        true,
        None,
        1,
        0,
    )?;

    // Bob creates an order conflict just to keep line "c" connected
    // to the root.
    repo_bob
        .write_file("file", Inode::ROOT)
        .unwrap()
        .write_all(b"x\nc\n")?;
    debug!("bob records conflict");
    let p_bob = record_all(&repo_bob, &changes, &txn_bob, &channel_bob, "").unwrap();

    // Bob applies all of Alice's other changes
    for (n, p) in (&p_alice[1..]).iter().enumerate() {
        info!("{}. Applying {:?}", n, p);
        apply::apply_change_arc(&changes, &txn_bob, &channel_bob, p).unwrap();
        // if n == 2 {
        //     panic!("n")
        // }
    }
    output::output_repository_no_pending(
        &repo_bob,
        &changes,
        &txn_bob,
        &channel_bob,
        "",
        true,
        None,
        1,
        0,
    )?;
    let mut buf = Vec::new();
    repo_bob.read_file("file", &mut buf)?;
    debug!("file = {:?}", std::str::from_utf8(&buf));
    let re = regex::bytes::Regex::new(r#" \[[^\]]*\]"#).unwrap();
    let buf_ = re.replace_all(&buf, &[][..]);

    assert_eq!(std::str::from_utf8(&buf_), Ok(">>>>>>> 1\nx\n<<<<<<< 1\n"));

    repo_bob
        .write_file("file", Inode::ROOT)
        .unwrap()
        .write_all(b"x\nc\n")?;
    let resolution = record_all(&repo_bob, &changes, &txn_bob, &channel_bob, "").unwrap();
    output::output_repository_no_pending(
        &repo_bob,
        &changes,
        &txn_bob,
        &channel_bob,
        "",
        true,
        None,
        1,
        0,
    )?;
    buf.clear();
    repo_bob.read_file("file", &mut buf)?;
    assert_eq!(buf, b"x\nc\n");

    // Alice applies Bob's change and resolution.
    debug!("Alice applies Bob's change");
    apply::apply_change_arc(&changes, &txn_alice, &channel_alice, &p_bob).unwrap();
    output::output_repository_no_pending(
        &repo_alice,
        &changes,
        &txn_alice,
        &channel_alice,
        "",
        true,
        None,
        1,
        0,
    )?;
    let mut buf2 = Vec::new();
    repo_alice.read_file("file", &mut buf2)?;
    let buf2_ = re.replace_all(&buf2, &[][..]);

    assert_eq!(std::str::from_utf8(&buf2_), Ok(">>>>>>> 1\nx\n<<<<<<< 1\n"));
    apply::apply_change_arc(&changes, &txn_alice, &channel_alice, &resolution).unwrap();
    output::output_repository_no_pending(
        &repo_alice,
        &changes,
        &txn_alice,
        &channel_alice,
        "",
        true,
        None,
        1,
        0,
    )?;
    let mut buf2 = Vec::new();
    repo_alice.read_file("file", &mut buf2)?;
    assert_eq!(std::str::from_utf8(&buf), std::str::from_utf8(&buf2));
    Ok(())
}
#[test]
fn zombie_half_survivor() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let repo_alice = working_copy::memory::Memory::new();
    let repo_bob = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();

    let env_alice = pristine::sanakirja::Pristine::new_anon()?;
    let txn_alice = env_alice.arc_txn_begin().unwrap();
    let env_bob = pristine::sanakirja::Pristine::new_anon()?;
    let txn_bob = env_bob.arc_txn_begin().unwrap();

    let channel_alice = txn_alice
        .write()
        .open_or_create_channel(&SmallString::from_str("alice"))
        .unwrap();

    // Alice records
    txn_alice.write().add_file("file", 0).unwrap();
    repo_alice.add_file("file", b"".to_vec());
    let x: &[&[u8]] = &[b"a\nb\nc\nd\n", b""];
    let p_alice: Vec<_> = x
        .iter()
        .map(|c| {
            repo_alice
                .write_file("file", Inode::ROOT)
                .unwrap()
                .write_all(c)
                .unwrap();
            record_all(&repo_alice, &changes, &txn_alice, &channel_alice, "").unwrap()
        })
        .collect();

    // Bob clones
    let channel_bob = txn_bob
        .write()
        .open_or_create_channel(&SmallString::from_str("bob"))
        .unwrap();
    apply::apply_change_arc(&changes, &txn_bob, &channel_bob, &p_alice[0]).unwrap();
    output::output_repository_no_pending(
        &repo_bob,
        &changes,
        &txn_bob,
        &channel_bob,
        "",
        true,
        None,
        1,
        0,
    )?;

    // Bob creates an order conflict just to keep line "c" connected
    // to the root.
    repo_bob
        .write_file("file", Inode::ROOT)
        .unwrap()
        .write_all(b"a\nb\nx\ny\nz\nc\nd\n")
        .unwrap();
    let p_bob = record_all(&repo_bob, &changes, &txn_bob, &channel_bob, "").unwrap();

    // Bob applies all of Alice's other changes
    for p in &p_alice[1..] {
        apply::apply_change_arc(&changes, &txn_bob, &channel_bob, p).unwrap();
    }
    output::output_repository_no_pending(
        &repo_bob,
        &changes,
        &txn_bob,
        &channel_bob,
        "",
        true,
        None,
        1,
        0,
    )?;
    let mut buf = Vec::new();
    repo_bob.read_file("file", &mut buf)?;
    let re = regex::bytes::Regex::new(r#" \[[^\]]*\]"#).unwrap();
    let buf_ = re.replace_all(&buf, &[][..]);
    assert_eq!(
        std::str::from_utf8(&buf_),
        Ok(">>>>>>> 1\nx\ny\nz\n<<<<<<< 1\n")
    );

    repo_bob
        .write_file("file", Inode::ROOT)
        .unwrap()
        .write_all(b"a\nz\nd\n")?;
    let resolution = record_all(&repo_bob, &changes, &txn_bob, &channel_bob, "").unwrap();
    output::output_repository_no_pending(
        &repo_bob,
        &changes,
        &txn_bob,
        &channel_bob,
        "",
        true,
        None,
        1,
        0,
    )?;
    buf.clear();
    repo_bob.read_file("file", &mut buf)?;
    assert_eq!(buf, b"a\nz\nd\n");

    // Alice applies Bob's change and resolution.
    apply::apply_change_arc(&changes, &txn_alice, &channel_alice, &p_bob).unwrap();
    apply::apply_change_arc(&changes, &txn_alice, &channel_alice, &resolution).unwrap();
    output::output_repository_no_pending(
        &repo_alice,
        &changes,
        &txn_alice,
        &channel_alice,
        "",
        true,
        None,
        1,
        0,
    )?;
    let mut buf2 = Vec::new();
    repo_alice.read_file("file", &mut buf2)?;
    assert_eq!(std::str::from_utf8(&buf), std::str::from_utf8(&buf2));
    Ok(())
}

#[test]
fn three_way_zombie() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let contents = b"u\na\nb\nc\nd\nv\n";
    let alice = b"u\na\nb\nx\nc\nd\nv\n";
    let bob = b"u\na\nd\nv\n";
    let alice_bob = b"u\na\nx\nd\nv\n";
    let charlie = b"u\nv\n";

    let repo_alice = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();
    repo_alice.add_file("file", contents.to_vec());

    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    let channel_alice = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("alice"))?;
    txn.write().add_file("file", 0)?;
    let init_h = record_all(&repo_alice, &changes, &txn, &channel_alice, "")?;

    // Bob clones
    let repo_bob = working_copy::memory::Memory::new();
    let channel_bob = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("bob"))?;
    apply::apply_change_arc(&changes, &txn, &channel_bob, &init_h)?;
    output::output_repository_no_pending(
        &repo_bob,
        &changes,
        &txn,
        &channel_bob,
        "",
        true,
        None,
        1,
        0,
    )?;
    info!("Done outputting Bob's working_copy");
    {
        let mut buf = Vec::new();
        repo_bob.read_file("file", &mut buf).unwrap();
        info!("Bob = {:?}", std::str::from_utf8(&buf));
    }

    // Charlie clones
    let repo_charlie = working_copy::memory::Memory::new();
    let channel_charlie = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("charlie"))?;
    apply::apply_change_arc(&changes, &txn, &channel_charlie, &init_h)?;
    output::output_repository_no_pending(
        &repo_charlie,
        &changes,
        &txn,
        &channel_charlie,
        "",
        true,
        None,
        1,
        0,
    )?;

    // Alice adds a line.
    repo_alice
        .write_file("file", Inode::ROOT)
        .unwrap()
        .write_all(alice)
        .unwrap();
    let alice_h = record_all(&repo_alice, &changes, &txn, &channel_alice, "")?;

    // Bob deletes the context.
    repo_bob
        .write_file("file", Inode::ROOT)
        .unwrap()
        .write_all(bob)
        .unwrap();
    let bob_h = record_all(&repo_bob, &changes, &txn, &channel_bob, "")?;

    // Charlie also deletes the context.
    repo_charlie
        .write_file("file", Inode::ROOT)
        .unwrap()
        .write_all(charlie)
        .unwrap();
    record_all(&repo_charlie, &changes, &txn, &channel_charlie, "")?;

    // Alice applies Bob's change
    apply::apply_change_arc(&changes, &txn, &channel_alice, &bob_h)?;
    output::output_repository_no_pending(
        &repo_alice,
        &changes,
        &txn,
        &channel_alice,
        "",
        true,
        None,
        1,
        0,
    )?;
    let mut buf = Vec::new();
    repo_alice.read_file("file", &mut buf)?;
    debug!("alice = {:?}", std::str::from_utf8(&buf));

    // Alice solves the conflict.
    repo_alice
        .write_file("file", Inode::ROOT)
        .unwrap()
        .write_all(alice_bob)
        .unwrap();

    let resolution = record_all(&repo_alice, &changes, &txn, &channel_alice, "")?;
    output::output_repository_no_pending(
        &repo_alice,
        &changes,
        &txn,
        &channel_alice,
        "",
        true,
        None,
        1,
        0,
    )?;

    // Bob applies Alice's edits and resolution.
    apply::apply_change_arc(&changes, &txn, &channel_bob, &alice_h)?;
    apply::apply_change_arc(&changes, &txn, &channel_bob, &resolution)?;

    // Charlie applies all changes
    /*output::output_repository_no_pending(
    &mut repo_charlie,
    &changes,
    &mut txn,
    &mut channel_charlie,
    "",
    )?;*/
    apply::apply_change_arc(&changes, &txn, &channel_charlie, &bob_h)?;
    apply::apply_change_arc(&changes, &txn, &channel_charlie, &alice_h)?;
    apply::apply_change_arc(&changes, &txn, &channel_charlie, &resolution)?;

    Ok(())
}

#[test]
fn cyclic_conflict_resolution() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let contents = b"a\nb\n";
    let alice = b"a\nx\ny\nz\nb\n";
    let bob = b"a\nu\nv\nw\nb\n";
    let charlie = b"a\nU\nV\nW\nb\n";

    let repo_alice = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();
    repo_alice.add_file("file", contents.to_vec());

    let env = pristine::sanakirja::Pristine::new_anon()?;
    let mut txn = env.arc_txn_begin().unwrap();
    let channel_alice = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("alice"))?;
    txn.write().add_file("file", 0)?;
    let init_h = record_all(&repo_alice, &changes, &txn, &channel_alice, "")?;

    // Bob clones
    let mut repo_bob = working_copy::memory::Memory::new();
    let mut channel_bob = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("bob"))?;
    apply::apply_change_arc(&changes, &txn, &channel_bob, &init_h)?;
    output::output_repository_no_pending(
        &repo_bob,
        &changes,
        &txn,
        &channel_bob,
        "",
        true,
        None,
        1,
        0,
    )?;
    info!("Done outputting Bob's working_copy");
    {
        let mut buf = Vec::new();
        repo_bob.read_file("file", &mut buf).unwrap();
        info!("Bob = {:?}", std::str::from_utf8(&buf));
    }

    // Charlie clones and makes something independent.
    let repo_charlie = working_copy::memory::Memory::new();
    let channel_charlie = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("charlie"))?;
    apply::apply_change_arc(&changes, &txn, &channel_charlie, &init_h)?;
    output::output_repository_no_pending(
        &repo_charlie,
        &changes,
        &txn,
        &channel_charlie,
        "",
        true,
        None,
        1,
        0,
    )?;
    repo_charlie
        .write_file("file", Inode::ROOT)
        .unwrap()
        .write_all(charlie)
        .unwrap();
    let charlie_h = record_all(&repo_charlie, &changes, &txn, &channel_charlie, "")?;
    info!("Done outputting Charlie's working_copy");
    {
        let mut buf = Vec::new();
        repo_charlie.read_file("file", &mut buf).unwrap();
        info!("Charlie = {:?}", std::str::from_utf8(&buf));
    }

    // Bob edits and records
    repo_bob
        .write_file("file", Inode::ROOT)
        .unwrap()
        .write_all(bob)
        .unwrap();
    let bob_h = record_all(&repo_bob, &changes, &txn, &channel_bob, "")?;

    // Alice edits and records
    repo_alice
        .write_file("file", Inode::ROOT)
        .unwrap()
        .write_all(alice)
        .unwrap();
    let alice_h = record_all(&repo_alice, &changes, &txn, &channel_alice, "")?;

    // Alice applies Bob's change
    apply::apply_change_arc(&changes, &txn, &channel_alice, &bob_h)?;
    output::output_repository_no_pending(
        &repo_alice,
        &changes,
        &txn,
        &channel_alice,
        "",
        true,
        None,
        1,
        0,
    )?;
    let mut buf = Vec::new();
    repo_alice.read_file("file", &mut buf)?;
    debug!("alice: {:?}", std::str::from_utf8(&buf));

    // Alice solves the conflict.
    let conflict: Vec<_> = std::str::from_utf8(&buf)?.lines().collect();
    {
        let mut w = repo_alice.write_file("file", Inode::ROOT).unwrap();
        for l in conflict.iter() {
            if l.len() < 10 {
                writeln!(w, "{}", l)?
            }
        }
    }
    info!("resolving");
    let alices_resolution = record_all(&repo_alice, &changes, &txn, &channel_alice, "")?;

    // Bob applies Alice's change
    apply::apply_change_arc(&changes, &txn, &channel_bob, &alice_h).unwrap();
    output::output_repository_no_pending(
        &repo_bob,
        &changes,
        &txn,
        &channel_bob,
        "",
        true,
        None,
        1,
        0,
    )?;
    buf.clear();
    repo_bob.read_file("file", &mut buf)?;
    debug!("bob: {:?}", std::str::from_utf8(&buf));
    let conflict: Vec<_> = std::str::from_utf8(&buf)?.lines().collect();
    {
        let mut w = repo_bob.write_file("file", Inode::ROOT).unwrap();
        for l in conflict.iter() {
            if l.len() < 10 {
                writeln!(w, "{}", l)?
            }
        }
    }
    info!("resolving");
    let _bobs_resolution = record_all(&repo_bob, &changes, &txn, &channel_bob, "")?;

    // Bob applies Alice's resolution
    apply::apply_change_arc(&changes, &txn, &channel_bob, &alices_resolution).unwrap();
    // Bob applies Charlie's side
    apply::apply_change_arc(&changes, &txn, &channel_bob, &charlie_h).unwrap();
    debug!("outputting bob2");
    output::output_repository_no_pending(
        &repo_bob,
        &changes,
        &txn,
        &channel_bob,
        "",
        true,
        None,
        1,
        0,
    )?;
    buf.clear();
    repo_bob.read_file("file", &mut buf)?;
    // Check that there is a conflict.
    assert!(std::str::from_utf8(&buf)?.lines().any(|l| l.len() >= 10));
    debug!("{:?}", std::str::from_utf8(&buf));
    // Solve it again, in the same way and output the result.
    let conflict: Vec<_> = std::str::from_utf8(&buf)?.lines().collect();
    {
        let mut w = repo_bob.write_file("file", Inode::ROOT).unwrap();
        for l in conflict.iter() {
            if l.len() < 10 {
                writeln!(w, "{}", l)?
            }
        }
    }
    debug!("resolving again");
    let second_resolution = record_all(&mut repo_bob, &changes, &mut txn, &mut channel_bob, "")?;
    output::output_repository_no_pending(
        &mut repo_bob,
        &changes,
        &mut txn,
        &mut channel_bob,
        "",
        true,
        None,
        1,
        0,
    )?;
    buf.clear();
    repo_bob.read_file("file", &mut buf)?;

    // Check that the conflict is gone.
    assert!(std::str::from_utf8(&buf)?.lines().all(|l| l.len() < 10));

    // Unrecord
    crate::unrecord::unrecord(
        &mut *txn.write(),
        &channel_bob,
        &changes,
        &second_resolution,
        0,
        &mut Default::default(),
    )
    .unwrap();
    output::output_repository_no_pending(
        &repo_bob,
        &changes,
        &txn,
        &channel_bob,
        "",
        true,
        None,
        1,
        0,
    )?;
    buf.clear();
    repo_bob.read_file("file", &mut buf)?;

    // Check that the conflict is back.
    assert!(std::str::from_utf8(&buf)?.lines().any(|l| l.len() >= 10));

    Ok(())
}

#[test]
fn cyclic_zombies() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let contents = b"a\nb\nc\n";
    let alice = b"a\nx\ny\nz\nb\nc\n";
    let alice2 = b"a\nx\nX\ny\nz\nb\nc\n";
    let alice3 = b"a\nx\nX\nY\ny\nz\nb\nc\n";
    let bob = b"a\nu\nv\nw\nb\nc\n";
    let bob2 = b"a\nu\nU\nv\nw\nb\nc\n";
    let bob3 = b"a\nu\nU\nV\nv\nw\nb\nc\n";
    let charlie = b"a\nc\n";

    let mut repo_alice = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();
    repo_alice.add_file("file", contents.to_vec());

    let env = pristine::sanakirja::Pristine::new_anon()?;
    let mut txn = env.arc_txn_begin().unwrap();
    let mut channel_alice = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("alice"))?;
    txn.write().add_file("file", 0)?;
    let init_h = record_all(&repo_alice, &changes, &txn, &channel_alice, "")?;

    // Bob clones
    let repo_bob = working_copy::memory::Memory::new();
    let channel_bob = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("bob"))?;
    apply::apply_change_arc(&changes, &txn, &channel_bob, &init_h)?;
    output::output_repository_no_pending(
        &repo_bob,
        &changes,
        &txn,
        &channel_bob,
        "",
        true,
        None,
        1,
        0,
    )?;
    info!("Done outputting Bob's working_copy");
    {
        let mut buf = Vec::new();
        repo_bob.read_file("file", &mut buf).unwrap();
        info!("Bob = {:?}", std::str::from_utf8(&buf));
    }

    // Bob edits and records
    repo_bob
        .write_file("file", Inode::ROOT)
        .unwrap()
        .write_all(bob)
        .unwrap();
    let bob_h1 = record_all(&repo_bob, &changes, &txn, &channel_bob, "")?;
    repo_bob
        .write_file("file", Inode::ROOT)
        .unwrap()
        .write_all(bob2)
        .unwrap();
    let bob_h2 = record_all(&repo_bob, &changes, &txn, &channel_bob, "")?;
    repo_bob
        .write_file("file", Inode::ROOT)
        .unwrap()
        .write_all(bob3)
        .unwrap();
    let bob_h3 = record_all(&repo_bob, &changes, &txn, &channel_bob, "")?;

    // Alice edits and records
    repo_alice
        .write_file("file", Inode::ROOT)
        .unwrap()
        .write_all(alice)
        .unwrap();
    let alice_h1 = record_all(&repo_alice, &changes, &txn, &channel_alice, "")?;
    repo_alice
        .write_file("file", Inode::ROOT)
        .unwrap()
        .write_all(alice2)
        .unwrap();
    let alice_h2 = record_all(&repo_alice, &changes, &txn, &channel_alice, "")?;
    repo_alice
        .write_file("file", Inode::ROOT)
        .unwrap()
        .write_all(alice3)
        .unwrap();
    let alice_h3 = record_all(&repo_alice, &changes, &txn, &channel_alice, "")?;

    // Alice applies Bob's change
    apply::apply_change_arc(&changes, &txn, &channel_alice, &bob_h1)?;
    apply::apply_change_arc(&changes, &txn, &channel_alice, &bob_h2)?;
    apply::apply_change_arc(&changes, &txn, &channel_alice, &bob_h3)?;
    output::output_repository_no_pending(
        &repo_alice,
        &changes,
        &txn,
        &channel_alice,
        "",
        true,
        None,
        1,
        0,
    )?;
    let mut buf = Vec::new();
    repo_alice.read_file("file", &mut buf)?;
    debug!("alice: {:?}", std::str::from_utf8(&buf));

    // Alice solves the conflict.
    let conflict: Vec<_> = std::str::from_utf8(&buf)?.lines().collect();
    {
        let mut w = repo_alice.write_file("file", Inode::ROOT).unwrap();
        for l in conflict.iter() {
            if l.len() < 10 {
                writeln!(w, "{}", l)?
            }
        }
    }
    info!("resolving");
    let alices_resolution =
        record_all(&mut repo_alice, &changes, &mut txn, &mut channel_alice, "")?;

    // Bob applies Alice's change
    apply::apply_change_arc(&changes, &txn, &channel_bob, &alice_h1).unwrap();
    apply::apply_change_arc(&changes, &txn, &channel_bob, &alice_h2).unwrap();
    apply::apply_change_arc(&changes, &txn, &channel_bob, &alice_h3).unwrap();
    output::output_repository_no_pending(
        &repo_bob,
        &changes,
        &txn,
        &channel_bob,
        "",
        true,
        None,
        1,
        0,
    )?;
    buf.clear();
    repo_bob.read_file("file", &mut buf)?;
    debug!("bob: {:?}", std::str::from_utf8(&buf));
    let conflict: Vec<_> = std::str::from_utf8(&buf)?.lines().collect();
    {
        let mut w = repo_bob.write_file("file", Inode::ROOT).unwrap();
        for l in conflict.iter() {
            if l.len() < 10 {
                writeln!(w, "{}", l)?
            }
        }
    }
    info!("resolving");
    // Bob solves the conflict
    let bobs_resolution = record_all(&repo_bob, &changes, &txn, &channel_bob, "")?;

    // Charlie clones and deletes
    let repo_charlie = working_copy::memory::Memory::new();
    let channel_charlie = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("charlie"))?;
    apply::apply_change_arc(&changes, &txn, &channel_charlie, &init_h)?;
    apply::apply_change_arc(&changes, &txn, &channel_charlie, &alice_h1)?;
    apply::apply_change_arc(&changes, &txn, &channel_charlie, &alice_h2)?;
    apply::apply_change_arc(&changes, &txn, &channel_charlie, &alice_h3)?;
    apply::apply_change_arc(&changes, &txn, &channel_charlie, &bob_h1)?;
    apply::apply_change_arc(&changes, &txn, &channel_charlie, &bob_h2)?;
    apply::apply_change_arc(&changes, &txn, &channel_charlie, &bob_h3)?;
    output::output_repository_no_pending(
        &repo_charlie,
        &changes,
        &txn,
        &channel_charlie,
        "",
        true,
        None,
        1,
        0,
    )?;
    repo_charlie
        .write_file("file", Inode::ROOT)
        .unwrap()
        .write_all(charlie)
        .unwrap();
    let charlie_h = record_all(&repo_charlie, &changes, &txn, &channel_charlie, "")?;

    // Bob applies Alice's resolution
    apply::apply_change_arc(&changes, &txn, &channel_bob, &alices_resolution).unwrap();
    debug!("outputting bob2");
    output::output_repository_no_pending(
        &repo_bob,
        &changes,
        &txn,
        &channel_bob,
        "",
        true,
        None,
        1,
        0,
    )?;
    buf.clear();
    repo_bob.read_file("file", &mut buf)?;

    // Bob applies Charlie's side
    debug!("applying charlie's patch");

    {
        let txn = txn.read();
        let graph = channel_bob.read();
        let mut f = std::fs::File::create("bob0").unwrap();
        crate::pristine::debug(&*txn, &txn.graph(&*graph), &mut f).unwrap();
    }
    apply::apply_change_arc(&changes, &txn, &channel_bob, &charlie_h).unwrap();

    {
        let txn = txn.read();
        let graph = channel_bob.read();
        let mut f = std::fs::File::create("bob1").unwrap();
        crate::pristine::debug(&*txn, &txn.graph(&*graph), &mut f).unwrap();
    }
    let (alive_, reachable_) = check_alive(&*txn.read(), &channel_bob.read());
    if !alive_.is_empty() {
        error!("alive (bob0): {:?}", alive_);
    }
    if !reachable_.is_empty() {
        error!("reachable (bob0): {:?}", reachable_);
    }
    debug!("outputting bob's repo");
    output::output_repository_no_pending(
        &repo_bob,
        &changes,
        &txn,
        &channel_bob,
        "",
        true,
        None,
        1,
        0,
    )?;
    let (alive, reachable) = check_alive(&*txn.read(), &channel_bob.read());
    if !alive.is_empty() {
        panic!("alive (bob1): {:?}", alive);
    } else if !alive_.is_empty() {
        panic!("alive_ (bob1): {:?}", alive_);
    }
    if !reachable.is_empty() {
        panic!("reachable (bob1): {:?}", reachable);
    } else if !reachable_.is_empty() {
        panic!("reachable_ (bob1): {:?}", reachable_);
    }

    // Symmetric: Charlie applies the other sides.
    debug!("Charlie applies");
    apply::apply_change_arc(&changes, &txn, &channel_charlie, &alices_resolution).unwrap();
    apply::apply_change_arc(&changes, &txn, &channel_charlie, &bobs_resolution).unwrap();
    let (alive, reachable) = check_alive(&*txn.read(), &channel_charlie.read());
    if !alive.is_empty() {
        panic!("alive (charlie0): {:?}", alive);
    }
    if !reachable.is_empty() {
        panic!("reachable (charlie0): {:?}", reachable);
    }
    output::output_repository_no_pending(
        &repo_charlie,
        &changes,
        &txn,
        &channel_charlie,
        "",
        true,
        None,
        1,
        0,
    )?;

    let (alive, reachable) = check_alive(&*txn.read(), &channel_charlie.read());
    if !alive.is_empty() {
        panic!("alive (charlie1): {:?}", alive);
    }
    if !reachable.is_empty() {
        panic!("reachable (charlie1): {:?}", reachable);
    }

    Ok(())
}

#[test]
fn cyclic_files() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let contents = b"a\nb\n";

    let mut repo_alice = working_copy::memory::Memory::new();
    let repo_bob = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();
    repo_alice.add_file("a/file", contents.to_vec());
    repo_alice.add_file("b/file", contents.to_vec());

    let env_alice = pristine::sanakirja::Pristine::new_anon()?;
    let env_bob = pristine::sanakirja::Pristine::new_anon()?;
    let mut txn_alice = env_alice.arc_txn_begin().unwrap();
    let txn_bob = env_bob.arc_txn_begin().unwrap();
    let mut channel_alice = txn_alice
        .write()
        .open_or_create_channel(&SmallString::from_str("alice"))?;
    txn_alice.write().add_file("a/file", 0)?;
    txn_alice.write().add_file("b/file", 0)?;
    let init_h = record_all(&repo_alice, &changes, &txn_alice, &channel_alice, "")?;

    // Bob clones and moves a -> a/b
    let channel_bob = txn_bob
        .write()
        .open_or_create_channel(&SmallString::from_str("bob"))?;
    apply::apply_change_arc(&changes, &txn_bob, &channel_bob, &init_h)?;
    output::output_repository_no_pending(
        &repo_bob,
        &changes,
        &txn_bob,
        &channel_bob,
        "",
        true,
        None,
        1,
        0,
    )?;
    info!("Done outputting Bob's working_copy");
    txn_bob.write().move_file("a", "b/a", 0).unwrap();
    repo_bob.rename("a", "b/a").unwrap();
    let ab = record_all(&repo_bob, &changes, &txn_bob, &channel_bob, "")?;

    // Alice moves b -> b/a
    txn_alice.write().move_file("b", "a/b", 0).unwrap();
    repo_alice.rename("b", "a/b").unwrap();
    let _ba = record_all(&repo_alice, &changes, &txn_alice, &channel_alice, "")?;
    apply::apply_change_arc(&changes, &txn_alice, &channel_alice, &ab)?;
    debug!("outputting cycle");
    output::output_repository_no_pending(
        &repo_alice,
        &changes,
        &txn_alice,
        &channel_alice,
        "",
        true,
        None,
        1,
        0,
    )?;

    let v: Vec<_> = txn_alice.write().iter_working_copy().collect();
    println!("{:?}", v);
    let (alive, reachable) = check_alive(&*txn_alice.read(), &channel_alice.read());
    if !alive.is_empty() {
        panic!("alive: {:?}", alive);
    }
    if !reachable.is_empty() {
        panic!("reachable: {:?}", reachable);
    }
    debug!("recording the resolution");
    let _resolution = record_all(
        &mut repo_alice,
        &changes,
        &mut txn_alice,
        &mut channel_alice,
        "",
    )?;
    Ok(())
}

#[test]
fn tree_inodes_test() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let contents = b"a\nb\n";
    let repo_alice = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();
    repo_alice.add_file("a/b/file", contents.to_vec());

    let env_alice = pristine::sanakirja::Pristine::new_anon()?;
    let txn_alice = env_alice.arc_txn_begin().unwrap();
    let channel_alice = txn_alice
        .write()
        .open_or_create_channel(&SmallString::from_str("alice"))?;
    txn_alice.write().add_file("a/b/file", 0)?;

    let env_bob = pristine::sanakirja::Pristine::new_anon()?;
    let txn_bob = env_bob.arc_txn_begin().unwrap();
    let channel_bob = txn_bob
        .write()
        .open_or_create_channel(&SmallString::from_str("bob"))?;
    txn_bob.write().add_file("a/b/file", 0)?;

    let init_h = record_all(&repo_alice, &changes, &txn_alice, &channel_alice, "")?;

    // Bob clones
    let repo_bob = working_copy::memory::Memory::new();
    apply::apply_change_arc(&changes, &txn_bob, &channel_bob, &init_h)?;
    output::output_repository_no_pending(
        &repo_bob,
        &changes,
        &txn_bob,
        &channel_bob,
        "",
        true,
        None,
        1,
        0,
    )?;
    info!("Done outputting Bob's working_copy");

    // Bob moves and deletes a/b
    repo_bob.rename("a/b/file", "c/d/file")?;
    txn_bob.write().move_file("a/b/file", "c/d/file", 0)?;
    repo_bob.remove_path("a", true)?;
    txn_bob.write().remove_file("a")?;
    let bob_h = record_all(&repo_bob, &changes, &txn_bob, &channel_bob, "")?;

    // Alice applies
    apply::apply_change_arc(&changes, &txn_alice, &channel_alice, &bob_h)?;
    output::output_repository_no_pending(
        &repo_alice,
        &changes,
        &txn_alice,
        &channel_alice,
        "",
        true,
        None,
        1,
        0,
    )?;

    check_tree_inodes(&*txn_alice.read(), &*channel_alice.read());
    Ok(())
}

fn check_tree_inodes<T: TxnT>(txn: &T, channel: &T::Channel) {
    // Sanity check
    for x in txn.iter_inodes().unwrap() {
        let (&inode, sinode) = x.unwrap();
        let vertex = sinode.position;
        debug!("inode = {:?}, vertex = {:?}", inode, vertex);
        let mut inode_ = inode;
        while !inode_.is_root() {
            if let Some(next) = txn.get_revtree(&inode_, None).unwrap() {
                debug!("next = {:?}", next);
                inode_ = next.parent_inode;
            } else {
                panic!("inode = {:?}, inode_ = {:?}", inode, inode_);
            }
        }
        if !is_alive(txn, T::graph(txn, &channel), &vertex.inode_vertex()).unwrap() {
            for e in iter_adjacent(
                txn,
                T::graph(txn, &channel),
                vertex.inode_vertex(),
                EdgeFlags::empty(),
                EdgeFlags::all(),
            )
            .unwrap()
            {
                error!("{:?} {:?} {:?}", inode, vertex, e)
            }
            panic!(
                "inode {:?}, vertex {:?}, is not alive, {:?}",
                inode,
                vertex,
                tree_path(txn, &vertex)
            )
        }
    }
}

// Recording a file that is still in conflict must treat the conflict markers as
// *structure*, not as content: a still-conflicted file records nothing, and
// editing the sides of the conflict (without resolving it) records only those
// edits — never the marker lines themselves.
//
// This is the diff post-processing feature: the recorded change must not
// contain any conflict-marker string (`>>>>>>>`, `=======`, `<<<<<<<`) as
// inserted or deleted content. A regression that "just records the conflict
// markers" as plain text would still round-trip through output (the markers
// become literal content and reappear), so a round-trip assertion alone does
// not catch it — we assert on the recorded change itself.
#[test]
fn record_conflict_ignores_markers() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let contents = b"a\nb\nc\n";
    let alice = b"a\nx\nb\nc\n";
    let bob = b"a\ny\nb\nc\n";

    let mut repo = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();
    repo.add_file("file", contents.to_vec());

    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    let channel_alice = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("alice"))?;
    txn.write().add_file("file", 0)?;
    let init_h = record_all(&mut repo, &changes, &txn, &channel_alice, "")?;

    let channel_bob = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("bob"))?;
    apply::apply_change_arc(&changes, &txn, &channel_bob, &init_h)?;
    output::output_repository_no_pending(
        &repo,
        &changes,
        &txn,
        &channel_bob,
        "",
        true,
        None,
        1,
        0,
    )?;

    repo.write_file("file", Inode::ROOT)
        .unwrap()
        .write_all(bob)
        .unwrap();
    let bob_h = record_all(&repo, &changes, &txn, &channel_bob, "")?;

    repo.write_file("file", Inode::ROOT)
        .unwrap()
        .write_all(alice)
        .unwrap();
    let _alice_h = record_all(&repo, &changes, &txn, &channel_alice, "")?;

    apply::apply_change_arc(&changes, &txn, &channel_alice, &bob_h)?;
    output::output_repository_no_pending(
        &repo,
        &changes,
        &txn,
        &channel_alice,
        "",
        true,
        None,
        1,
        0,
    )?;
    let mut conflicted = Vec::new();
    repo.read_file("file", &mut conflicted)?;
    let conflicted_str = std::str::from_utf8(&conflicted)?.to_string();
    info!("conflicted:\n{}", conflicted_str);
    // Sanity: the working copy really is in conflict (has both markers and both
    // sides), otherwise the rest of the test proves nothing.
    assert!(
        conflicted_str.contains(">>>>>>>"),
        "no start marker: {:?}",
        conflicted_str
    );
    assert!(
        conflicted_str.contains("======="),
        "no separator: {:?}",
        conflicted_str
    );
    assert!(
        conflicted_str.contains("<<<<<<<"),
        "no end marker: {:?}",
        conflicted_str
    );
    assert!(conflicted_str.contains("\nx\n") && conflicted_str.contains("\ny\n"));

    // 1. Recording the still-conflicted file *unchanged* must be a no-op: the
    //    conflict parts are ignored, so there is nothing to record.
    let (_h, noop) = record_all_change(&repo, &changes, &txn, &channel_alice, "")?;
    assert!(
        noop.changes.is_empty(),
        "recording an unchanged conflict produced hunks:\n{}",
        render_change(&changes, &noop)
    );

    // 2. Edit both sides of the conflict without resolving it: x -> X, y -> Y,
    //    leaving every marker line untouched.
    let edited: String = conflicted_str
        .lines()
        .map(|l| match l {
            "x" => "X".to_string(),
            "y" => "Y".to_string(),
            other => other.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    repo.write_file("file", Inode::ROOT)
        .unwrap()
        .write_all(edited.as_bytes())
        .unwrap();

    let (_h, edit, markers) = record_all_change_markers(&repo, &changes, &txn, &channel_alice, "")?;
    let rendered = render_change(&changes, &edit);
    info!("recorded edit:\n{}", rendered);

    // The recorded change must contain the side edits …
    assert!(
        !edit.changes.is_empty(),
        "editing the conflict sides recorded nothing"
    );
    // … and the file must NOT be flagged as carrying conflict markers: here the
    // graph *is* in conflict, so the markers are handled as structure and never
    // reach content — the record proceeds without `--accept-conflict-markers`.
    assert!(
        markers.is_empty(),
        "the nominal edited-sides case must not be flagged, got {:?}",
        markers
    );
    // … but MUST NOT record any conflict-marker line as content. This is the
    // assertion that a marker-dumping regression fails.
    for line in rendered.lines() {
        let content = line.strip_prefix("+ ").or_else(|| line.strip_prefix("- "));
        if let Some(content) = content {
            for marker in [">>>>>>>", "=======", "<<<<<<<"] {
                assert!(
                    !content.contains(marker),
                    "recorded a conflict marker as content ({:?}):\n{}",
                    marker,
                    rendered
                );
            }
        }
    }

    // The edit really did register: X and Y are recorded as content.
    assert!(
        rendered.contains("+ X"),
        "the 'x -> X' edit was not recorded:\n{}",
        rendered
    );
    assert!(
        rendered.contains("+ Y"),
        "the 'y -> Y' edit was not recorded:\n{}",
        rendered
    );

    // Round-trip: outputting again reproduces the same still-conflicted file
    // with the edited sides (modulo the bracketed marker ids). The two sides are
    // now authored by the same change, so their relative order is a tie-break and
    // not guaranteed — compare the lines as a multiset rather than in order.
    output::output_repository_no_pending(
        &repo,
        &changes,
        &txn,
        &channel_alice,
        "",
        true,
        None,
        1,
        0,
    )?;
    let mut reout = Vec::new();
    repo.read_file("file", &mut reout)?;
    let re = regex::bytes::Regex::new(r#"\[[^\]]*\]"#).unwrap();
    let a = re.replace_all(edited.as_bytes(), &[][..]);
    let b = re.replace_all(&reout, &[][..]);
    let mut a: Vec<_> = std::str::from_utf8(&a)?.lines().collect();
    let mut b: Vec<_> = std::str::from_utf8(&b)?.lines().collect();
    a.sort_unstable();
    b.sort_unstable();
    assert_eq!(a, b, "round-trip changed the conflicted file's content");
    Ok(())
}

fn render_change<C: changestore::ChangeStore>(
    changes: &C,
    change: &crate::change::Change,
) -> String {
    let mut out = Vec::new();
    change.write(changes, None, true, &mut out).unwrap();
    String::from_utf8_lossy(&out).into_owned()
}

// Reproduction: one side of a conflict is a *pending* patch (an ephemeral
// change recorded to represent the working copy's unrecorded edits, as `pijul
// pull`/push do). That patch is later unrecorded and its change file dropped
// from the changestore — "déjà disparu du repo" — while the working copy keeps
// the conflict markers that reference it.
//
// A live conflict is re-emitted by the graph and the diff aligns those markers
// as structure, so they never reach recorded content. Once the pending side is
// unrecorded the graph is no longer in conflict, so recording the working copy
// would bake the marker lines (and the vanished side) in as literal content.
// Pijul does not heuristically guess-and-drop; it records faithfully but flags
// the file in `Recorded::conflict_marker_files` (via `is_conflict_marker_line`),
// which the CLI turns into a refusal unless `--accept-conflict-markers`.
#[test]
fn record_conflict_pending_side_gone() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let contents = b"a\nb\nc\n";
    let local = b"a\nx\nb\nc\n"; // the working copy's unrecorded (pending) edit
    let incoming = b"a\ny\nb\nc\n"; // the change pulled in, conflicting with it

    let mut repo = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();
    repo.add_file("file", contents.to_vec());

    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    let channel = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;
    txn.write().add_file("file", 0)?;
    let init_h = record_all(&mut repo, &changes, &txn, &channel, "")?;

    // The "incoming" change is authored on a separate channel and pulled in.
    let channel_other = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("other"))?;
    apply::apply_change_arc(&changes, &txn, &channel_other, &init_h)?;
    output::output_repository_no_pending(
        &repo,
        &changes,
        &txn,
        &channel_other,
        "",
        true,
        None,
        1,
        0,
    )?;
    repo.write_file("file", Inode::ROOT)
        .unwrap()
        .write_all(incoming)
        .unwrap();
    let incoming_h = record_all(&repo, &changes, &txn, &channel_other, "")?;

    // Back on `main`: the working copy has an unrecorded edit. Record it as an
    // ephemeral *pending* patch (mirrors `pijul_remote::pending`) and apply it.
    repo.write_file("file", Inode::ROOT)
        .unwrap()
        .write_all(local)
        .unwrap();
    let pending_h = record_all(&repo, &changes, &txn, &channel, "")?;

    // Pull the incoming change on top → conflict between the pending patch and
    // the incoming change; output the merged (conflicted) working copy.
    apply::apply_change_arc(&changes, &txn, &channel, &incoming_h)?;
    output::output_repository_no_pending(&repo, &changes, &txn, &channel, "", true, None, 1, 0)?;
    let mut conflicted = Vec::new();
    repo.read_file("file", &mut conflicted)?;
    let conflicted_str = std::str::from_utf8(&conflicted)?.to_string();
    info!("conflicted working copy:\n{}", conflicted_str);
    assert!(
        conflicted_str.contains(">>>>>>>"),
        "expected a conflict: {:?}",
        conflicted_str
    );

    // Restore the pending patch to unrecorded state: unrecord it AND drop its
    // change file — exactly what `Local::upload_changes` / `pijul pull` do. Now
    // one side of the conflict markers still sitting in the working copy points
    // at a change that no longer exists anywhere.
    use crate::changestore::ChangeStore;
    crate::unrecord::unrecord(
        &mut *txn.write(),
        &channel,
        &changes,
        &pending_h,
        0,
        &mut Default::default(),
    )?;
    changes.del_change(&pending_h)?;

    // Record the still-marked working copy. Pijul does not guess that these are
    // conflict markers and silently drop them; instead it records them faithfully
    // as content but *flags the file* in `conflict_marker_files`, which is what
    // the CLI turns into a refusal ("resolve the conflict, or re-run with
    // --accept-conflict-markers"). This models the `--accept-conflict-markers`
    // path (the marker text is committed verbatim) plus the signal used to refuse.
    let (_h, change, markers) = record_all_change_markers(&repo, &changes, &txn, &channel, "")?;
    let rendered = render_change(&changes, &change);
    info!("recorded:\n{}", rendered);
    assert_eq!(
        markers,
        vec!["file".to_string()],
        "the orphaned conflict markers should flag the file for the record refusal"
    );
    // The markers really are what got recorded as content (the accepted path).
    assert!(
        rendered.lines().any(|l| l
            .strip_prefix("+ ")
            .is_some_and(|c| c.starts_with(">>>>>>>"))),
        "expected the marker line to be recorded verbatim:\n{}",
        rendered
    );

    // A clean file (no markers) is never flagged: resolving the conflict lets the
    // record go through untouched.
    repo.write_file("file", Inode::ROOT)
        .unwrap()
        .write_all(b"a\nx\nb\nc\n")
        .unwrap();
    let (_h, _change, markers) = record_all_change_markers(&repo, &changes, &txn, &channel, "")?;
    assert!(
        markers.is_empty(),
        "a resolved file must not be flagged, got {:?}",
        markers
    );
    Ok(())
}

// A brand-new file bypasses the diff (its content is inserted wholesale), so the
// conflict-marker guard also lives on the add path. A freshly added file whose
// content contains markers is flagged just like a tracked one.
#[test]
fn record_new_file_conflict_markers() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let marked = b"a\n>>>>>>> 1 [ABCD1234 msg]\nx\n======= 1 [EFGH5678 msg]\ny\n<<<<<<< 1\nb\n";
    let repo = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();
    repo.add_file("file", marked.to_vec());

    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    let channel = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;
    txn.write().add_file("file", 0)?;
    let (_h, _change, markers) = record_all_change_markers(&repo, &changes, &txn, &channel, "")?;
    assert_eq!(
        markers,
        vec!["file".to_string()],
        "a new file containing conflict markers must be flagged for the record refusal"
    );

    // A clean new file is not flagged.
    let repo2 = working_copy::memory::Memory::new();
    let changes2 = changestore::memory::Memory::new();
    repo2.add_file("clean", b"a\nb\nc\n".to_vec());
    let channel2 = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("two"))?;
    txn.write().add_file("clean", 0)?;
    let (_h, _change, markers) = record_all_change_markers(&repo2, &changes2, &txn, &channel2, "")?;
    assert!(
        markers.is_empty(),
        "a clean new file must not be flagged, got {:?}",
        markers
    );
    Ok(())
}
