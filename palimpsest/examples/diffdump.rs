//! Dump `change_diff` output for a repo/channel/hash — debugging aid.
//! Usage: diffdump <repo-root> <channel> <base32-hash>

use pijul_core::changestore::filesystem::FileSystem as Changes;
use pijul_core::pristine::sanakirja::Pristine;
use pijul_core::{Base32, Hash};

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let root = args.next().expect("repo root");
    let channel = args.next().expect("channel name");
    let hash = args.next().expect("base32 hash");

    let root = std::path::Path::new(&root);
    let pristine = Pristine::new(root.join(".pijul").join("pristine").join("db"))?;
    let changes = Changes::from_root(root, 1024);
    let hash = Hash::from_base32(hash.as_bytes()).expect("valid base32");

    let resp = palimpsest::change_diff(&pristine, &changes, &channel, hash)?;
    println!("(full={})", resp.full);
    for d in resp.files {
        println!("=== {}", d.path);
        for v in d.vertices {
            println!(
                "{:?}\t{}:{}-{}\t{:?}",
                v.kind,
                v.change,
                v.start,
                v.end,
                v.text.as_deref().unwrap_or("<lazy>")
            );
        }
        if let Some(folds) = &d.folds {
            println!("--- folds ({}):", folds.len());
            for f in folds {
                print_fold(f, 1);
            }
        }
    }
    Ok(())
}

fn print_fold(f: &palimpsest::FoldNode, depth: usize) {
    println!("{}[{}-{}]", "  ".repeat(depth), f.start, f.end);
    for c in &f.children {
        print_fold(c, depth + 1);
    }
}
