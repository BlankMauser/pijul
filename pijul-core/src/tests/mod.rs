use crate::change::Change;
use crate::changestore::ChangeStore;
use crate::pristine::*;
use crate::record::{Algorithm, Builder};
pub use crate::small_string::SmallString;
use crate::working_copy::WorkingCopy;
use crate::*;

mod add_file;
mod change;
mod clone;
mod conflict;
mod diff;
mod file_conflicts;
mod filesystem;
mod inode_symmetry;
mod missing_context;
mod partial;
mod performance;
mod record_parallel;
mod record_reads;
mod replaces;
mod rm_file;
mod rollback;
mod text;
mod text_changes;
mod unrecord;

thread_local! {
    /// Per-thread deterministic clock for `FUZZ_FIXED_TIME` (see
    /// `record_all_change`). `None` until a fuzz seed sets it via
    /// [`fuzz_clock_set`]; otherwise falls back to the env base.
    static FUZZ_CLOCK: std::cell::Cell<Option<i64>> = const { std::cell::Cell::new(None) };
}

/// Reset the calling thread's deterministic fuzzer clock. Called at the start of
/// each `fuzz_seed` with a per-seed base so that parallel fuzzer runs produce
/// the same timestamps (and thus hashes) for a given seed regardless of how the
/// seeds are scheduled across threads.
pub(crate) fn fuzz_clock_set(base: i64) {
    FUZZ_CLOCK.with(|c| c.set(Some(base)));
}

fn record_all_change<
    T: MutTxnT + Send + Sync + 'static,
    R: WorkingCopy + Clone + Send + Sync + 'static,
    P: ChangeStore + Clone + Send + 'static,
>(
    repo: &R,
    store: &P,
    txn: &ArcTxn<T>,
    channel: &ChannelRef<T>,
    prefix: &str,
) -> Result<(Hash, Change), anyhow::Error>
where
    R::Error: Send + Sync + 'static,
{
    let mut state = Builder::new();
    state.record(
        txn.clone(),
        Algorithm::default(),
        false,
        &crate::DEFAULT_SEPARATOR,
        channel.clone(),
        repo,
        store,
        prefix,
        1,
    )?;

    let rec = state.finish();
    let stat_updates = rec.take_stat_updates();
    let changes = rec
        .actions
        .into_iter()
        .map(|rec| rec.globalize(&*txn.read()).unwrap())
        .collect();
    let mut change0 = crate::change::Change::make_change(
        &*txn.read(),
        &channel.clone(),
        changes,
        std::mem::take(&mut *rec.contents.lock()),
        crate::change::ChangeHeader {
            message: "test".to_string(),
            authors: vec![],
            description: None,
            // Beware of changing the following line: two changes
            // doing the same thing will be equal. Sometimes we don't
            // want that, as in tests::unrecord::unrecord_double.
            //
            // For deterministic hashes when debugging the fuzzer, set
            // FUZZ_FIXED_TIME to a nanosecond count; each recorded change uses a
            // monotonically increasing timestamp so the run is reproducible
            // without hash collisions. The clock is THREAD-LOCAL (not a shared
            // global) and reset per seed by `fuzz_clock_set`, so a *parallel*
            // fuzzer run is still reproducible per seed — thread interleaving
            // can't perturb a given seed's timestamps.
            timestamp: if let Ok(t) = std::env::var("FUZZ_FIXED_TIME") {
                let base: i64 = t.parse().unwrap_or(0);
                let n = FUZZ_CLOCK.with(|c| {
                    let v = c.get().unwrap_or(base);
                    c.set(Some(v + 1));
                    v
                });
                jiff::Timestamp::from_nanosecond(n as i128).unwrap()
            } else {
                jiff::Timestamp::now()
            },
        },
        Vec::new(),
    )
    .unwrap();
    let hash = store.save_change(&mut change0, |_, _| Ok::<_, anyhow::Error>(()))?;
    if log_enabled!(log::Level::Debug) {
        change0
            .write(store, Some(hash), true, &mut std::io::stderr())
            .unwrap();
    }
    apply::apply_local_change(
        &mut *txn.write(),
        &channel,
        &change0,
        &hash,
        &rec.updatables,
    )?;
    // Persist the per-inode stat cache now that the inodes exist with their
    // final positions (mirrors what the CLI does after applying).
    crate::record::update_stat_cache(&mut *txn.write(), &stat_updates, false)?;
    Ok((hash, change0))
}

fn record_all<T: MutTxnT, R: WorkingCopy, P: ChangeStore>(
    repo: &R,
    store: &P,
    txn: &ArcTxn<T>,
    channel: &ChannelRef<T>,
    prefix: &str,
) -> Result<Hash, anyhow::Error>
where
    T: MutTxnT + Send + Sync + 'static,
    R: WorkingCopy + Clone + Send + Sync + 'static,
    P: ChangeStore + Clone + Send + 'static,
    R::Error: Send + Sync + 'static,
{
    let (hash, _) = record_all_change(repo, store, txn, channel, prefix)?;
    Ok(hash)
}

fn record_all_output<
    T: MutTxnT + Send + Sync + 'static,
    R: WorkingCopy + Clone + Send + Sync + 'static,
    P: ChangeStore + Clone + Send + Sync + 'static,
>(
    repo: &R,
    changes: P,
    txn: &ArcTxn<T>,
    channel: &ChannelRef<T>,
    prefix: &str,
) -> Result<Hash, anyhow::Error>
where
    T: MutTxnT + Send + Sync + 'static,
    R: WorkingCopy + Clone + Send + Sync + 'static,
    P: ChangeStore + Clone + Send + Sync + 'static,
    R::Error: Send + Sync + 'static,
{
    let hash = record_all(repo, &changes, txn, channel, prefix)?;
    output::output_repository_no_pending(repo, &changes, txn, channel, "", true, None, 1, 0)
        .unwrap();
    Ok(hash)
}
