//! Outcome of the final application-record gate, distinct from connection authority.

/// A synchronous prepared-record send either admits the whole record or leaves
/// it with its application owner. Neither outcome proves remote delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendAdmission<E> {
    /// The unchanged record entered the bounded transport queue.
    Accepted,
    /// No byte of this record entered a transport queue. The original
    /// connection and previously admitted records remain owned by the caller.
    /// The application decides whether this typed refusal permits recovery;
    /// it must never treat an authority or malformed-record error as expiry.
    Refused(E),
}
