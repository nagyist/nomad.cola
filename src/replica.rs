use core::ops::{Range, RangeBounds};

use crate::panic_messages as panic;
use crate::*;

/// A CRDT for text.
///
/// Like all other text CRDTs it allows multiple peers on a distributed
/// network to concurrently edit the same text document, making sure that they
/// all converge to the same final state without relying on a central server to
/// coordinate the edits.
///
/// However, unlike many other CRDTs, a `Replica` doesn't actually store the
/// text contents itself. This allows to decouple the text buffer from the CRDT
/// machinery needed to handle concurrent edits and guarantee convergence.
///
/// Put another way, a `Replica` is a pure CRDT that doesn't know anything
/// about where the text is actually stored. This is great because it makes it
/// very easy to use it together with any text data structure of your choice:
/// simple `String`s, gap buffers, piece tables, ropes, etc.
///
/// # How to distribute `Replica`s between peers.
///
/// When starting a new collaborative editing session, the first peer
/// initializes its `Replica` via the [`new`](Self::new) method,
/// [`encode`](Self::encode)s it and sends the result to the other peers in the
/// session. If a new peer joins the session later on, one of the peers already
/// in the session can [`encode`](Self::encode) their `Replica` and send it to
/// them.
///
/// # How to integrate remote edits.
///
/// Every time a peer performs an edit on their local buffer they must inform
/// their `Replica` by calling either [`inserted`](Self::inserted) or
/// [`deleted`](Self::deleted). This produces [`Insertion`]s and [`Deletion`]s
/// which can be sent over to the other peers using the network layer of your
/// choice.
///
/// When a peer receives a remote `Insertion` or `Deletion` they can integrate
/// it into their own `Replica` by calling either
/// [`integrate_insertion`](Self::integrate_insertion) or
/// [`integrate_deletion`](Self::integrate_deletion), respectively. The output
/// of those methods tells the peer *where* in their local buffer they should
/// apply the edit, taking into account all the other edits that have happened
/// concurrently.
///
/// Basically, you tell your `Replica` how your buffer changes, and it tells
/// you how your buffer *should* change when receiving remote edits.
#[derive(Clone)]
pub struct Replica {
    /// The unique identifier of this replica.
    id: ReplicaId,

    /// Contains all the [`EditRun`]s that have been applied to this replica so
    /// far. This is the main data structure.
    run_tree: RunTree,

    /// The value of the Lamport clock at this replica.
    lamport_clock: LamportClock,

    /// A local clock that's incremented every time a new insertion run is
    /// created at this replica. If an insertion continues the current run the
    /// clock is not incremented.
    run_clock: RunClock,

    /// Contains the latest character timestamps of all the replicas that this
    /// replica has seen so far.
    version_map: VersionMap,

    /// A clock that keeps track of the order in which insertions happened at
    /// this replica.
    deletion_map: DeletionMap,

    /// A collection of remote edits waiting to be merged.
    backlog: Backlog,
}

impl Replica {
    #[doc(hidden)]
    pub fn assert_invariants(&self) {
        self.run_tree.assert_invariants();
        self.backlog.assert_invariants(&self.version_map, &self.deletion_map);
    }

    #[doc(hidden)]
    pub fn average_gtree_inode_occupancy(&self) -> f32 {
        self.run_tree.average_inode_occupancy()
    }

    /// The [`integrate_deletion`](Replica::integrate_deletion) method is not
    /// able to immediately produce the offset range(s) to be deleted if the
    /// `Deletion` is itself dependent on some context that the `Replica`
    /// doesn't yet have. When this happens the `Deletion` is stored in an
    /// internal backlog of edits that can't be processed yet, but may be in
    /// the future.
    ///
    /// This method returns an iterator over all the backlogged deletions
    /// which are now ready to be applied to your buffer.
    ///
    /// The [`BackloggedDeletions`] iterator yields the same kind of offset
    /// ranges that [`integrate_deletion`](Replica::integrate_deletion) would
    /// have produced had the `Deletion` been integrated right away.
    ///
    /// It's very important for the ranges to be deleted in the exact same
    /// order in which they were yielded by the iterator. If you don't your
    /// buffer could permanently diverge from the other peers.
    ///
    /// # Examples
    ///
    /// ```
    /// # use cola::Replica;
    /// // A buffer with the text "Hello" is replicated between three peers.
    /// let mut replica1 = Replica::new(1, 5);
    /// let mut replica2 = replica1.fork(2);
    /// let mut replica3 = replica2.fork(3);
    ///
    /// // Peer 1 inserts " world!" at the end of the buffer, and after
    /// // integrating the insertion peer 2 deletes "world", leaving only
    /// // "Hello!".
    /// let insert_spc_world_excl = replica1.inserted(5, 7);
    /// let _ = replica2.integrate_insertion(&insert_spc_world_excl);
    /// let delete_world = replica2.deleted(5..11);
    ///
    /// // Peer 3 receives the deletion, but it can't integrate it right away
    /// // because it doesn't have the context it needs. The deletion is stored
    /// // in the backlog.
    /// let ranges = replica3.integrate_deletion(&delete_world);
    ///
    /// assert!(ranges.is_empty());
    ///
    /// // After peer 3 receives the " world!" insertion from peer 1 it can
    /// // finally integrate the deletion.
    /// let _ = replica3.integrate_insertion(&insert_spc_world_excl);
    ///
    /// let mut deletions = replica3.backlogged_deletions();
    /// assert_eq!(deletions.next(), Some(vec![5..11]));
    /// assert_eq!(deletions.next(), None);
    /// ```
    #[inline]
    pub fn backlogged_deletions(&mut self) -> BackloggedDeletions<'_> {
        BackloggedDeletions::from_replica(self)
    }

    /// The [`integrate_insertion`](Replica::integrate_insertion) method is not
    /// able to immediately produce an offset if the `Insertion` is itself
    /// dependent on some context that the `Replica` doesn't yet have. When
    /// this happens the `Insertion` is stored in an internal backlog of edits
    /// that can't be processed yet, but may be in the future.
    ///
    /// This method returns an iterator over all the backlogged insertions
    /// which are now ready to be applied to your buffer.
    ///
    /// The [`BackloggedInsertions`] iterator yields `(Text, Length)` pairs
    /// containing the [`Text`] to be inserted and the offset at which it
    /// should be inserted.
    ///
    /// It's very important for the insertions to be applied in the exact same
    /// order in which they were yielded by the iterator. If you don't your
    /// buffer could permanently diverge from the other peers.
    ///
    /// # Examples
    ///
    /// ```
    /// # use cola::Replica;
    /// // The buffer at peer 1 is "ab".
    /// let mut replica1 = Replica::new(1, 2);
    ///
    /// // A second peer joins the session.
    /// let mut replica2 = replica1.fork(2);
    ///
    /// // Peer 1 inserts 'c', 'd' and 'e' at the end of the buffer.
    /// let insert_c = replica1.inserted(2, 1);
    /// let insert_d = replica1.inserted(3, 1);
    /// let insert_e = replica1.inserted(4, 1);
    ///
    /// // For some reason, the network layer messes up the order of the edits
    /// // and they get to the second peer in the opposite order. Because each
    /// // edit depends on the previous one, peer 2 can't merge the insertions
    /// // of the 'd' and the 'e' until it sees the 'c'.
    /// let none_e = replica2.integrate_insertion(&insert_e);
    /// let none_d = replica2.integrate_insertion(&insert_d);
    ///
    /// assert!(none_e.is_none());
    /// assert!(none_d.is_none());
    ///
    /// // Finally, peer 2 receives the 'c' and it's able merge it right away.
    /// let offset_c = replica2.integrate_insertion(&insert_c).unwrap();
    ///
    /// assert_eq!(offset_c, 2);
    ///
    /// // Peer 2 now has all the context it needs to merge the rest of the
    /// // edits that were previously backlogged.
    /// let mut backlogged = replica2.backlogged_insertions();
    ///
    /// assert!(matches!(backlogged.next(), Some((_, 3))));
    /// assert!(matches!(backlogged.next(), Some((_, 4))));
    /// ```
    #[inline]
    pub fn backlogged_insertions(&mut self) -> BackloggedInsertions<'_> {
        BackloggedInsertions::from_replica(self)
    }

    #[inline]
    pub(crate) fn backlog_mut(&mut self) -> &mut Backlog {
        &mut self.backlog
    }

    /// Returns `true` if this `Replica` is ready to merge the given
    /// `Deletion`.
    #[inline]
    pub(crate) fn can_merge_deletion(&self, deletion: &Deletion) -> bool {
        debug_assert!(!self.has_merged_deletion(deletion));

        (
            // Makes sure that we merge deletions in the same order they were
            // created.
            self.deletion_map.get(deletion.deleted_by()) + 1
                == deletion.deletion_ts()
        ) && (
            // Makes sure that we have already merged all the insertions that
            // the remote `Replica` had when it generated the deletion.
            self.version_map >= *deletion.version_map()
        )
    }

    /// Returns `true` if this `Replica` is ready to merge the given
    /// `Insertion`.
    #[inline]
    pub(crate) fn can_merge_insertion(&self, insertion: &Insertion) -> bool {
        debug_assert!(!self.has_merged_insertion(insertion));

        (
            // Makes sure that we merge insertions in the same order they were
            // created.
            //
            // This is technically not needed to merge a single insertion (all
            // that matters is that we know where to anchor the insertion), but
            // it's needed to correctly increment the chararacter clock inside
            // this `Replica`'s `VersionMap` without skipping any temporal
            // range.
            self.version_map.get(insertion.inserted_by()) == insertion.start()
        ) && (
            // Makes sure that we have already merged the insertion containing
            // the anchor of this insertion.
            self.has_anchor(insertion.anchor())
        )
    }

    /// Creates a new [`Anchor`] at the given offset, with the given bias.
    ///
    /// You can think of an `Anchor` as a sticky line cursor that you can
    /// attach to a specific position in a text document, and that
    /// automatically moves when the text around it changes to make sure that
    /// it always refers to the same position.
    ///
    /// An offset alone is not enough to create an `Anchor` because it doesn't
    /// uniquely identify a position in the document. For example, the offset
    /// `1` inside `"ab"` could either refer to the right side of `'a'` or to
    /// the left side of `'b'`. If we insert a `'c'` between the two
    /// characters, how should the `Anchor` move?
    ///
    /// To resolve this ambiguity, this method requires you to specify an
    /// [`AnchorBias`]. This tells the `Replica` whether the `Anchor` should
    /// stick to the left or to the right side of the position at the given
    /// offset.
    ///
    /// # Panics
    ///
    /// Panics if the offset is out of bounds (i.e. greater than the current
    /// length of your buffer).
    ///
    /// # Examples
    ///
    /// ```
    /// # use cola::{Replica, AnchorBias};
    /// // The buffer is "ab".
    /// let mut replica = Replica::new(1, 2);
    ///
    /// // We create two anchors at the same offset but with different biases.
    /// let right_of_a = replica.create_anchor(1, AnchorBias::Left);
    /// let left_of_b = replica.create_anchor(1, AnchorBias::Right);
    ///
    /// // Now we insert a 'c' between the two characters.
    /// let _ = replica.inserted(1, 1);
    ///
    /// // When we resolve the anchors we see that the first one has stayed at
    /// // the same offset, while the second one has moved to the right.
    /// assert_eq!(replica.resolve_anchor(right_of_a).unwrap(), 1);
    /// assert_eq!(replica.resolve_anchor(left_of_b).unwrap(), 2);
    /// ```
    ///
    /// `Anchor`s can still be resolved even if the position they refer to has
    /// been deleted. In this case they will resolve to the offset of the
    /// closest position that's still visible.
    ///
    /// ```
    /// # use cola::{Replica, AnchorBias};
    /// // The buffer is "Hello world".
    /// let mut replica = Replica::new(1, 11);
    ///
    /// let right_of_r = replica.create_anchor(9, AnchorBias::Left);
    ///
    /// // " worl" is deleted, the buffer is now "Hellod".
    /// let _ = replica.deleted(5..10);
    ///
    /// // The anchor can still be resolved, and it now points to `5`, i.e.
    /// // between the 'o' and the 'd'.
    /// assert_eq!(replica.resolve_anchor(right_of_r).unwrap(), 5);
    /// ```
    ///
    /// There are two special cases to be aware of:
    ///
    /// - when the offset is zero and the bias is [`AnchorBias::Left`], the
    ///   returned `Anchor` will always resolve to zero;
    ///
    /// - when the offset is equal to the length of the buffer and the bias is
    ///   [`AnchorBias::Right`], the returned `Anchor` will always resolve to
    ///   the end of the buffer.
    ///
    /// ```
    /// # use cola::{Replica, AnchorBias};
    /// let mut replica = Replica::new(1, 5);
    ///
    /// // This anchor is to the start of the document, so it will always
    /// // resolve to zero.
    /// let start_of_document = replica.create_anchor(0, AnchorBias::Left);
    ///
    /// let _ = replica.inserted(0, 5);
    /// let _ = replica.inserted(0, 5);
    ///
    /// assert_eq!(replica.resolve_anchor(start_of_document).unwrap(), 0);
    ///
    /// // This anchor is to the end of the document, so it will always
    /// // resolve to the current length of the buffer.
    /// let end_of_document = replica.create_anchor(15, AnchorBias::Right);
    ///
    /// let _ = replica.inserted(15, 5);
    /// let _ = replica.inserted(20, 5);
    ///
    /// assert_eq!(replica.resolve_anchor(end_of_document).unwrap(), 25);
    /// ```
    #[track_caller]
    #[inline]
    pub fn create_anchor(
        &self,
        at_offset: Length,
        with_bias: AnchorBias,
    ) -> Anchor {
        if at_offset > self.len() {
            panic::offset_out_of_bounds(at_offset, self.len());
        }

        self.run_tree.create_anchor(at_offset, with_bias)
    }

    #[doc(hidden)]
    pub fn debug(&self) -> debug::DebugAsSelf<'_> {
        self.into()
    }

    #[doc(hidden)]
    pub fn debug_as_btree(&self) -> debug::DebugAsBtree<'_> {
        self.into()
    }

    /// Creates a new `Replica` with the given [`ReplicaId`] by decoding the
    /// contents of the [`EncodedReplica`].
    ///
    /// # Panics
    ///
    /// Panics if the [`ReplicaId`] is zero.
    ///
    /// # Examples
    ///
    /// ```
    /// # use cola::{Replica, EncodedReplica};
    /// let replica1 = Replica::new(1, 42);
    ///
    /// let encoded: EncodedReplica = replica1.encode();
    ///
    /// let replica2 = Replica::decode(2, &encoded).unwrap();
    ///
    /// assert_eq!(replica2.id(), 2);
    /// ```
    #[cfg(feature = "encode")]
    #[cfg_attr(docsrs, doc(cfg(feature = "encode")))]
    #[track_caller]
    #[inline]
    pub fn decode(
        id: ReplicaId,
        encoded: &EncodedReplica<'_>,
    ) -> Result<Self, DecodeError> {
        if id == 0 {
            panic::replica_id_is_zero();
        }

        let (
            run_tree,
            lamport_clock,
            mut version_map,
            mut deletion_map,
            backlog,
        ) = encoded.to_replica()?;

        version_map.fork_in_place(id, 0);

        deletion_map.fork_in_place(id, 0);

        let replica = Self {
            id,
            run_tree,
            run_clock: RunClock::new(),
            lamport_clock,
            version_map,
            deletion_map,
            backlog,
        };

        Ok(replica)
    }

    /// Informs the `Replica` that you have deleted the characters in the given
    /// offset range.
    ///
    /// This produces a [`Deletion`] which can be sent to all the other peers
    /// to integrate the deletion into their own `Replica`s.
    ///
    /// # Panics
    ///
    /// Panics if the start of the range is greater than the end or if the end
    /// is out of bounds (i.e. greater than the current length of your buffer).
    ///
    /// # Examples
    ///
    /// ```
    /// # use cola::{Replica, Deletion};
    /// // The buffer at peer 1 is "Hello World".
    /// let mut replica1 = Replica::new(1, 11);
    ///
    /// // Peer 1 deletes "Hello ".
    /// let deletion: Deletion = replica1.deleted(..6);
    /// ```
    #[track_caller]
    #[must_use]
    #[inline]
    pub fn deleted<R>(&mut self, range: R) -> Deletion
    where
        R: RangeBounds<Length>,
    {
        let (start, end) = range_bounds_to_start_end(range, 0, self.len());

        if end > self.len() {
            panic::offset_out_of_bounds(end, self.len());
        }

        if start > end {
            panic::start_greater_than_end(start, end);
        }

        if start == end {
            return Deletion::no_op();
        }

        let deleted_range = (start..end).into();

        let mut version_map = VersionMap::new(self.id(), 0);

        let (start, end) =
            self.run_tree.delete(deleted_range, &mut version_map);

        for (id, ts) in version_map.iter_mut() {
            *ts = self.version_map.get(id);
        }

        *self.deletion_map.this_mut() += 1;

        Deletion::new(start, end, version_map, self.deletion_map.this())
    }

    #[doc(hidden)]
    pub fn empty_leaves(&self) -> (usize, usize) {
        self.run_tree.count_empty_leaves()
    }

    /// Returns `true` if the given `Replica` shares the same document state as
    /// this one.
    ///
    /// This is used in tests to make sure that an encode-decode roundtrip was
    /// successful.
    #[doc(hidden)]
    pub fn eq_decoded(&self, other: &Self) -> bool {
        self.run_tree == other.run_tree && self.backlog == other.backlog
    }

    /// Encodes the `Replica` in a custom binary format.
    ///
    /// This can be used to send a `Replica` to another peer over the network.
    /// Once they have received the [`EncodedReplica`] they can decode it via
    /// the [`decode`](Replica::decode) method.
    ///
    /// Note that if you want to collaborate within a single process you can
    /// just [`fork`](Replica::fork) the `Replica` without having to encode it
    /// and decode it again.
    #[cfg(feature = "encode")]
    #[cfg_attr(docsrs, doc(cfg(feature = "encode")))]
    #[inline]
    pub fn encode(&self) -> EncodedReplica<'static> {
        EncodedReplica::from_replica(self)
    }

    /// Creates a new `Replica` with the given [`ReplicaId`] but with the same
    /// internal state as this one.
    ///
    /// Note that this method should be used when the collaborative session is
    /// limited to a single process (e.g. multiple threads working on the same
    /// document). If you want to collaborate across different processes or
    /// machines you should [`encode`](Replica::encode) the `Replica` and send
    /// the result to the other peers.
    ///
    /// # Panics
    ///
    /// Panics if the [`ReplicaId`] is zero or if it's equal to the id of this
    /// `Replica`.
    ///
    /// # Examples
    ///
    /// ```
    /// # use cola::{Replica, ReplicaId};
    /// let replica1 = Replica::new(1, 0);
    /// let replica2 = replica1.fork(2);
    /// assert_eq!(replica2.id(), 2)
    /// ```
    #[track_caller]
    #[inline]
    pub fn fork(&self, new_id: ReplicaId) -> Self {
        if new_id == 0 {
            panic::replica_id_is_zero();
        }

        if new_id == self.id {
            panic::replica_id_equal_to_forked();
        }

        Self {
            id: new_id,
            run_tree: self.run_tree.clone(),
            run_clock: RunClock::new(),
            lamport_clock: self.lamport_clock,
            version_map: self.version_map.fork(new_id, 0),
            deletion_map: self.deletion_map.fork(new_id, 0),
            backlog: self.backlog.clone(),
        }
    }

    /// Returns `true` if this `Replica` contains the given [`Anchor`]
    /// somewhere in its Gtree.
    #[inline]
    fn has_anchor(&self, anchor: InnerAnchor) -> bool {
        self.version_map.get(anchor.replica_id()) >= anchor.offset()
    }

    /// Returns `true` if this `Replica` has already merged the given
    /// `Deletion`.
    #[inline]
    fn has_merged_deletion(&self, deletion: &Deletion) -> bool {
        self.deletion_map.get(deletion.deleted_by()) >= deletion.deletion_ts()
    }

    /// Returns `true` if this `Replica` has already merged the given
    /// `Insertion`.
    #[inline]
    fn has_merged_insertion(&self, insertion: &Insertion) -> bool {
        self.version_map.get(insertion.inserted_by()) > insertion.start()
    }

    /// Returns the id of this `Replica`.
    #[inline]
    pub fn id(&self) -> ReplicaId {
        self.id
    }

    /// Informs the `Replica` that you have inserted `len` characters at the
    /// given offset.
    ///
    /// This produces an [`Insertion`] which can be sent to all the other peers
    /// to integrate the insertion into their own `Replica`s.
    ///
    /// # Panics
    ///
    /// Panics if the offset is out of bounds (i.e. greater than the current
    /// length of your buffer).
    ///
    /// # Examples
    ///
    /// ```
    /// # use cola::{Replica, Insertion};
    /// // The buffer at peer 1 is "ab".
    /// let mut replica1 = Replica::new(1, 2);
    ///
    /// // Peer 1 inserts two characters between the 'a' and the 'b'.
    /// let insertion: Insertion = replica1.inserted(1, 2);
    /// ```
    #[track_caller]
    #[must_use]
    #[inline]
    pub fn inserted(&mut self, at_offset: Length, len: Length) -> Insertion {
        if at_offset > self.len() {
            panic::offset_out_of_bounds(at_offset, self.len());
        }

        if len == 0 {
            return Insertion::no_op();
        }

        let start = self.version_map.this();

        *self.version_map.this_mut() += len;

        let end = self.version_map.this();

        let text = Text::new(self.id, start..end);

        let anchor = self.run_tree.insert(
            at_offset,
            text.clone(),
            &mut self.run_clock,
            &mut self.lamport_clock,
        );

        Insertion::new(
            anchor,
            text,
            self.lamport_clock.highest(),
            self.run_clock.last(),
        )
    }

    #[allow(clippy::len_without_is_empty)]
    #[doc(hidden)]
    pub fn len(&self) -> Length {
        self.run_tree.len()
    }

    /// Integrates a remote [`Deletion`] into this `Replica`, returning a
    /// sequence of offset [`Range`]s to be deleted from your buffer.
    ///
    /// The number of ranges can be:
    ///
    /// - zero, if the `Deletion` has already been integrated by this `Replica`
    ///   or if it depends on some context that this `Replica` doesn't yet have
    ///   (see the [`backlogged_deletions`](Replica::backlogged_deletions)
    ///   method which handles this case);
    ///
    /// - one, if there haven't been any concurrent insertions (local or
    ///   remote) within the original range of the deletion;
    ///
    /// - more than one, if there have been. In this case the deleted range has
    ///   been split into multiple smaller ranges that "skip over" the newly
    ///   inserted text.
    ///
    /// The ranges are guaranteed to be sorted in ascending order and to not
    /// overlap, i.e. for any two indices `i` and `j` where `i < j` and `j <
    /// ranges.len()` it holds that `ranges[i].end < ranges[j].start` (and of
    /// course that `ranges[i].start < ranges[i].end`).
    ///
    /// # Examples
    ///
    /// ```
    /// # use cola::Replica;
    /// // Peer 1 starts with the text "abcd" and sends it to a second peer.
    /// let mut replica1 = Replica::new(1, 4);
    ///
    /// let mut replica2 = replica1.fork(2);
    ///
    /// // Peer 1 deletes the "bc" in "abcd".
    /// let deletion = replica1.deleted(1..3);
    ///
    /// // Concurrently, peer 2 inserts a single character at start of the
    /// // document.
    /// let _ = replica2.inserted(0, 1);
    ///
    /// // Now peer 2 receives the deletion from peer 1. Since the previous
    /// // insertion was outside of the deleted region the latter is still
    /// // contiguous at this peer.
    /// let ranges = replica2.integrate_deletion(&deletion);
    ///
    /// assert_eq!(ranges.as_slice(), &[2..4]);
    /// ```
    ///
    /// ```
    /// # use cola::Replica;
    /// // Same as before..
    /// let mut replica1 = Replica::new(1, 4);
    /// let mut replica2 = replica1.fork(2);
    ///
    /// let deletion = replica1.deleted(1..3);
    ///
    /// // ..except now peer 2 inserts a single character between the 'b' and
    /// // the 'c'.
    /// let _ = replica2.inserted(2, 1);
    ///
    /// // Now peer 2 receives the deletion from peer 1. Since the previous
    /// // insertion was inside the deleted range, the latter has now been
    /// // split into two separate ranges.
    /// let ranges = replica2.integrate_deletion(&deletion);
    ///
    /// assert_eq!(ranges.as_slice(), &[1..2, 3..4]);
    /// ```
    #[must_use]
    #[inline]
    pub fn integrate_deletion(
        &mut self,
        deletion: &Deletion,
    ) -> Vec<Range<Length>> {
        if deletion.is_no_op() || self.has_merged_deletion(deletion) {
            Vec::new()
        } else if self.can_merge_deletion(deletion) {
            self.merge_unchecked_deletion(deletion)
        } else {
            self.backlog.insert_deletion(deletion.clone());
            Vec::new()
        }
    }

    /// Integrates a remote [`Insertion`] into this `Replica`, optionally
    /// returning the offset at which to insert the `Insertion`'s
    /// [`Text`](Insertion::text) into your buffer.
    ///
    /// A `None` value can be returned if the `Insertion` has already been
    /// integrated by this `Replica` or if it depends on some context that this
    /// `Replica` doesn't yet have (see the
    /// [`backlogged_insertions`](Replica::backlogged_insertions) method which
    /// handles this case).
    ///
    /// # Examples
    ///
    /// ```
    /// # use cola::{Replica, Insertion};
    /// // Peer 1 starts with the text "ab" and sends it to a second peer.
    /// let mut replica1 = Replica::new(1, 2);
    ///
    /// let mut replica2 = replica1.fork(2);
    ///
    /// // Peer 1 inserts two characters between the 'a' and the 'b'.
    /// let insertion_1 = replica1.inserted(1, 2);
    ///
    /// // Concurrently, peer 2 inserts a character at the start of the
    /// // document.
    /// let insertion_2 = replica2.inserted(0, 1);
    ///
    /// // Peer 1 receives this insertion, and since there haven't been any
    /// // concurrent insertions at the start of the document, its offset
    /// // hasn't changed.
    /// let offset_2 = replica1.integrate_insertion(&insertion_2).unwrap();
    ///
    /// assert_eq!(offset_2, 0);
    ///
    /// // If we try to integrate the same insertion again, we'll get a `None`.
    /// assert!(replica1.integrate_insertion(&insertion_2).is_none());
    ///
    /// // Finally, peer 2 receives the first insertion from peer 1. Its text
    /// // should be inserted between the 'a' and the 'b', which is at offset
    /// // 2 at this peer.
    /// let offset_1 = replica2.integrate_insertion(&insertion_1).unwrap();
    ///
    /// assert_eq!(offset_1, 2);
    /// ```
    #[must_use]
    #[inline]
    pub fn integrate_insertion(
        &mut self,
        insertion: &Insertion,
    ) -> Option<Length> {
        if insertion.is_no_op() || self.has_merged_insertion(insertion) {
            None
        } else if self.can_merge_insertion(insertion) {
            Some(self.merge_unchecked_insertion(insertion))
        } else {
            self.backlog.insert_insertion(insertion.clone());
            None
        }
    }

    /// Merges the given [`Deletion`] without checking whether it can be
    /// merged.
    #[inline]
    pub(crate) fn merge_unchecked_deletion(
        &mut self,
        deletion: &Deletion,
    ) -> Vec<Range<Length>> {
        debug_assert!(self.can_merge_deletion(deletion));

        let ranges = self.run_tree.merge_deletion(deletion);

        *self.deletion_map.get_mut(deletion.deleted_by()) =
            deletion.deletion_ts();

        ranges
    }

    /// Merges the given [`Insertion`] without checking whether it can be
    /// merged.
    #[inline]
    pub(crate) fn merge_unchecked_insertion(
        &mut self,
        insertion: &Insertion,
    ) -> Length {
        debug_assert!(self.can_merge_insertion(insertion));

        let offset = self.run_tree.merge_insertion(insertion);

        *self.version_map.get_mut(insertion.inserted_by()) += insertion.len();

        self.lamport_clock.merge(insertion.lamport_ts());

        offset
    }

    /// Creates a new `Replica` with the given [`ReplicaId`] from the initial
    /// [`Length`] of your buffer.
    ///
    /// Note that if you have multiple peers working on the same document you
    /// should only use this constructor on the first peer, usually the one
    /// that starts the collaboration session.
    ///
    /// The other peers should get their `Replica` from another `Replica`
    /// already in the session by either:
    ///
    /// a) [`fork`](Replica::fork)ing it if the collaboration happens all in
    /// the same process (e.g. a text editor with plugins running on separate
    /// threads),
    ///
    /// b) [`encode`](Replica::encode)ing it and sending the result over the
    /// network if the collaboration is between different processes or
    /// machines.
    ///
    /// # Panics
    ///
    /// Panics if the [`ReplicaId`] is zero.
    ///
    /// # Examples
    ///
    /// ```
    /// # use std::thread;
    /// # use cola::Replica;
    /// // A text editor initializes a new Replica on the main thread where the
    /// // buffer is "foo".
    /// let replica_main = Replica::new(1, 3);
    ///
    /// // It then starts a plugin on a separate thread and wants to give it a
    /// // Replica to keep its buffer synchronized with the one on the main
    /// // thread. It does *not* call `new()` again, but instead forks the
    /// // existing Replica and sends it to the new thread.
    /// let replica_plugin = replica_main.fork(2);
    ///
    /// thread::spawn(move || {
    ///     // The plugin can now use its Replica to exchange edits with the
    ///     // main thread.
    ///     println!("{replica_plugin:?}");
    /// });
    /// ```
    #[track_caller]
    #[inline]
    pub fn new(id: ReplicaId, len: Length) -> Self {
        if id == 0 {
            panic::replica_id_is_zero();
        }

        let mut run_clock = RunClock::new();

        let mut lamport_clock = LamportClock::new();

        let initial_text = Text::new(id, 0..len);

        let first_run = EditRun::new_visible(
            initial_text,
            run_clock.next(),
            lamport_clock.next(),
        );

        let run_tree = RunTree::new(first_run);

        Self {
            id,
            run_tree,
            run_clock,
            lamport_clock,
            version_map: VersionMap::new(id, len),
            deletion_map: DeletionMap::new(id, 0),
            backlog: Backlog::new(),
        }
    }

    #[doc(hidden)]
    pub fn num_runs(&self) -> usize {
        self.run_tree.count_empty_leaves().1
    }

    /// Resolves the given [`Anchor`] to an offset in the document.
    ///
    /// This method returns `None` if the `Replica` hasn't yet integrated the
    /// insertion containing the `Anchor`. In all other cases it returns
    /// `Some(offset)`, even if the position the `Anchor` refers to has been
    /// deleted.
    ///
    /// For more information, see the documentation of
    /// [`Replica::create_anchor()`] and [`Anchor`].
    ///
    /// # Examples
    ///
    /// ```
    /// # use cola::{Replica, AnchorBias};
    /// let mut peer_1 = Replica::new(1, 10);
    /// let mut peer_2 = peer_1.fork(2);
    ///
    /// let insertion_at_2 = peer_2.inserted(10, 10);
    ///
    /// let anchor = peer_2.create_anchor(15, AnchorBias::Left);
    ///
    /// // The anchor refers to the insertion made at peer 2, which peer 1
    /// // doesn't yet have.
    /// assert_eq!(peer_1.resolve_anchor(anchor), None);
    ///
    /// peer_1.integrate_insertion(&insertion_at_2);
    ///
    /// // Now that peer 1 has integrated peer 2's insertion, the anchor can be
    /// // resolved.
    /// assert_eq!(peer_1.resolve_anchor(anchor), Some(15));
    /// ```
    #[inline]
    pub fn resolve_anchor(&self, anchor: Anchor) -> Option<Length> {
        if self.has_anchor(anchor.inner()) {
            Some(self.run_tree.resolve_anchor(anchor))
        } else {
            None
        }
    }
}

impl core::fmt::Debug for Replica {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        struct DebugHexU64(u64);

        impl core::fmt::Debug for DebugHexU64 {
            fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
                write!(f, "{:x}", self.0)
            }
        }

        // In the public Debug we just print the ReplicaId to avoid leaking
        // our internals.
        //
        // During development the `Replica::debug()` method (which is public
        // but hidden from the API) can be used to obtain a more useful
        // representation.
        f.debug_tuple("Replica").field(&DebugHexU64(self.id)).finish()
    }
}

pub type LamportTs = u64;

/// A distributed logical clock used to determine if a run was in the document
/// when another run was inserted.
///
/// If it was then its [`LamportTs`] is guaranteed to be strictly less than the
/// new run's [`LamportTs`].
///
/// See [this](https://en.wikipedia.org/wiki/Lamport_timestamp) for more.
#[derive(Copy, Clone)]
pub struct LamportClock(LamportTs);

impl core::fmt::Debug for LamportClock {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        self.0.fmt(f)
    }
}

impl LamportClock {
    #[inline]
    pub fn highest(&self) -> LamportTs {
        self.0.saturating_sub(1)
    }

    #[inline]
    fn merge(&mut self, remote_ts: LamportTs) {
        if remote_ts >= self.0 {
            self.0 = remote_ts + 1;
        }
    }

    #[inline]
    fn new() -> Self {
        Self(0)
    }

    #[inline]
    pub fn next(&mut self) -> LamportTs {
        let next = self.0;
        self.0 += 1;
        next
    }
}

pub type RunTs = u64;

/// A local clock that's increased every time a new insertion run is started.
#[derive(Copy, Clone)]
pub struct RunClock(RunTs);

impl core::fmt::Debug for RunClock {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        self.0.fmt(f)
    }
}

impl RunClock {
    #[inline]
    fn last(&self) -> RunTs {
        self.0.saturating_sub(1)
    }

    #[inline]
    fn new() -> Self {
        Self(0)
    }

    #[inline]
    pub fn next(&mut self) -> RunTs {
        let next = self.0;
        self.0 += 1;
        next
    }
}

pub type DeletionTs = u64;

#[cfg(feature = "encode")]
mod encode {
    use super::*;
    use crate::backlog::encode::BacklogDecodeError;
    use crate::encode::{Decode, Encode, IntDecodeError};
    use crate::run_tree::encode::RunTreeDecodeError;
    use crate::version_map::encode::BaseMapDecodeError;

    impl Encode for LamportClock {
        #[inline(always)]
        fn encode(&self, buf: &mut Vec<u8>) {
            self.0.encode(buf)
        }
    }

    impl Decode for LamportClock {
        type Value = Self;

        type Error = IntDecodeError;

        #[inline(always)]
        fn decode(buf: &[u8]) -> Result<(Self, &[u8]), IntDecodeError> {
            LamportTs::decode(buf).map(|(ts, buf)| (Self(ts), buf))
        }
    }

    impl Encode for Replica {
        #[inline(always)]
        fn encode(&self, buf: &mut Vec<u8>) {
            self.run_tree.encode(buf);
            self.lamport_clock.encode(buf);
            self.version_map.encode(buf);
            self.deletion_map.encode(buf);
            self.backlog.encode(buf);
        }
    }

    pub(crate) enum ReplicaDecodeError {
        Backlog(BacklogDecodeError),
        DeletionMap(BaseMapDecodeError<DeletionTs>),
        Int(IntDecodeError),
        RunTree(RunTreeDecodeError),
        VersionMap(BaseMapDecodeError<Length>),
    }

    impl From<BacklogDecodeError> for ReplicaDecodeError {
        #[inline(always)]
        fn from(err: BacklogDecodeError) -> Self {
            Self::Backlog(err)
        }
    }

    impl From<BaseMapDecodeError<DeletionTs>> for ReplicaDecodeError {
        #[inline(always)]
        fn from(err: BaseMapDecodeError<DeletionTs>) -> Self {
            Self::DeletionMap(err)
        }
    }

    impl From<IntDecodeError> for ReplicaDecodeError {
        #[inline(always)]
        fn from(err: IntDecodeError) -> Self {
            Self::Int(err)
        }
    }

    impl From<RunTreeDecodeError> for ReplicaDecodeError {
        #[inline(always)]
        fn from(err: RunTreeDecodeError) -> Self {
            Self::RunTree(err)
        }
    }

    impl From<BaseMapDecodeError<Length>> for ReplicaDecodeError {
        #[inline(always)]
        fn from(err: BaseMapDecodeError<Length>) -> Self {
            Self::VersionMap(err)
        }
    }

    impl core::fmt::Display for ReplicaDecodeError {
        fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
            let err: &dyn core::fmt::Display = match self {
                Self::Backlog(err) => err,
                Self::DeletionMap(err) => err,
                Self::Int(err) => err,
                Self::RunTree(err) => err,
                Self::VersionMap(err) => err,
            };

            write!(f, "Replica: couldn't be decoded: {err}")
        }
    }

    impl Decode for Replica {
        type Value = (RunTree, LamportClock, VersionMap, DeletionMap, Backlog);

        type Error = ReplicaDecodeError;

        #[inline(always)]
        fn decode(buf: &[u8]) -> Result<(Self::Value, &[u8]), Self::Error> {
            let (run_tree, buf) = RunTree::decode(buf)?;
            let (lamport_clock, buf) = LamportClock::decode(buf)?;
            let (version_map, buf) = VersionMap::decode(buf)?;
            let (deletion_map, buf) = DeletionMap::decode(buf)?;
            let (backlog, buf) = Backlog::decode(buf)?;
            let this =
                (run_tree, lamport_clock, version_map, deletion_map, backlog);
            Ok((this, buf))
        }
    }
}

mod debug {
    use core::fmt::Debug;

    use super::*;

    pub struct DebugAsSelf<'a>(BaseDebug<'a, run_tree::DebugAsSelf<'a>>);

    impl<'a> From<&'a Replica> for DebugAsSelf<'a> {
        #[inline]
        fn from(replica: &'a Replica) -> DebugAsSelf<'a> {
            let base = BaseDebug {
                replica,
                debug_run_tree: replica.run_tree.debug_as_self(),
            };

            Self(base)
        }
    }

    impl core::fmt::Debug for DebugAsSelf<'_> {
        fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
            self.0.fmt(f)
        }
    }

    pub struct DebugAsBtree<'a>(BaseDebug<'a, run_tree::DebugAsBtree<'a>>);

    impl<'a> From<&'a Replica> for DebugAsBtree<'a> {
        #[inline]
        fn from(replica: &'a Replica) -> DebugAsBtree<'a> {
            let base = BaseDebug {
                replica,
                debug_run_tree: replica.run_tree.debug_as_btree(),
            };

            Self(base)
        }
    }

    impl core::fmt::Debug for DebugAsBtree<'_> {
        fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
            self.0.fmt(f)
        }
    }

    struct BaseDebug<'a, T: Debug> {
        replica: &'a Replica,
        debug_run_tree: T,
    }

    impl<T: Debug> Debug for BaseDebug<'_, T> {
        fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
            let replica = &self.replica;

            f.debug_struct("Replica")
                .field("id", &replica.id)
                .field("run_tree", &self.debug_run_tree)
                .field("run_indices", &replica.run_tree.run_indices())
                .field("lamport_clock", &replica.lamport_clock)
                .field("run_clock", &replica.run_clock)
                .field("version_map", &replica.version_map)
                .field("deletion_map", &replica.deletion_map)
                .field("backlog", &replica.backlog)
                .finish()
        }
    }
}
