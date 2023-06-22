use core::fmt::Debug;
use core::mem;
use core::ops::{Add, AddAssign, Range, Sub, SubAssign};

/// TODO: docs
pub trait Summary:
    Debug
    + Copy
    + Add<Self, Output = Self>
    + AddAssign<Self>
    + Sub<Self, Output = Self>
    + SubAssign<Self>
    + PartialEq<Self>
{
    type Patch: Copy;

    fn patch(old_summary: Self, new_summary: Self) -> Self::Patch;

    fn apply_patch(&mut self, patch: Self::Patch);

    fn empty() -> Self;

    fn is_empty(&self) -> bool {
        self == &Self::empty()
    }
}

/// TODO: docs
pub trait Summarize {
    type Summary: Summary;

    fn summarize(&self) -> Self::Summary;
}

/// TODO: docs
pub trait Length<S: Summary>:
    Debug
    + Copy
    + Add<Self, Output = Self>
    + AddAssign<Self>
    + Sub<Self, Output = Self>
    + SubAssign<Self>
    + Ord
{
    fn zero() -> Self;

    fn len(summary: &S) -> Self;
}

/// TODO: docs
pub trait Delete {
    fn delete(&mut self);
}

/// TODO: docs
pub trait Leaf: Clone + Debug + Summarize + Delete {
    type Length: Length<Self::Summary>;
}

/// Statically checks that `InodeIdx` and `LeafIdx` have the same size.
///
/// This invariant is required by [`Inode::children()`] to safely transmute
/// `NodeIdx` slices into `InodeIdx` and `LeafIdx` slices.
const _NODE_IDX_LAYOUT_CHECK: usize =
    (mem::size_of::<InodeIdx>() == mem::size_of::<LeafIdx>()) as usize - 1;

/// A grow-only, self-balancing tree.
///
/// TODO: describe the data structure.
#[derive(Clone)]
pub struct Gtree<const ARITY: usize, L: Leaf> {
    /// The internal nodes of the Gtree.
    inodes: Matrix<128, Inode<ARITY, L>>,

    /// The leaf nodes of the Gtree.
    lnodes: Matrix<128, Lnode<L>>,

    /// An index into `self.inodes` which points to the current root of the
    /// Gtree.
    root_idx: InodeIdx,

    /// TODO: docs
    last_insertion_cache: Option<(LeafIdx, L::Summary)>,
}

#[derive(Clone)]
struct Matrix<const COLUMNS: usize, T> {
    rows: Vec<Vec<T>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct MatrixIdx {
    row: usize,
    col: usize,
}

impl<const COLUMNS: usize, T> Matrix<COLUMNS, T> {
    #[inline]
    fn get(&self, idx: MatrixIdx) -> &T {
        unsafe { self.rows.get_unchecked(idx.row).get_unchecked(idx.col) }
    }

    #[inline]
    fn get_mut(&mut self, idx: MatrixIdx) -> &mut T {
        unsafe {
            self.rows.get_unchecked_mut(idx.row).get_unchecked_mut(idx.col)
        }
    }

    #[inline]
    fn new(value: T) -> (MatrixIdx, Self) {
        let mut first_row = Vec::with_capacity(COLUMNS);
        first_row.push(value);
        let this = Self { rows: vec![first_row] };
        let idx = MatrixIdx { row: 0, col: 0 };
        (idx, this)
    }

    #[inline]
    fn push(&mut self, value: T) -> MatrixIdx {
        if self.rows.last().unwrap().len() == COLUMNS {
            self.rows.push(Vec::with_capacity(COLUMNS));
        }

        let row = self.rows.len() - 1;
        let col = unsafe { self.rows.get_unchecked(row).len() };
        unsafe { self.rows.get_unchecked_mut(row).push(value) };
        MatrixIdx { row, col }
    }
}

/// An identifier for an internal node of the Gtree.
///
/// It can be passed to [`Gtree::inode()`] and [`Gtree::inode_mut()`] to
/// retrieve the node.
#[derive(Clone, Copy, PartialEq, Eq)]
struct InodeIdx(MatrixIdx);

impl InodeIdx {
    /// Returns a "dangling" identifier which doesn't point to any inode of
    /// the Gtree.
    ///
    /// This value is used by [`Inode`]s to:
    ///
    /// 1. Initialize their `children` field by filling the array with this
    ///    value;
    ///
    /// 2. For the root node, to indicate that it has no parent.
    #[inline]
    const fn dangling() -> Self {
        Self(MatrixIdx { row: usize::MAX, col: 0 })
    }

    /// Returns `true` if this identifier is dangling.
    ///
    /// See [`Self::dangling()`] for more information.
    #[inline]
    fn is_dangling(self) -> bool {
        self == Self::dangling()
    }
}

/// A stable identifier for a particular leaf of the Gtree.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct LeafIdx(MatrixIdx);

impl<const ARITY: usize, L: Leaf> Gtree<ARITY, L> {
    /// Asserts the invariants of the Gtree.
    ///
    /// If this method returns without panicking, then the Gtree is in a
    /// consistent state.
    pub fn assert_invariants(&self) {
        if let Some((leaf_idx, cached_summary)) = self.last_insertion_cache {
            self.assert_summary_offset_of_leaf(leaf_idx, cached_summary);
        }
    }

    /// Asserts that the summary offset of the given leaf is equal to the
    /// given summary.
    ///
    /// The summary offset of a leaf is the sum of the summaries of all the
    /// leaves that precede it in the Gtree, *not* including the summary of
    /// the leaf itself.
    fn assert_summary_offset_of_leaf(
        &self,
        leaf_idx: LeafIdx,
        leaf_offset: L::Summary,
    ) {
        let mut summary = L::Summary::empty();

        let mut parent_idx = self.lnode(leaf_idx).parent();

        let parent = self.inode(parent_idx);
        let child_idx = parent.idx_of_leaf_child(leaf_idx);
        summary += self.summary_offset_of_child(parent_idx, child_idx);
        let mut inode_idx = parent_idx;
        parent_idx = parent.parent();

        while !parent_idx.is_dangling() {
            let parent = self.inode(parent_idx);
            let child_idx = parent.idx_of_internal_child(inode_idx);
            summary += self.summary_offset_of_child(parent_idx, child_idx);
            inode_idx = parent_idx;
            parent_idx = parent.parent();
        }

        assert_eq!(summary, leaf_offset);
    }

    /// Returns the index and summary offsets of the inode's child at the given
    /// length offset.
    ///
    /// If the length offset falls between two children, then the infos of the
    /// child that follows the length offset are returned.
    #[inline]
    fn child_at_offset(
        &self,
        of_inode: InodeIdx,
        at_offset: L::Length,
    ) -> (usize, L::Summary) {
        debug_assert!(
            at_offset <= L::Length::len(&self.inode(of_inode).summary())
        );

        let mut offset = L::Summary::empty();

        match self.inode(of_inode).children() {
            Either::Internal(inode_idxs) => {
                for (idx, &inode_idx) in inode_idxs.iter().enumerate() {
                    let summary = self.inode(inode_idx).summary();
                    offset += summary;
                    if L::Length::len(&offset) >= at_offset {
                        return (idx, offset - summary);
                    }
                }
            },

            Either::Leaf(leaf_idxs) => {
                for (idx, &leaf_idx) in leaf_idxs.iter().enumerate() {
                    let summary = self.leaf(leaf_idx).summarize();
                    offset += summary;
                    if L::Length::len(&offset) >= at_offset {
                        return (idx, offset - summary);
                    }
                }
            },
        }

        unreachable!();
    }

    /// Returns the index and length offsets of the inode's child that fully
    /// contains the given length range, or `None` if no such child exists.
    #[inline]
    fn child_containing_range(
        &self,
        of_inode: InodeIdx,
        range: Range<L::Length>,
    ) -> Option<(usize, L::Length)> {
        #[inline(always)]
        fn measure<L, I>(
            iter: I,
            range: Range<L::Length>,
        ) -> Option<(usize, L::Length)>
        where
            L: Leaf,
            I: Iterator<Item = L::Length>,
        {
            let mut offset = L::Length::zero();
            for (idx, child_len) in iter.enumerate() {
                offset += child_len;
                if offset > range.start {
                    return if offset >= range.end {
                        Some((idx, offset - child_len))
                    } else {
                        None
                    };
                }
            }
            unreachable!();
        }

        match self.inode(of_inode).children() {
            Either::Internal(inode_idxs) => {
                let iter = inode_idxs.iter().copied().map(|idx| {
                    let inode = self.inode(idx);
                    L::Length::len(&inode.summary())
                });

                measure::<L, _>(iter, range)
            },

            Either::Leaf(leaf_idxs) => {
                let iter = leaf_idxs.iter().copied().map(|idx| {
                    let leaf = self.leaf(idx);
                    L::Length::len(&leaf.summarize())
                });

                measure::<L, _>(iter, range)
            },
        }
    }

    /// Returns the length of the inode's child at the given index.
    #[inline]
    fn child_measure(
        &self,
        of_inode: InodeIdx,
        child_idx: usize,
    ) -> L::Length {
        match self.inode(of_inode).child(child_idx) {
            Either::Internal(inode_idx) => {
                L::Length::len(&self.inode(inode_idx).summary())
            },

            Either::Leaf(leaf_idx) => {
                L::Length::len(&self.leaf(leaf_idx).summarize())
            },
        }
    }

    #[doc(hidden)]
    pub fn debug_as_btree(&self) -> debug::DebugAsBtree<'_, ARITY, L> {
        self.debug_inode_as_btree(self.root_idx)
    }

    #[doc(hidden)]
    pub fn debug_as_self(&self) -> debug::DebugAsSelf<'_, ARITY, L> {
        debug::DebugAsSelf(self)
    }

    fn debug_inode_as_btree(
        &self,
        inode_idx: InodeIdx,
    ) -> debug::DebugAsBtree<'_, ARITY, L> {
        debug::DebugAsBtree { gtree: self, inode_idx }
    }

    #[inline]
    fn delete_child(&mut self, of_inode: InodeIdx, child_idx: usize) {
        let child_summary = match self.inode(of_inode).child(child_idx) {
            Either::Internal(inode_idx) => {
                let inode = self.inode_mut(inode_idx);
                mem::replace(inode.summary_mut(), L::Summary::empty())
            },
            Either::Leaf(leaf_idx) => {
                let leaf = self.leaf_mut(leaf_idx);
                let old_summary = leaf.summarize();
                leaf.delete();
                old_summary
            },
        };

        *self.inode_mut(of_inode).summary_mut() -= child_summary;
    }

    /// TODO: docs
    #[inline]
    pub fn delete_range<DelRange, DelFrom, DelUpTo>(
        &mut self,
        range: Range<L::Length>,
        delete_range: DelRange,
        delete_from: DelFrom,
        delete_up_to: DelUpTo,
    ) -> (Option<LeafIdx>, Option<LeafIdx>)
    where
        DelRange: FnOnce(&mut L, Range<L::Length>) -> (Option<L>, Option<L>),
        DelFrom: FnOnce(&mut L, L::Length) -> Option<L>,
        DelUpTo: FnOnce(&mut L, L::Length) -> Option<L>,
    {
        let (idxs, maybe_split) = delete::delete_range(
            self,
            self.root_idx,
            range,
            delete_range,
            delete_from,
            delete_up_to,
        );

        if let Some(root_split) = maybe_split {
            self.root_has_split(root_split);
        }

        self.last_insertion_cache = None;

        idxs
    }

    #[inline(always)]
    fn inode(&self, idx: InodeIdx) -> &Inode<ARITY, L> {
        self.inodes.get(idx.0)
    }

    #[inline]
    fn inode_mut(&mut self, idx: InodeIdx) -> &mut Inode<ARITY, L> {
        self.inodes.get_mut(idx.0)
    }

    /// TODO: docs
    pub fn insert<F>(
        &mut self,
        offset: L::Length,
        insert_with: F,
    ) -> (Option<LeafIdx>, Option<LeafIdx>)
    where
        F: FnOnce(&mut L, L::Length) -> (Option<L>, Option<L>),
    {
        if let Some((leaf_idx, leaf_offset)) = self.last_insertion_cache {
            let m_offset = L::Length::len(&leaf_offset);

            let leaf_measure =
                L::Length::len(&self.leaf(leaf_idx).summarize());

            if offset > m_offset && offset <= m_offset + leaf_measure {
                self.insert_at_leaf(leaf_idx, leaf_offset, offset, insert_with)
            } else {
                self.insert_at_offset(offset, insert_with)
            }
        } else {
            self.insert_at_offset(offset, insert_with)
        }
    }

    /// TODO: docs
    fn insert_at_leaf<F>(
        &mut self,
        leaf_idx: LeafIdx,
        leaf_offset: L::Summary,
        provided_offset: L::Length,
        insert_with: F,
    ) -> (Option<LeafIdx>, Option<LeafIdx>)
    where
        F: FnOnce(&mut L, L::Length) -> (Option<L>, Option<L>),
    {
        let lnode = self.lnode_mut(leaf_idx);

        let old_leaf = lnode.value().clone();

        let old_summary = old_leaf.summarize();

        let (first, second) = insert_with(
            lnode.value_mut(),
            provided_offset - L::Length::len(&leaf_offset),
        );

        if first.is_some() {
            let new_leaf = mem::replace(lnode.value_mut(), old_leaf);

            self.last_insertion_cache = None;

            return self.insert_at_offset(provided_offset, |old_leaf, _| {
                *old_leaf = new_leaf;
                (first, second)
            });
        }

        let new_summary = lnode.value().summarize();

        let summary_patch = L::Summary::patch(old_summary, new_summary);

        let mut parent = lnode.parent();

        while !parent.is_dangling() {
            let inode = self.inode_mut(parent);
            inode.summary_mut().apply_patch(summary_patch);
            parent = inode.parent();
        }

        (None, None)
    }

    /// TODO: docs
    fn insert_at_offset<F>(
        &mut self,
        offset: L::Length,
        insert_with: F,
    ) -> (Option<LeafIdx>, Option<LeafIdx>)
    where
        F: FnOnce(&mut L, L::Length) -> (Option<L>, Option<L>),
    {
        let (idxs, maybe_split) = insert::insert_at_offset(
            self,
            self.root_idx,
            L::Summary::empty(),
            offset,
            insert_with,
        );

        if let Some(root_split) = maybe_split {
            self.root_has_split(root_split);
        }

        idxs
    }

    /// TODO: docs
    #[inline]
    fn insert_in_inode(
        &mut self,
        idx: InodeIdx,
        at_offset: usize,
        child: NodeIdx,
        child_summary: L::Summary,
    ) -> Option<Inode<ARITY, L>> {
        let min_children = Inode::<ARITY, L>::min_children();

        let inode = self.inode_mut(idx);

        debug_assert!(at_offset <= inode.len());

        if inode.is_full() {
            let split_offset = inode.len() - min_children;

            // Split so that the extra inode always has the minimum number
            // of children.
            let rest = if at_offset <= min_children {
                let rest = self.split_inode(idx, split_offset);

                self.inode_mut(idx).insert(at_offset, child, child_summary);
                rest
            } else {
                let mut rest = self.split_inode(idx, split_offset + 1);
                rest.insert(
                    at_offset - self.inode(idx).len(),
                    child,
                    child_summary,
                );
                rest
            };

            debug_assert_eq!(rest.len(), min_children);

            Some(rest)
        } else {
            inode.insert(at_offset, child, child_summary);
            None
        }
    }

    /// TODO: docs
    #[allow(clippy::too_many_arguments)]
    #[inline]
    fn insert_two_in_inode(
        &mut self,
        idx: InodeIdx,
        mut first_offset: usize,
        mut first_child: NodeIdx,
        mut first_summary: L::Summary,
        mut second_offset: usize,
        mut second_child: NodeIdx,
        mut second_summary: L::Summary,
    ) -> Option<Inode<ARITY, L>> {
        use core::cmp::Ordering;

        let min_children = Inode::<ARITY, L>::min_children();

        debug_assert!(min_children >= 2);

        if first_offset > second_offset {
            (
                first_child,
                second_child,
                first_offset,
                second_offset,
                first_summary,
                second_summary,
            ) = (
                second_child,
                first_child,
                second_offset,
                first_offset,
                second_summary,
                first_summary,
            )
        }

        let max_children = Inode::<ARITY, L>::max_children();

        let inode = self.inode_mut(idx);

        let len = inode.len();

        debug_assert!(second_offset <= inode.len());

        if max_children - len < 2 {
            let split_offset = len - min_children;

            let children_after_second = len - second_offset;

            // Split so that the extra inode always has the minimum number
            // of children.
            //
            // The logic to make this work is a bit annoying to reason
            // about. We should probably add some unit tests to avoid
            // possible regressions.
            let rest = match children_after_second.cmp(&(min_children - 1)) {
                Ordering::Greater => {
                    let rest = self.split_inode(idx, split_offset);
                    self.insert_two_in_inode(
                        idx,
                        first_offset,
                        first_child,
                        first_summary,
                        second_offset,
                        second_child,
                        second_summary,
                    );
                    rest
                },

                Ordering::Less if first_offset >= split_offset + 2 => {
                    let mut rest = self.split_inode(idx, split_offset + 2);
                    let new_len = self.inode(idx).len();
                    first_offset -= new_len;
                    second_offset -= new_len;
                    rest.insert_two(
                        first_offset,
                        first_child,
                        first_summary,
                        second_offset,
                        second_child,
                        second_summary,
                    );
                    rest
                },

                _ => {
                    let mut rest = self.split_inode(idx, split_offset + 1);
                    let new_len = self.inode(idx).len();
                    rest.insert(
                        second_offset - new_len,
                        second_child,
                        second_summary,
                    );
                    self.inode_mut(idx).insert(
                        first_offset,
                        first_child,
                        first_summary,
                    );
                    rest
                },
            };

            debug_assert_eq!(rest.len(), min_children);

            Some(rest)
        } else {
            inode.insert_two(
                first_offset,
                first_child,
                first_summary,
                second_offset,
                second_child,
                second_summary,
            );
            None
        }
    }

    #[inline(always)]
    fn leaf(&self, idx: LeafIdx) -> &L {
        self.lnode(idx).value()
    }

    #[inline(always)]
    fn leaf_mut(&mut self, idx: LeafIdx) -> &mut L {
        self.lnode_mut(idx).value_mut()
    }

    #[inline]
    fn lnode(&self, idx: LeafIdx) -> &Lnode<L> {
        self.lnodes.get(idx.0)
    }

    #[inline]
    fn lnode_mut(&mut self, idx: LeafIdx) -> &mut Lnode<L> {
        self.lnodes.get_mut(idx.0)
    }

    /// Creates a new Gtree with the given leaf as its first leaf.
    #[inline]
    pub fn new(first_leaf: L) -> Self {
        let summary = first_leaf.summarize();

        let leaf_idx = LeafIdx(MatrixIdx { row: 0, col: 0 });

        let root = Inode::from_leaf(leaf_idx, summary, InodeIdx::dangling());
        let (root_idx, inodes) = Matrix::new(root);
        let root_idx = InodeIdx(root_idx);

        let lnode = Lnode::new(first_leaf, root_idx);
        let (_, lnodes) = Matrix::new(lnode);

        Self { inodes, lnodes, root_idx, last_insertion_cache: None }
    }

    /// Pushes an inode to the Gtree, returning its index.
    ///
    /// Note that the `parent` field of the given inode should be updated
    /// before calling this method.
    #[inline(always)]
    fn push_inode(&mut self, inode: Inode<ARITY, L>) -> InodeIdx {
        let idx = InodeIdx(self.inodes.push(inode));

        assert!(!idx.is_dangling());

        let children = self.inode(idx).children();

        // Transmute the children into the same type to get around Rust's
        // aliasing rules.
        let children = unsafe {
            mem::transmute::<_, Either<&[InodeIdx], &[LeafIdx]>>(children)
        };

        match children {
            Either::Internal(internal_idxs) => {
                for &InodeIdx(child_idx) in internal_idxs {
                    let child_inode = self.inodes.get_mut(child_idx);
                    *child_inode.parent_mut() = idx;
                }
            },

            Either::Leaf(leaf_idxs) => {
                for &LeafIdx(child_idx) in leaf_idxs {
                    let child_lnode = self.lnodes.get_mut(child_idx);
                    *child_lnode.parent_mut() = idx;
                }
            },
        }

        idx
    }

    /// Pushes a leaf node to the Gtree, returning its index.
    #[inline]
    fn push_leaf(&mut self, leaf: L, parent: InodeIdx) -> LeafIdx {
        let lnode = Lnode::new(leaf, parent);
        LeafIdx(self.lnodes.push(lnode))
    }

    #[inline]
    fn root(&self) -> &Inode<ARITY, L> {
        self.inode(self.root_idx)
    }

    /// Called when the root has split into two inodes and a new root
    /// needs to be created.
    #[inline]
    fn root_has_split(&mut self, root_split: Inode<ARITY, L>) {
        let split_summary = root_split.summary();

        let split_idx = self.push_inode(root_split);

        let new_root = Inode::from_two_internals(
            self.root_idx,
            split_idx,
            self.summary(),
            split_summary,
            InodeIdx::dangling(),
        );

        let new_root_idx = self.push_inode(new_root);

        *self.root_mut().parent_mut() = new_root_idx;
        *self.inode_mut(split_idx).parent_mut() = new_root_idx;

        self.root_idx = new_root_idx;
    }

    #[inline]
    fn root_mut(&mut self) -> &mut Inode<ARITY, L> {
        self.inode_mut(self.root_idx)
    }

    #[inline]
    fn split_inode(
        &mut self,
        inode_idx: InodeIdx,
        at_offset: usize,
    ) -> Inode<ARITY, L> {
        let summary = self.summary_offset_of_child(inode_idx, at_offset);
        self.inode_mut(inode_idx).split(at_offset, summary)
    }

    #[inline]
    pub fn summary(&self) -> L::Summary {
        self.root().summary()
    }

    #[inline]
    fn summary_offset_of_child(
        &self,
        in_inode: InodeIdx,
        child_offset: usize,
    ) -> L::Summary {
        let mut summary = L::Summary::empty();

        match self.inode(in_inode).children() {
            Either::Internal(inode_idxs) => {
                for &idx in &inode_idxs[..child_offset] {
                    summary += self.inode(idx).summary();
                }
            },

            Either::Leaf(leaf_idxs) => {
                for &idx in &leaf_idxs[..child_offset] {
                    summary += self.leaf(idx).summarize();
                }
            },
        }

        summary
    }

    /// TODO: docs
    #[inline]
    fn with_internal_mut<F, R>(&mut self, inode_idx: InodeIdx, fun: F) -> R
    where
        F: FnOnce(&mut Self) -> R,
    {
        debug_assert!(inode_idx != self.root_idx);

        let old_summary = self.inode(inode_idx).summary();

        let ret = fun(self);

        let inode = self.inode(inode_idx);

        let new_summary = inode.summary();

        let parent_idx = inode.parent();

        debug_assert!(!parent_idx.is_dangling());

        let parent = self.inode_mut(parent_idx);
        *parent.summary_mut() -= old_summary;
        *parent.summary_mut() += new_summary;

        ret
    }

    /// TODO: docs
    #[inline]
    fn with_internal_mut_handle_split<F, R>(
        &mut self,
        inode_idx: InodeIdx,
        idx_in_parent: usize,
        fun: F,
    ) -> (R, Option<Inode<ARITY, L>>)
    where
        F: FnOnce(&mut Self) -> (R, Option<Inode<ARITY, L>>),
    {
        let (ret, maybe_split) = self.with_internal_mut(inode_idx, fun);

        let split = maybe_split.and_then(|mut split| {
            let parent_idx = self.inode(inode_idx).parent();
            *split.parent_mut() = parent_idx;
            let summary = split.summary();
            let split_idx = self.push_inode(split);
            self.insert_in_inode(
                parent_idx,
                idx_in_parent + 1,
                NodeIdx::from_internal(split_idx),
                summary,
            )
        });

        (ret, split)
    }

    /// TODO: docs
    #[inline]
    fn with_leaf_mut<F, T>(&mut self, leaf_idx: LeafIdx, fun: F) -> T
    where
        F: FnOnce(&mut L) -> T,
    {
        let lnode = self.lnode_mut(leaf_idx);

        let leaf = lnode.value_mut();

        let old_summary = leaf.summarize();

        let ret = fun(leaf);

        let new_summary = leaf.summarize();

        let parent_idx = lnode.parent();

        let parent = self.inode_mut(parent_idx);
        *parent.summary_mut() -= old_summary;
        *parent.summary_mut() += new_summary;

        ret
    }

    /// TODO: docs
    #[inline]
    fn with_leaf_mut_handle_split<F>(
        &mut self,
        leaf_idx: LeafIdx,
        idx_in_parent: usize,
        fun: F,
    ) -> (Option<LeafIdx>, Option<LeafIdx>, Option<Inode<ARITY, L>>)
    where
        F: FnOnce(&mut L) -> (Option<L>, Option<L>),
    {
        let parent_idx = self.lnode(leaf_idx).parent();

        let (left, right) = self.with_leaf_mut(leaf_idx, fun);

        let insert_at = idx_in_parent + 1;

        match (left, right) {
            (Some(left), Some(right)) => {
                let left_summary = left.summarize();
                let left_idx = self.push_leaf(left, parent_idx);

                let right_summary = right.summarize();
                let right_idx = self.push_leaf(right, parent_idx);

                let split = self.insert_two_in_inode(
                    parent_idx,
                    insert_at,
                    NodeIdx::from_leaf(left_idx),
                    left_summary,
                    insert_at,
                    NodeIdx::from_leaf(right_idx),
                    right_summary,
                );

                (Some(left_idx), Some(right_idx), split)
            },

            (Some(left), None) => {
                let summary = left.summarize();
                let left_idx = self.push_leaf(left, parent_idx);

                let split = self.insert_in_inode(
                    parent_idx,
                    insert_at,
                    NodeIdx::from_leaf(left_idx),
                    summary,
                );

                (Some(left_idx), None, split)
            },

            _ => (None, None, None),
        }
    }
}

/// An internal node of the Gtree.
#[derive(Clone)]
struct Inode<const ARITY: usize, L: Leaf> {
    /// The total summary of this node, which is the sum of the summaries of
    /// all of its children.
    summary: L::Summary,

    /// The index of this node's parent, or `InodeIdx::dangling()` if this is
    /// the root node.
    parent: InodeIdx,

    /// The number of children this inode is storing, which is always less than
    /// or equal to `ARITY`.
    len: usize,

    /// The indexes of this node's children in the Gtree. The first `len` of
    /// these are valid, and the rest are dangling.
    children: [NodeIdx; ARITY],

    /// Whether `children` contains `LeafIdx`s or `InodeIdx`s.
    has_leaves: bool,
}

/// An index to either an internal node or a leaf node of the Gtree.
///
/// We use a union here to save space, since we know that an inode can only
/// store either internal nodes or leaf nodes, but not both.
///
/// This means that we can use a single boolean (the `has_leaves` field of
/// `Inode`) instead of storing the same tag for every single child of the
/// inode, like we would have to do if we used an enum.
#[derive(Clone, Copy)]
union NodeIdx {
    internal: InodeIdx,
    leaf: LeafIdx,
}

impl NodeIdx {
    #[inline]
    const fn dangling() -> Self {
        Self { internal: InodeIdx::dangling() }
    }

    #[inline]
    const fn from_internal(internal_idx: InodeIdx) -> Self {
        Self { internal: internal_idx }
    }

    #[inline]
    const fn from_leaf(leaf_idx: LeafIdx) -> Self {
        Self { leaf: leaf_idx }
    }
}

enum Either<I, L> {
    Internal(I),
    Leaf(L),
}

impl<const ARITY: usize, L: Leaf> Inode<ARITY, L> {
    #[inline]
    fn child(&self, child_idx: usize) -> Either<InodeIdx, LeafIdx> {
        let child = self.children[child_idx];

        if self.has_leaves {
            Either::Leaf(unsafe { child.leaf })
        } else {
            Either::Internal(unsafe { child.internal })
        }
    }

    #[inline]
    fn children(&self) -> Either<&[InodeIdx], &[LeafIdx]> {
        let children = &self.children[..self.len];

        // SAFETY: `LeafIdx` and `InodeIdx` have the same layout, so the
        // `NodeIdx` union also has the same layout and we can safely
        // transmute it into either of them.

        if self.has_leaves {
            let leaves = unsafe { mem::transmute::<_, &[LeafIdx]>(children) };
            Either::Leaf(leaves)
        } else {
            let inodes = unsafe { mem::transmute::<_, &[InodeIdx]>(children) };
            Either::Internal(inodes)
        }
    }

    /// Creates a new internal node containing the given leaf.
    #[inline]
    fn from_leaf(
        leaf: LeafIdx,
        summary: L::Summary,
        parent: InodeIdx,
    ) -> Self {
        let mut children = [NodeIdx::dangling(); ARITY];
        children[0] = NodeIdx::from_leaf(leaf);
        Self { children, parent, summary, has_leaves: true, len: 1 }
    }

    /// Creates a new internal node containing the two given inodes.
    #[inline]
    fn from_two_internals(
        first: InodeIdx,
        second: InodeIdx,
        first_summary: L::Summary,
        second_summary: L::Summary,
        parent: InodeIdx,
    ) -> Self {
        let mut children = [NodeIdx::dangling(); ARITY];
        children[0] = NodeIdx::from_internal(first);
        children[1] = NodeIdx::from_internal(second);

        Self {
            children,
            parent,
            summary: first_summary + second_summary,
            has_leaves: false,
            len: 2,
        }
    }

    /// Returns the index of the child matching the given `InodeIdx`.
    ///
    /// Panics if this inode contains leaf nodes or if it doesn't contain the
    /// given `InodeIdx`.
    fn idx_of_internal_child(&self, inode_idx: InodeIdx) -> usize {
        let Either::Internal(idxs) = self.children() else {
            panic!("this inode contains leaf nodes");
        };

        idxs.iter()
            .enumerate()
            .find_map(|(i, &idx)| (idx == inode_idx).then_some(i))
            .expect("this inode does not contain the given inode idx")
    }

    /// Returns the index of the child matching the given `LeafIdx`.
    ///
    /// Panics if this inode contains inodes or if it doesn't contain the given
    /// `LeafIdx`.
    fn idx_of_leaf_child(&self, leaf_idx: LeafIdx) -> usize {
        let Either::Leaf(idxs) = self.children() else {
                panic!("this inode contains other inodes");
            };

        idxs.iter()
            .enumerate()
            .find_map(|(i, &idx)| (idx == leaf_idx).then_some(i))
            .expect("this inode does not contain the given leaf idx")
    }

    /// Inserts two children into this inode at the given offsets.
    ///
    /// Panics if this doesn't have enough space to store two more children, if
    /// the second offset is less than the first, or if one of the offsets is
    /// out of bounds.
    #[inline]
    fn insert_two(
        &mut self,
        first_offset: usize,
        first_child: NodeIdx,
        first_summary: L::Summary,
        second_offset: usize,
        second_child: NodeIdx,
        second_summary: L::Summary,
    ) {
        debug_assert!(first_offset <= second_offset);
        debug_assert!(second_offset <= self.len());
        debug_assert!(Self::max_children() - self.len() >= 2);

        self.insert(first_offset, first_child, first_summary);
        self.insert(second_offset + 1, second_child, second_summary);
    }

    /// Inserts a child into this inode at the given offset.
    ///
    /// Panics if this inode is already full or if the offset is out of bounds.
    #[inline]
    fn insert(
        &mut self,
        at_offset: usize,
        child: NodeIdx,
        child_summary: L::Summary,
    ) {
        debug_assert!(at_offset <= self.len());
        debug_assert!(!self.is_full());

        self.children[at_offset..].rotate_right(1);
        self.children[at_offset] = child;
        self.summary += child_summary;
        self.len += 1;
    }

    #[inline]
    fn is_full(&self) -> bool {
        self.len() == Self::max_children()
    }

    #[inline(always)]
    fn len(&self) -> usize {
        self.len
    }

    #[inline]
    const fn max_children() -> usize {
        ARITY
    }

    #[inline]
    const fn min_children() -> usize {
        ARITY / 2
    }

    #[inline(always)]
    fn parent(&self) -> InodeIdx {
        self.parent
    }

    #[inline(always)]
    fn parent_mut(&mut self) -> &mut InodeIdx {
        &mut self.parent
    }

    /// Split this inode at the given offset, returning the right side of the
    /// split.
    ///
    /// The `new_summary` parameter is the sum of the summaries of the first
    /// `at_offset` children.
    #[inline]
    fn split(&mut self, at_offset: usize, new_summary: L::Summary) -> Self {
        let len = self.len() - at_offset;

        let mut children = [NodeIdx::dangling(); ARITY];
        children[..len].copy_from_slice(&self.children[at_offset..self.len()]);

        let summary = self.summary - new_summary;

        self.summary = new_summary;

        self.len = at_offset;

        Self {
            children,
            has_leaves: self.has_leaves,
            len,
            parent: InodeIdx::dangling(),
            summary,
        }
    }

    #[inline(always)]
    fn summary(&self) -> L::Summary {
        self.summary
    }

    #[inline(always)]
    fn summary_mut(&mut self) -> &mut L::Summary {
        &mut self.summary
    }
}

/// A leaf node of the Gtree.
#[derive(Clone)]
struct Lnode<Leaf> {
    /// The value of this leaf node.
    value: Leaf,

    /// The index of this leaf node's parent.
    parent: InodeIdx,
}

impl<L: Leaf> Lnode<L> {
    #[inline(always)]
    fn new(value: L, parent: InodeIdx) -> Self {
        Self { value, parent }
    }

    #[inline(always)]
    fn parent(&self) -> InodeIdx {
        self.parent
    }

    #[inline(always)]
    fn parent_mut(&mut self) -> &mut InodeIdx {
        &mut self.parent
    }

    #[inline(always)]
    fn value(&self) -> &L {
        &self.value
    }

    #[inline(always)]
    fn value_mut(&mut self) -> &mut L {
        &mut self.value
    }
}

mod insert {
    //! TODO: docs.

    use super::*;

    #[allow(clippy::type_complexity)]
    pub(super) fn insert_at_offset<const N: usize, L, F>(
        gtree: &mut Gtree<N, L>,
        in_inode: InodeIdx,
        mut leaf_offset: L::Summary,
        at_offset: L::Length,
        insert_with: F,
    ) -> ((Option<LeafIdx>, Option<LeafIdx>), Option<Inode<N, L>>)
    where
        L: Leaf,
        F: FnOnce(&mut L, L::Length) -> (Option<L>, Option<L>),
    {
        let (child_idx, offset) = gtree.child_at_offset(
            in_inode,
            at_offset - L::Length::len(&leaf_offset),
        );

        leaf_offset += offset;

        match gtree.inode(in_inode).child(child_idx) {
            Either::Internal(next_idx) => gtree
                .with_internal_mut_handle_split(
                    next_idx,
                    child_idx,
                    |gtree| {
                        insert_at_offset(
                            gtree,
                            next_idx,
                            leaf_offset,
                            at_offset,
                            insert_with,
                        )
                    },
                ),

            Either::Leaf(leaf_idx) => {
                let (inserted_idx, split_idx, split) = gtree
                    .with_leaf_mut_handle_split(leaf_idx, child_idx, |leaf| {
                        insert_with(
                            leaf,
                            at_offset - L::Length::len(&leaf_offset),
                        )
                    });

                gtree.last_insertion_cache = if at_offset == L::Length::zero()
                {
                    None
                } else if let Some(idx) = inserted_idx {
                    let leaf_summary = gtree.leaf(leaf_idx).summarize();
                    Some((idx, leaf_offset + leaf_summary))
                } else {
                    Some((leaf_idx, leaf_offset))
                };

                ((inserted_idx, split_idx), split)
            },
        }
    }
}

mod delete {
    //! TODO: docs.

    use super::*;

    #[allow(clippy::type_complexity)]
    pub(super) fn delete_range<const N: usize, L, DelRange, DelFrom, DelUpTo>(
        gtree: &mut Gtree<N, L>,
        in_inode: InodeIdx,
        mut range: Range<L::Length>,
        del_range: DelRange,
        del_from: DelFrom,
        del_up_to: DelUpTo,
    ) -> ((Option<LeafIdx>, Option<LeafIdx>), Option<Inode<N, L>>)
    where
        L: Leaf,
        DelRange: FnOnce(&mut L, Range<L::Length>) -> (Option<L>, Option<L>),
        DelFrom: FnOnce(&mut L, L::Length) -> Option<L>,
        DelUpTo: FnOnce(&mut L, L::Length) -> Option<L>,
    {
        let Some((child_idx, offset)) =
            gtree.child_containing_range(in_inode, range.clone()) else {
                return delete_range_in_inode(
                    gtree,
                    in_inode,
                    range,
                    del_from,
                    del_up_to,
                );
            };

        range.start -= offset;

        range.end -= offset;

        match gtree.inode(in_inode).child(child_idx) {
            Either::Internal(next_idx) => gtree
                .with_internal_mut_handle_split(
                    next_idx,
                    child_idx,
                    |gtree| {
                        delete_range(
                            gtree, next_idx, range, del_range, del_from,
                            del_up_to,
                        )
                    },
                ),

            Either::Leaf(leaf_idx) => {
                let (first_idx, second_idx, split) = gtree
                    .with_leaf_mut_handle_split(leaf_idx, child_idx, |leaf| {
                        del_range(leaf, range)
                    });

                ((first_idx, second_idx), split)
            },
        }
    }

    #[allow(clippy::type_complexity)]
    fn delete_range_in_inode<const N: usize, L, DelFrom, DelUpTo>(
        gtree: &mut Gtree<N, L>,
        idx: InodeIdx,
        range: Range<L::Length>,
        del_from: DelFrom,
        del_up_to: DelUpTo,
    ) -> ((Option<LeafIdx>, Option<LeafIdx>), Option<Inode<N, L>>)
    where
        L: Leaf,
        DelFrom: FnOnce(&mut L, L::Length) -> Option<L>,
        DelUpTo: FnOnce(&mut L, L::Length) -> Option<L>,
    {
        let mut idx_start = 0;
        let mut leaf_idx_start = None;
        let mut extra_from_start = None;

        let mut idx_end = 0;
        let mut leaf_idx_end = None;
        let mut extra_from_end = None;

        let mut idxs = 0..gtree.inode(idx).len();
        let mut offset = L::Length::zero();

        for child_idx in idxs.by_ref() {
            let child_measure = gtree.child_measure(idx, child_idx);

            offset += child_measure;

            if offset > range.start {
                offset -= child_measure;

                idx_start = child_idx;

                match gtree.inode(idx).child(child_idx) {
                    Either::Internal(inode_idx) => {
                        let (leaf_idx, split) =
                            gtree.with_internal_mut(inode_idx, |gtree| {
                                delete_from(
                                    gtree,
                                    inode_idx,
                                    range.start - offset,
                                    del_from,
                                )
                            });

                        leaf_idx_start = leaf_idx;

                        if let Some(mut extra) = split {
                            *extra.parent_mut() = idx;
                            let summary = extra.summary();
                            let inode_idx = gtree.push_inode(extra);
                            let node_idx = NodeIdx::from_internal(inode_idx);
                            extra_from_start = Some((node_idx, summary));
                        }
                    },

                    Either::Leaf(leaf_idx) => {
                        let split = gtree.with_leaf_mut(leaf_idx, |leaf| {
                            del_from(leaf, range.start - offset)
                        });

                        if let Some(split) = split {
                            let summary = split.summarize();
                            let leaf_idx = gtree.push_leaf(split, idx);
                            leaf_idx_start = Some(leaf_idx);
                            let node_idx = NodeIdx::from_leaf(leaf_idx);
                            extra_from_start = Some((node_idx, summary));
                        }
                    },
                }

                offset += child_measure;

                break;
            }
        }

        for child_idx in idxs {
            let child_measure = gtree.child_measure(idx, child_idx);

            offset += child_measure;

            if offset >= range.end {
                offset -= child_measure;

                idx_end = child_idx;

                match gtree.inode(idx).child(child_idx) {
                    Either::Internal(inode_idx) => {
                        let (leaf_idx, split) =
                            gtree.with_internal_mut(inode_idx, |gtree| {
                                delete_up_to(
                                    gtree,
                                    inode_idx,
                                    range.end - offset,
                                    del_up_to,
                                )
                            });

                        leaf_idx_end = leaf_idx;

                        if let Some(mut extra) = split {
                            *extra.parent_mut() = idx;
                            let summary = extra.summary();
                            let inode_idx = gtree.push_inode(extra);
                            let node_idx = NodeIdx::from_internal(inode_idx);
                            extra_from_end = Some((node_idx, summary));
                        }
                    },

                    Either::Leaf(leaf_idx) => {
                        let split = gtree.with_leaf_mut(leaf_idx, |leaf| {
                            del_up_to(leaf, range.end - offset)
                        });

                        if let Some(split) = split {
                            let summary = split.summarize();
                            let leaf_idx = gtree.push_leaf(split, idx);

                            leaf_idx_end = Some(leaf_idx);

                            let node_idx = NodeIdx::from_leaf(leaf_idx);
                            extra_from_end = Some((node_idx, summary));
                        }
                    },
                }

                break;
            } else {
                gtree.delete_child(idx, child_idx);
            }
        }

        let start_offset = idx_start + 1;

        let end_offset = idx_end + 1;

        let split = match (extra_from_start, extra_from_end) {
            (Some((start, start_summary)), Some((end, end_summary))) => gtree
                .insert_two_in_inode(
                    idx,
                    start_offset,
                    start,
                    start_summary,
                    end_offset,
                    end,
                    end_summary,
                ),

            (Some((start, summary)), None) => {
                gtree.insert_in_inode(idx, start_offset, start, summary)
            },

            (None, Some((end, summary))) => {
                gtree.insert_in_inode(idx, end_offset, end, summary)
            },

            (None, None) => None,
        };

        ((leaf_idx_start, leaf_idx_end), split)
    }

    fn delete_from<const N: usize, L, DelFrom>(
        gtree: &mut Gtree<N, L>,
        idx: InodeIdx,
        mut from: L::Length,
        del_from: DelFrom,
    ) -> (Option<LeafIdx>, Option<Inode<N, L>>)
    where
        L: Leaf,
        DelFrom: FnOnce(&mut L, L::Length) -> Option<L>,
    {
        let len = gtree.inode(idx).len();

        let mut offset = L::Length::zero();

        for child_idx in 0..len {
            let child_measure = gtree.child_measure(idx, child_idx);

            offset += child_measure;

            if offset > from {
                for child_idx in child_idx + 1..len {
                    gtree.delete_child(idx, child_idx);
                }

                offset -= child_measure;

                from -= offset;

                return match gtree.inode(idx).child(child_idx) {
                    Either::Internal(next_idx) => gtree
                        .with_internal_mut_handle_split(
                            next_idx,
                            child_idx,
                            |gtree| {
                                delete_from(gtree, next_idx, from, del_from)
                            },
                        ),

                    Either::Leaf(leaf_idx) => {
                        let (idx, _none, split) = gtree
                            .with_leaf_mut_handle_split(
                                leaf_idx,
                                child_idx,
                                |leaf| (del_from(leaf, from), None),
                            );

                        (idx, split)
                    },
                };
            }
        }

        unreachable!();
    }

    fn delete_up_to<const N: usize, L, DelUpTo>(
        gtree: &mut Gtree<N, L>,
        idx: InodeIdx,
        mut up_to: L::Length,
        del_up_to: DelUpTo,
    ) -> (Option<LeafIdx>, Option<Inode<N, L>>)
    where
        L: Leaf,
        DelUpTo: FnOnce(&mut L, L::Length) -> Option<L>,
    {
        let mut offset = L::Length::zero();

        for child_idx in 0..gtree.inode(idx).len() {
            let child_measure = gtree.child_measure(idx, child_idx);

            offset += child_measure;

            if offset >= up_to {
                offset -= child_measure;

                up_to -= offset;

                return match gtree.inode(idx).child(child_idx) {
                    Either::Internal(next_idx) => gtree
                        .with_internal_mut_handle_split(
                            next_idx,
                            child_idx,
                            |gtree| {
                                delete_up_to(gtree, next_idx, up_to, del_up_to)
                            },
                        ),

                    Either::Leaf(leaf_idx) => {
                        let (idx, _none, split) = gtree
                            .with_leaf_mut_handle_split(
                                leaf_idx,
                                child_idx,
                                |leaf| (del_up_to(leaf, up_to), None),
                            );

                        (idx, split)
                    },
                };
            } else {
                gtree.delete_child(idx, child_idx);
            }
        }

        unreachable!();
    }
}

mod debug {
    //! Debug implementations for types in the outer module.
    //!
    //! Placed here to avoid cluttering the main module.

    use core::fmt::{Formatter, Result as FmtResult};

    use super::*;

    impl Debug for MatrixIdx {
        fn fmt(&self, f: &mut Formatter) -> FmtResult {
            write!(f, "{}:{}", self.row, self.col)
        }
    }

    /// Custom Debug implementation which always prints the output in a single
    /// line even when the alternate flag (`{:#?}`) is enabled.
    impl Debug for InodeIdx {
        fn fmt(&self, f: &mut Formatter) -> FmtResult {
            write!(f, "InodeIdx({:?})", self.0)
        }
    }

    /// Custom Debug implementation which always prints the output in a single
    /// line even when the alternate flag (`{:#?}`) is enabled.
    impl Debug for LeafIdx {
        fn fmt(&self, f: &mut Formatter) -> FmtResult {
            write!(f, "LeafIdx({:?})", self.0)
        }
    }

    impl<const ARITY: usize, L: Leaf> Debug for Gtree<ARITY, L> {
        fn fmt(&self, f: &mut Formatter) -> FmtResult {
            self.debug_as_self().fmt(f)
        }
    }

    struct DebugAsDisplay<'a>(&'a dyn core::fmt::Display);

    impl Debug for DebugAsDisplay<'_> {
        fn fmt(&self, f: &mut Formatter) -> FmtResult {
            core::fmt::Display::fmt(self.0, f)
        }
    }

    /// A type providing a Debug implementation for `Gtree` which prints
    /// `Inode`s and `Lnode`s sequentially like they are stored in memory.
    pub struct DebugAsSelf<'a, const N: usize, L: Leaf>(
        pub(super) &'a Gtree<N, L>,
    );

    impl<const N: usize, L: Leaf> Debug for DebugAsSelf<'_, N, L> {
        fn fmt(&self, _f: &mut Formatter) -> FmtResult {
            let _gtree = self.0;

            todo!();

            //let debug_inodes = DebugInodesSequentially {
            //    inodes: gtree.inodes.as_slice(),
            //    root_idx: gtree.root_idx.0,
            //};

            //let debug_lnodes =
            //    DebugLnodesSequentially(gtree.lnodes.as_slice());

            //f.debug_map()
            //    .entry(&DebugAsDisplay(&"inodes"), &debug_inodes)
            //    .entry(&DebugAsDisplay(&"lnodes"), &debug_lnodes)
            //    .finish()
        }
    }

    struct DebugInodesSequentially<'a, const N: usize, L: Leaf> {
        inodes: &'a [Inode<N, L>],
        root_idx: usize,
    }

    impl<const N: usize, L: Leaf> Debug for DebugInodesSequentially<'_, N, L> {
        fn fmt(&self, f: &mut Formatter) -> FmtResult {
            struct Key {
                idx: usize,
                is_root: bool,
            }

            impl Debug for Key {
                fn fmt(&self, f: &mut Formatter) -> FmtResult {
                    let prefix = if self.is_root { "R -> " } else { "" };
                    write!(f, "{prefix}{}", self.idx)
                }
            }

            let entries =
                self.inodes.iter().enumerate().map(|(idx, inode)| {
                    let key = Key { idx, is_root: idx == self.root_idx };
                    (key, inode)
                });

            f.debug_map().entries(entries).finish()
        }
    }

    struct DebugLnodesSequentially<'a, L: Leaf>(&'a [Lnode<L>]);

    impl<L: Leaf> Debug for DebugLnodesSequentially<'_, L> {
        fn fmt(&self, f: &mut Formatter) -> FmtResult {
            f.debug_map()
                .entries(self.0.iter().map(Lnode::value).enumerate())
                .finish()
        }
    }

    /// A type providing a Debug implementation for `Gtree` which prints it as
    /// the equivalent Btree, highlighting the parent-child relationships
    /// between the inodes and the leaf nodes.
    ///
    /// Note that this is not how the tree is actually stored in memory. If you
    /// want to see the actual memory layout, use `DebugAsSelf`.
    pub struct DebugAsBtree<'a, const N: usize, L: Leaf> {
        pub(super) gtree: &'a Gtree<N, L>,
        pub(super) inode_idx: InodeIdx,
    }

    impl<const N: usize, L: Leaf> Debug for DebugAsBtree<'_, N, L> {
        fn fmt(&self, f: &mut Formatter) -> FmtResult {
            fn print_inode_as_tree<const N: usize, L: Leaf>(
                gtree: &Gtree<N, L>,
                inode_idx: InodeIdx,
                shifts: &mut String,
                ident: &str,
                last_shift_byte_len: usize,
                f: &mut Formatter,
            ) -> FmtResult {
                let inode = gtree.inode(inode_idx);

                writeln!(
                    f,
                    "{}{}{:?}",
                    &shifts[..shifts.len() - last_shift_byte_len],
                    ident,
                    inode.summary()
                )?;

                let is_last = |idx: usize| idx + 1 == inode.len();

                let ident = |idx: usize| {
                    if is_last(idx) {
                        "└── "
                    } else {
                        "├── "
                    }
                };

                let shift = |idx: usize| {
                    if is_last(idx) {
                        "    "
                    } else {
                        "│   "
                    }
                };

                match inode.children() {
                    Either::Internal(inode_idxs) => {
                        for (i, &inode_idx) in inode_idxs.iter().enumerate() {
                            let shift = shift(i);
                            shifts.push_str(shift);
                            let ident = ident(i);
                            print_inode_as_tree(
                                gtree,
                                inode_idx,
                                shifts,
                                ident,
                                shift.len(),
                                f,
                            )?;
                            shifts.truncate(shifts.len() - shift.len());
                        }
                    },

                    Either::Leaf(leaf_idxs) => {
                        for (i, &leaf_idx) in leaf_idxs.iter().enumerate() {
                            let ident = ident(i);
                            let lnode = gtree.lnode(leaf_idx);
                            writeln!(
                                f,
                                "{}{}{:#?}",
                                &shifts,
                                ident,
                                &lnode.value()
                            )?;
                        }
                    },
                }

                Ok(())
            }

            writeln!(f)?;

            print_inode_as_tree(
                self.gtree,
                self.inode_idx,
                &mut String::new(),
                "",
                0,
                f,
            )
        }
    }

    impl<const N: usize, L: Leaf> Debug for Inode<N, L> {
        fn fmt(&self, f: &mut Formatter) -> FmtResult {
            if !self.parent().is_dangling() {
                write!(f, "{:?} <- ", self.parent())?;
            }

            write!(f, "{:?} @ ", self.summary())?;

            let mut dbg = f.debug_list();

            match self.children() {
                Either::Internal(inode_idxs) => {
                    dbg.entries(inode_idxs).finish()
                },
                Either::Leaf(leaf_idxs) => dbg.entries(leaf_idxs).finish(),
            }
        }
    }
}
