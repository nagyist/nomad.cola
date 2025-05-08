use crate::*;

/// A stable reference to a position in a [`Replica`].
///
/// After its creation, an `Anchor` can be given to a `Replica` to
/// retrieve the current offset of the position it refers to, taking into
/// account all the edits that have been applied to the `Replica` in the
/// meantime.
///
/// This property makes `Anchor`s useful to implement things like cursors and
/// selections in collaborative editing environments.
//
/// For more information, see the documentation of
/// [`Replica::create_anchor()`][crate::Replica::create_anchor] and
/// [`Replica::resolve_anchor()`][crate::Replica::resolve_anchor].
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct Anchor {
    /// TODO: docs
    inner: InnerAnchor,

    bias: AnchorBias,
}

impl Anchor {
    #[inline(always)]
    pub(crate) fn bias(&self) -> AnchorBias {
        self.bias
    }

    #[inline(always)]
    pub(crate) fn end_of_document() -> Self {
        Self::new(InnerAnchor::zero(), AnchorBias::Right)
    }

    #[inline(always)]
    pub(crate) fn inner(&self) -> InnerAnchor {
        self.inner
    }

    #[inline(always)]
    pub(crate) fn is_end_of_document(&self) -> bool {
        self.inner.is_zero() && self.bias == AnchorBias::Right
    }

    #[inline(always)]
    pub(crate) fn is_start_of_document(&self) -> bool {
        self.inner.is_zero() && self.bias == AnchorBias::Left
    }

    #[inline(always)]
    pub(crate) fn new(inner: InnerAnchor, bias: AnchorBias) -> Self {
        Self { inner, bias }
    }

    #[inline(always)]
    pub(crate) fn start_of_document() -> Self {
        Self::new(InnerAnchor::zero(), AnchorBias::Left)
    }
}

/// A bias to use when creating an [`Anchor`].
///
/// This is used in the
/// [`Replica::create_anchor()`][crate::Replica::create_anchor] method to
/// create a new [`Anchor`]. See the documentation of that method for more
/// information.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum AnchorBias {
    /// The anchor should attach to the left.
    Left,

    /// The anchor should attach to the right.
    Right,
}

impl core::ops::Not for AnchorBias {
    type Output = Self;

    #[inline]
    fn not(self) -> Self::Output {
        match self {
            Self::Left => Self::Right,
            Self::Right => Self::Left,
        }
    }
}

/// TODO: docs
#[derive(Copy, Clone, PartialEq, Eq)]
pub(crate) struct InnerAnchor {
    /// TODO: docs
    replica_id: ReplicaId,

    /// The [`RunTs`] of the [`EditRun`] containing this [`Anchor`].
    contained_in: RunTs,

    /// TODO: docs
    offset: Length,
}

impl core::fmt::Debug for InnerAnchor {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        if self == &Self::zero() {
            write!(f, "zero")
        } else if f.alternate() {
            write!(
                f,
                "{:x}.{} in {}",
                self.replica_id, self.offset, self.contained_in
            )
        } else {
            write!(f, "{:x}.{}", self.replica_id, self.offset)
        }
    }
}

impl InnerAnchor {
    #[inline(always)]
    pub(crate) fn is_zero(&self) -> bool {
        self.replica_id == 0
    }

    #[inline(always)]
    pub(crate) fn new(
        replica_id: ReplicaId,
        offset: Length,
        run_ts: RunTs,
    ) -> Self {
        Self { replica_id, offset, contained_in: run_ts }
    }

    #[inline(always)]
    pub(crate) fn offset(&self) -> Length {
        self.offset
    }

    #[inline(always)]
    pub(crate) fn replica_id(&self) -> ReplicaId {
        self.replica_id
    }

    #[inline(always)]
    pub(crate) fn run_ts(&self) -> RunTs {
        self.contained_in
    }

    /// A special value used to create an anchor at the start of the document.
    #[inline]
    pub const fn zero() -> Self {
        Self { replica_id: 0, offset: 0, contained_in: 0 }
    }
}

#[cfg(feature = "encode")]
mod encode {
    use super::*;
    use crate::encode::{BoolDecodeError, Decode, Encode, IntDecodeError};

    impl Encode for Anchor {
        #[inline]
        fn encode(&self, buf: &mut Vec<u8>) {
            self.inner.encode(buf);
            self.bias.encode(buf);
        }
    }

    pub(crate) enum AnchorDecodeError {
        Bool(BoolDecodeError),
        Int(IntDecodeError),
    }

    impl From<BoolDecodeError> for AnchorDecodeError {
        #[inline(always)]
        fn from(err: BoolDecodeError) -> Self {
            Self::Bool(err)
        }
    }

    impl From<IntDecodeError> for AnchorDecodeError {
        #[inline(always)]
        fn from(err: IntDecodeError) -> Self {
            Self::Int(err)
        }
    }

    impl core::fmt::Display for AnchorDecodeError {
        #[inline]
        fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
            let err: &dyn core::fmt::Display = match self {
                Self::Bool(err) => err,
                Self::Int(err) => err,
            };

            write!(f, "Anchor couldn't be decoded: {err}")
        }
    }

    impl Decode for Anchor {
        type Value = Self;

        type Error = AnchorDecodeError;

        #[inline]
        fn decode(buf: &[u8]) -> Result<(Self, &[u8]), Self::Error> {
            let (inner, buf) = InnerAnchor::decode(buf)?;
            let (bias, buf) = AnchorBias::decode(buf)?;
            let anchor = Self::new(inner, bias);
            Ok((anchor, buf))
        }
    }

    impl Encode for InnerAnchor {
        #[inline]
        fn encode(&self, buf: &mut Vec<u8>) {
            self.replica_id().encode(buf);
            self.run_ts().encode(buf);
            self.offset().encode(buf);
        }
    }

    impl Decode for InnerAnchor {
        type Value = Self;

        type Error = IntDecodeError;

        #[inline]
        fn decode(buf: &[u8]) -> Result<(Self, &[u8]), Self::Error> {
            let (replica_id, buf) = ReplicaId::decode(buf)?;
            let (run_ts, buf) = RunTs::decode(buf)?;
            let (offset, buf) = Length::decode(buf)?;
            let anchor = Self::new(replica_id, offset, run_ts);
            Ok((anchor, buf))
        }
    }

    impl Encode for AnchorBias {
        #[inline]
        fn encode(&self, buf: &mut Vec<u8>) {
            matches!(self, Self::Right).encode(buf);
        }
    }

    impl Decode for AnchorBias {
        type Value = Self;

        type Error = BoolDecodeError;

        #[inline]
        fn decode(buf: &[u8]) -> Result<(Self, &[u8]), Self::Error> {
            let (is_right, buf) = bool::decode(buf)?;
            let bias = if is_right { Self::Right } else { Self::Left };
            Ok((bias, buf))
        }
    }
}

#[cfg(feature = "serde")]
mod serde {
    crate::encode::impl_serialize!(super::Anchor);
    crate::encode::impl_deserialize!(super::Anchor);
}
