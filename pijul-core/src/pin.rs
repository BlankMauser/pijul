use crate::pristine::*;

pub enum ReorderError<T: ChannelMutTxnT> {
    Txn(TxnErr<T::GraphError>),
    /// A tag exists at a position that would need to be moved.
    TaggedPosition(ApplyTimestamp),
}

impl<T: ChannelMutTxnT> From<TxnErr<T::GraphError>> for ReorderError<T> {
    fn from(e: TxnErr<T::GraphError>) -> Self {
        ReorderError::Txn(e)
    }
}

impl<T: ChannelMutTxnT> std::fmt::Debug for ReorderError<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReorderError::Txn(e) => write!(f, "ReorderError::Txn({e:?})"),
            ReorderError::TaggedPosition(t) => write!(f, "ReorderError::TaggedPosition({t})"),
        }
    }
}

impl<T: ChannelMutTxnT> std::fmt::Display for ReorderError<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReorderError::Txn(e) => write!(f, "Transaction error: {e}"),
            ReorderError::TaggedPosition(t) => {
                write!(f, "Cannot reorder: position {t} is tagged")
            }
        }
    }
}

impl<T: ChannelMutTxnT + 'static> std::error::Error for ReorderError<T> {}

/// After applying a new patch, ensure pinned patches stay at the top of the
/// channel order.
///
/// If `new_change` does not depend on any pinned patch, it is moved to just
/// before the first explicitly pinned patch, and the pinned patches shift up
/// by one. If it does depend on a pinned patch it stays where it is (it is
/// implicitly pinned).
///
/// Returns `Err(ReorderError::TaggedPosition)` if any position in the range
/// that would be shuffled carries a tag.
pub fn reorder_pinned<T>(
    txn: &mut T,
    channel: &mut T::Channel,
    new_change: ChangeId,
    is_pinned: impl Fn(Hash) -> bool,
) -> Result<(), ReorderError<T>>
where
    T: ChannelMutTxnT + DepsTxnT<DepsError = <T as GraphTxnT>::GraphError> + GraphTxnT,
{
    let first_pinned = find_first_pinned(txn, channel, &is_pinned)?;
    let first_pinned = match first_pinned {
        Some(p) => p,
        None => return Ok(()),
    };

    let new_pos = match txn.get_changeset(txn.changes(channel), &new_change)? {
        Some(t) => u64::from(*t),
        None => return Ok(()),
    };

    if new_pos < first_pinned {
        return Ok(());
    }

    // If new_change depends on anything at or above the pinned watermark, it
    // is implicitly pinned — leave it in place.
    for dep in txn.iter_dep(&new_change)? {
        let (dep_id, _) = dep?;
        if let Some(dep_pos) = txn.get_changeset(txn.changes(channel), dep_id)? {
            if u64::from(*dep_pos) >= first_pinned {
                return Ok(());
            }
        }
    }

    // Refuse to shuffle over a tagged position.
    for pos in first_pinned..=new_pos {
        if txn.is_tagged(txn.tags(channel), pos)? {
            return Err(ReorderError::TaggedPosition(pos));
        }
    }

    txn.move_change(channel, new_change, new_pos, first_pinned)?;
    Ok(())
}

fn find_first_pinned<T>(
    txn: &T,
    channel: &T::Channel,
    is_pinned: &impl Fn(Hash) -> bool,
) -> Result<Option<ApplyTimestamp>, TxnErr<T::GraphError>>
where
    T: ChannelTxnT + GraphTxnT,
{
    let cursor = T::cursor_revchangeset_ref(txn, txn.rev_changes(channel), None)?;
    for x in cursor {
        let (t, pair) = x?;
        if let Some(h) = txn.get_external(&pair.a).optional()? {
            if is_pinned(h.into()) {
                return Ok(Some(u64::from(*t)));
            }
        }
    }
    Ok(None)
}

/// Returns the position of the first explicitly pinned patch in the channel
/// order, or `None` if no pins are configured on this channel. All positions
/// at or above this watermark are implicitly pinned and must not be pushed or
/// tagged.
pub fn pinned_watermark<T>(
    txn: &T,
    channel: &T::Channel,
    is_pinned: impl Fn(Hash) -> bool,
) -> Result<Option<ApplyTimestamp>, TxnErr<T::GraphError>>
where
    T: ChannelTxnT + GraphTxnT,
{
    find_first_pinned(txn, channel, &is_pinned)
}

/// Returns true if the channel state at position `pos` includes any explicitly
/// pinned patch (i.e., `pos` is at or after the pinned watermark). Used to
/// prevent tagging a state that contains pinned patches.
pub fn state_includes_pinned<T>(
    txn: &T,
    channel: &T::Channel,
    pos: ApplyTimestamp,
    is_pinned: impl Fn(Hash) -> bool,
) -> Result<bool, TxnErr<T::GraphError>>
where
    T: ChannelTxnT + GraphTxnT,
{
    match find_first_pinned(txn, channel, &is_pinned)? {
        Some(fp) => Ok(pos >= fp),
        None => Ok(false),
    }
}
