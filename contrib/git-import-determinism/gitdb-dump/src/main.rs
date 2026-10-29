//! Dump `.pijul/git/db`, the per-commit state table `pijul git` keeps:
//! one line per Git commit, `<git oid hex> <channel Merkle hex>`, sorted by oid.
//! Read-only; run it after a conversion has finished.
use sanakirja::btree::{self, UDb};
use sanakirja::{direct_repr, Env, RootDb};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(C)]
struct Oid([u8; 20]);
direct_repr!(Oid);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(C)]
struct Merkle([u8; 33]);
direct_repr!(Merkle);

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{:02x}", x)).collect()
}

fn main() {
    let path = std::env::args().nth(1).expect("usage: gitdb-dump <.pijul/git/db>");
    let env = Env::new(&path, 1 << 15, 2).expect("open env");
    let txn = Env::txn_begin(&env).expect("txn");
    let db: UDb<Oid, Merkle> = txn.root_db(0).expect("no root 0");
    for x in btree::iter(&txn, &db, None).expect("iter") {
        let (k, v) = x.expect("entry");
        println!("{} {}", hex(&k.0), hex(&v.0));
    }
}
