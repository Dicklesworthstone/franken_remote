//! Distinct typed identifiers and generations (plan section 7.2).
//!
//! Two families exist and they are deliberately not interchangeable with
//! each other or among themselves:
//!
//! - **Unpredictable identifiers** name a live thing: a host boot, an OS
//!   session, a remote session, an input lease. They carry 128 opaque bits
//!   supplied by the caller's qualified randomness (this crate does not
//!   generate randomness). Unpredictability is a hygiene property, not an
//!   authority credential — possession of an identifier grants nothing.
//! - **Monotonic generations** fence configurations that replace one
//!   another: display geometry, codec configuration, recovery chain,
//!   viewport mapping. They only move forward; on exhaustion they refuse to
//!   wrap, because identity reuse is exactly the bug they exist to prevent
//!   (the owning epoch must be replaced instead).
//!
//! Different kinds do not unify:
//!
//! ```compile_fail
//! use fr_core::ids::{HostBootId, OsSessionId};
//! let boot = HostBootId::from_raw(7);
//! let _session: OsSessionId = boot; // distinct types by design
//! ```
//!
//! ```compile_fail
//! use fr_core::ids::{CodecConfigurationGeneration, RecoveryGeneration};
//! let cfg = CodecConfigurationGeneration::INITIAL;
//! let _rec: RecoveryGeneration = cfg; // a codec config is not a recovery chain
//! ```

use core::fmt;

macro_rules! unpredictable_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash)]
        pub struct $name(u128);

        impl $name {
            /// Wraps caller-supplied opaque bits (from qualified randomness).
            #[must_use]
            pub const fn from_raw(raw: u128) -> Self {
                Self(raw)
            }

            /// The opaque bits, for wire encoding.
            #[must_use]
            pub const fn as_raw(self) -> u128 {
                self.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({:032x})"), self.0)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{:032x}", self.0)
            }
        }
    };
}

macro_rules! monotonic_generation {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(u64);

        impl $name {
            /// The first generation of a fresh epoch.
            pub const INITIAL: Self = Self(0);

            /// Reconstructs a generation received over an authenticated
            /// channel. The wire layer, not this type, decides whether the
            /// value belongs to a live epoch.
            #[must_use]
            pub const fn from_raw(raw: u64) -> Self {
                Self(raw)
            }

            /// The raw counter, for wire encoding.
            #[must_use]
            pub const fn as_raw(self) -> u64 {
                self.0
            }

            /// The successor generation, or `None` when the counter is
            /// exhausted. Exhaustion never wraps: reusing generation
            /// identities would let stale work masquerade as current, so the
            /// caller must retire the owning epoch instead (plan section
            /// 7.2).
            #[must_use]
            pub const fn next(self) -> Option<Self> {
                match self.0.checked_add(1) {
                    Some(n) => Some(Self(n)),
                    None => None,
                }
            }

            /// True when `self` supersedes `other` within the same epoch.
            #[must_use]
            pub const fn supersedes(self, other: Self) -> bool {
                self.0 > other.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({})"), self.0)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

unpredictable_id! {
    /// One boot of one host. Everything a previous boot issued is stale
    /// under a new `HostBootId`.
    HostBootId
}
unpredictable_id! {
    /// One interactive OS session on the host. Lock, logout, and user
    /// switching end it; a new session never inherits the old controller.
    OsSessionId
}
unpredictable_id! {
    /// One admitted remote application session (one viewer or controller
    /// connection lifecycle).
    RemoteSessionId
}
unpredictable_id! {
    /// One grant of input authority. Expired leases are terminal;
    /// reacquisition mints a new identity (plan section 6.3).
    InputLeaseId
}

monotonic_generation! {
    /// Display topology/geometry version. Coordinate-dependent input binds
    /// to this; hotplug or reconfiguration advances it before any
    /// coordinate is accepted.
    DisplayGeometryGeneration
}
monotonic_generation! {
    /// Codec configuration version (SPS-level change, resolution, profile,
    /// color). Advancing requires reconfiguration plus a fresh IDR.
    CodecConfigurationGeneration
}
monotonic_generation! {
    /// Decoder reference-chain recovery version. An IDR after loss advances
    /// this even when the codec configuration is unchanged; it belongs to a
    /// receiving subscription, not the shared encoder.
    RecoveryGeneration
}
monotonic_generation! {
    /// Viewport/input-mapping version. A server-side crop or pan changes the
    /// coordinate mapping without changing coded geometry; input must
    /// acknowledge the new mapping before coordinates are honored.
    ViewportMappingGeneration
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unpredictable_ids_round_trip_and_compare_by_value() {
        let a = InputLeaseId::from_raw(0x00ff_1234_5678_9abc_def0_1122_3344_5566);
        let b = InputLeaseId::from_raw(0x00ff_1234_5678_9abc_def0_1122_3344_5566);
        let c = InputLeaseId::from_raw(1);
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.as_raw(), b.as_raw());
        // Display is fixed-width hex so logs align and never truncate bits.
        assert_eq!(format!("{c}"), format!("{:032x}", 1));
        assert_eq!(
            format!("{a:?}"),
            format!("InputLeaseId({:032x})", a.as_raw())
        );
    }

    #[test]
    fn generations_start_at_initial_and_advance_monotonically() {
        let g0 = DisplayGeometryGeneration::INITIAL;
        let g1 = g0.next().expect("first advance succeeds");
        let g2 = g1.next().expect("second advance succeeds");
        assert!(g1.supersedes(g0));
        assert!(g2.supersedes(g1));
        assert!(!g0.supersedes(g0));
        assert!(!g0.supersedes(g2));
        assert!(g0 < g1 && g1 < g2);
    }

    #[test]
    fn generation_exhaustion_refuses_to_wrap() {
        let last = RecoveryGeneration::from_raw(u64::MAX);
        assert_eq!(last.next(), None);
        // The near-boundary value still advances exactly once.
        let almost = RecoveryGeneration::from_raw(u64::MAX - 1);
        assert_eq!(almost.next(), Some(last));
    }

    #[test]
    fn generation_wire_round_trip() {
        let g = ViewportMappingGeneration::from_raw(42);
        assert_eq!(ViewportMappingGeneration::from_raw(g.as_raw()), g);
        assert_eq!(format!("{g}"), "42");
        assert_eq!(format!("{g:?}"), "ViewportMappingGeneration(42)");
    }
}
