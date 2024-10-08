use sha2::{Digest, Sha256};

use crate::encode::{Decode, Encode, IntDecodeError};
use crate::*;

/// We use this instead of a `Vec<u8>` because it's 1/3 the size on the stack.
pub(crate) type Checksum = Box<ChecksumArray>;

pub(crate) type ChecksumArray = [u8; 32];

const CHECKSUM_LEN: usize = core::mem::size_of::<ChecksumArray>();

/// A [`Replica`] encoded into a compact binary format suitable for
/// transmission over the network.
///
/// This struct is created by [`encode`](Replica::encode)ing a [`Replica`] and
/// can be decoded back into a [`Replica`] by calling
/// [`decode`](Replica::decode). See the documentation of those methods for
/// more information.
#[cfg_attr(docsrs, doc(cfg(feature = "encode")))]
#[derive(Clone, PartialEq, Eq)]
pub struct EncodedReplica {
    protocol_version: ProtocolVersion,
    checksum: Checksum,
    bytes: Box<[u8]>,
}

impl core::fmt::Debug for EncodedReplica {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        struct HexSlice<'a>(&'a [u8]);

        impl core::fmt::Debug for HexSlice<'_> {
            fn fmt(
                &self,
                f: &mut core::fmt::Formatter<'_>,
            ) -> core::fmt::Result {
                for byte in self.0 {
                    write!(f, "{:02x}", byte)?;
                }
                Ok(())
            }
        }

        f.debug_struct("EncodedReplica")
            .field("protocol_version", &self.protocol_version)
            .field("checksum", &HexSlice(self.checksum()))
            .finish_non_exhaustive()
    }
}

impl EncodedReplica {
    #[inline]
    pub(crate) fn bytes(&self) -> &[u8] {
        &*self.bytes
    }

    #[inline]
    pub(crate) fn checksum(&self) -> &[u8] {
        &*self.checksum
    }

    #[inline]
    pub(crate) fn new(
        protocol_version: ProtocolVersion,
        checksum: Checksum,
        bytes: Box<[u8]>,
    ) -> Self {
        Self { protocol_version, checksum, bytes }
    }

    #[inline]
    pub(crate) fn protocol_version(&self) -> ProtocolVersion {
        self.protocol_version
    }
}

impl Encode for EncodedReplica {
    #[inline]
    fn encode(&self, buf: &mut Vec<u8>) {
        self.protocol_version.encode(buf);
        buf.extend_from_slice(&*self.checksum);
        (self.bytes.len() as u64).encode(buf);
        buf.extend_from_slice(&*self.bytes);
    }
}

pub(crate) enum EncodedReplicaDecodeError {
    Int(IntDecodeError),
    Checksum { actual: usize },
    Bytes { actual: usize, advertised: u64 },
}

impl From<IntDecodeError> for EncodedReplicaDecodeError {
    #[inline]
    fn from(err: IntDecodeError) -> Self {
        Self::Int(err)
    }
}

impl core::fmt::Display for EncodedReplicaDecodeError {
    #[inline]
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let prefix = "Couldn't decode EncodedReplica";

        match self {
            EncodedReplicaDecodeError::Int(err) => {
                write!(f, "{prefix}: {err}")
            },

            EncodedReplicaDecodeError::Checksum { actual } => {
                write!(
                    f,
                    "{prefix}: need {CHECKSUM_LEN} bytes to decode checksum, \
                     but there are only {actual}",
                )
            },

            EncodedReplicaDecodeError::Bytes { actual, advertised } => {
                write!(
                    f,
                    "{prefix}: {advertised} bytes were encoded, but there \
                     are only {actual}",
                )
            },
        }
    }
}

impl Decode for EncodedReplica {
    type Value = Self;

    type Error = EncodedReplicaDecodeError;

    #[inline]
    fn decode(buf: &[u8]) -> Result<(Self, &[u8]), Self::Error> {
        let (protocol_version, buf) = ProtocolVersion::decode(buf)?;

        if buf.len() < CHECKSUM_LEN {
            return Err(EncodedReplicaDecodeError::Checksum {
                actual: buf.len(),
            });
        }

        let (checksum_slice, buf) = buf.split_at(CHECKSUM_LEN);

        let mut checksum = [0; CHECKSUM_LEN];

        checksum.copy_from_slice(checksum_slice);

        let (num_bytes, buf) = u64::decode(buf)?;

        if (buf.len() as u64) < num_bytes {
            return Err(EncodedReplicaDecodeError::Bytes {
                actual: buf.len(),
                advertised: num_bytes,
            });
        }

        let (bytes, buf) = buf.split_at(num_bytes as usize);

        let this = Self {
            protocol_version,
            checksum: Box::new(checksum),
            bytes: bytes.into(),
        };

        Ok((this, buf))
    }
}

/// The type of error that can occur when [`decode`](Replica::decode)ing an
/// [`EncodedReplica`].
#[cfg_attr(docsrs, doc(cfg(feature = "encode")))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// This error occurs when the internal checksum of the [`EncodedReplica`]
    /// fails.
    ///
    /// This typically means that the [`EncodedReplica`] was corrupted during
    /// transmission.
    ChecksumFailed,

    /// This error occurs when the machine that created the [`EncodedReplica`]
    /// and the one that is trying to [`decode`](Replica::decode) it are using
    /// two incompatible versions of cola.
    DifferentProtocol {
        /// The `ProtocolVersion` of cola on the machine that created the
        /// `EncodedReplica`.
        encoded_on: ProtocolVersion,

        /// The `ProtocolVersion` of cola on the machine that is trying to
        /// decode the `EncodedReplica`.
        decoding_on: ProtocolVersion,
    },

    /// This error is an umbrella variant that encompasses all other errors
    /// that can occur when the binary data wrapped by the [`EncodedReplica`]
    /// cannot be decoded into a `Replica`.
    ///
    /// This is returned when the checksum and protocol version checks both
    /// succeed, *and yet* the data is still invalid. The only way this can
    /// occur in practice is if the `EncodedReplica` passed to
    /// [`decode`](Replica::decode) was deserialized from a byte vector that
    /// was not the result of serializing an `EncodedReplica`.
    ///
    /// As long as you're not doing that (and you shouldn't be) this variant
    /// can be ignored.
    InvalidData,
}

impl core::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            DecodeError::ChecksumFailed => f.write_str("checksum failed"),

            DecodeError::DifferentProtocol { encoded_on, decoding_on } => {
                write!(
                    f,
                    "different protocol: encoded on {:?}, decoding on {:?}",
                    encoded_on, decoding_on
                )
            },

            DecodeError::InvalidData => f.write_str("invalid data"),
        }
    }
}

impl std::error::Error for DecodeError {}

#[inline(always)]
pub(crate) fn checksum(bytes: &[u8]) -> Checksum {
    Box::new(checksum_array(bytes))
}

#[inline(always)]
pub(crate) fn checksum_array(bytes: &[u8]) -> ChecksumArray {
    let checksum = Sha256::digest(bytes);
    *checksum.as_ref()
}

#[cfg(feature = "serde")]
mod serde {
    crate::encode::impl_serialize!(super::EncodedReplica);
    crate::encode::impl_deserialize!(super::EncodedReplica);
}
