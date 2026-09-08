#![forbid(unsafe_code)]
//! Deterministic byte delivery for production state-machine scenarios.
//!
//! Asupersync's [`LabRuntime`] owns virtual time and its [`DetRng`] selects
//! faults. This adapter bounds payload **and metadata** before copying bytes.
//! It models datagram delivery, not QUIC reliability, TLS, HEVC, OS submission,
//! or the cancellation of runtime tasks/foreign workers. Shipping crates must
//! not depend on this test harness (plan sections 4.1, 18.4, 24.1).
//!
//! A scenario scripts both endpoints using [`Scenario::send`], [`Scenario::advance`],
//! [`Scenario::elapse`] and [`Scenario::drain`]. The callback feeds the production parser/state
//! machine. `elapse` deliberately runs no callbacks: a scheduler stall or sleep
//! advances the authority clock before queued input can be considered. Apply
//! the production resume/revoke boundary before draining. [`Scenario::fence`]
//! retires the old transport epoch; even already-committed queued sends remain
//! visible as stale drops, never as an implied rollback of external effects.
//!
//! The trace retains the complete admitted schedule, seed, delivery order,
//! fault choices and caller-supplied numeric outcomes. Payloads are never
//! formatted. Trace exhaustion is an explicit refusal; evidence is not silently
//! overwritten. An unwinding scenario prints its sanitized history as well, so
//! an assertion in a receiver retains the schedule. Reproduce with the same
//! scenario code, dependency lock and seed.

use asupersync::lab::{LabConfig, LabRuntime};
use asupersync::types::Time;
use asupersync::util::DetRng;
use fr_core::time::{HostDuration, HostInstant};
use std::fmt;
use std::mem::size_of;

/// Receiver of a simulated datagram. No network addresses are retained.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Destination {
    Host,
    Client,
}

/// Explicit faults compose exact schedules; seeded faults explore schedules.
/// Reordering is obtained by selecting different delivery delays.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    After(HostDuration),
    Drop,
    Duplicate {
        first: HostDuration,
        second: HostDuration,
    },
    Seeded {
        max_delay: HostDuration,
        loss_per_million: u32,
        duplicate_per_million: u32,
    },
}

impl Fault {
    pub const fn after(delay: HostDuration) -> Self {
        Self::After(delay)
    }
}

/// Hard, per-scenario allocation limits, including zero-payload metadata.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_packets: usize,
    /// Includes the fixed packet-slot allocation plus retained payload bytes.
    pub max_queue_bytes: usize,
    pub max_packet_bytes: usize,
    pub max_trace_events: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_packets: 256,
            max_queue_bytes: 2 * 1024 * 1024,
            max_packet_bytes: 65_536,
            max_trace_events: 4096,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    InvalidLimits,
    InvalidFault,
    PacketTooLarge,
    PacketBudget,
    ByteBudget,
    TraceBudget,
    ClockOverflow,
    EpochExhausted,
    ChannelClosed,
}

/// An error carries its bounded, sanitized history, including the seed.
/// Constructing this error can temporarily duplicate the trace allocation.
#[derive(Debug)]
pub struct Failure {
    pub seed: u64,
    pub reason: Refusal,
    pub trace: Vec<TraceEvent>,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "seed={} refusal={:?} schedule={:?}",
            self.seed, self.reason, self.trace
        )
    }
}

impl std::error::Error for Failure {}

/// Schedule metadata only. Packet IDs are local trace ordinals, not wire IDs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Sent {
        id: u64,
        to: Destination,
        bytes: usize,
        epoch: u64,
        fault: Fault,
        first_due: Option<HostInstant>,
        second_due: Option<HostInstant>,
    },
    Elapsed {
        until: HostInstant,
    },
    /// Recorded before invoking the receiver. Without a following `Delivered`
    /// event, its external effects are unknown (for example, after a panic).
    Dispatching {
        id: u64,
        to: Destination,
    },
    Delivered {
        id: u64,
        to: Destination,
        outcome: u16,
    },
    Stale {
        id: u64,
        epoch: u64,
    },
    Fenced {
        epoch: u64,
    },
    Reopened {
        epoch: u64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TraceEvent {
    pub at: HostInstant,
    pub event: Event,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Metrics {
    pub queued_packets: usize,
    /// Current payload plus all preallocated packet slots, even empty ones.
    pub queue_bytes: usize,
    pub peak_packets: usize,
    pub peak_queue_bytes: usize,
    /// Trace backing storage; separate from the packet queue's byte budget.
    pub trace_capacity_bytes: usize,
}

struct Packet {
    id: u64,
    copy: u8,
    to: Destination,
    epoch: u64,
    due: HostInstant,
    payload: Box<[u8]>,
}

/// Borrowed only for the callback; no further queue admission runs during it.
pub struct Delivery<'a> {
    pub id: u64,
    pub to: Destination,
    pub payload: &'a [u8],
    /// Actual callback time, never the earlier scheduled delivery time.
    pub now: HostInstant,
}

pub struct Scenario {
    seed: u64,
    limits: Limits,
    runtime: LabRuntime,
    rng: DetRng,
    packets: Box<[Option<Packet>]>,
    trace: Vec<TraceEvent>,
    metrics: Metrics,
    epoch: u64,
    closed: bool,
}

impl fmt::Debug for Scenario {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Scenario")
            .field("seed", &self.seed)
            .field("limits", &self.limits)
            .field("metrics", &self.metrics)
            .field("epoch", &self.epoch)
            .field("closed", &self.closed)
            .field("trace", &self.trace)
            .finish_non_exhaustive()
    }
}

impl Drop for Scenario {
    fn drop(&mut self) {
        if std::thread::panicking() {
            use std::io::Write;
            // Diagnostic I/O failure must not cause a second panic during unwind.
            let _ = writeln!(std::io::stderr().lock(), "{self:?}");
        }
    }
}

impl Scenario {
    /// Queue-slot metadata cost on the compiling target. Allocator overhead and
    /// Asupersync's fixed runtime baseline are not process-RSS measurements.
    pub const fn packet_slot_bytes() -> usize {
        size_of::<Option<Packet>>()
    }

    pub fn new(seed: u64, limits: Limits) -> Result<Self, Failure> {
        let invalid = || Failure {
            seed,
            reason: Refusal::InvalidLimits,
            trace: Vec::new(),
        };
        let slot_bytes = limits
            .max_packets
            .checked_mul(Self::packet_slot_bytes())
            .ok_or_else(invalid)?;
        if limits.max_packets == 0
            || limits.max_packets > 4096
            || limits.max_queue_bytes < slot_bytes
            || limits.max_queue_bytes > 64 * 1024 * 1024
            || limits.max_packet_bytes == 0
            || limits.max_packet_bytes > 16 * 1024 * 1024
            || limits.max_trace_events == 0
            || limits.max_trace_events > 65_536
        {
            return Err(invalid());
        }
        let mut config = LabConfig::new(seed);
        // There are no spawned tasks in this adapter; only the lab clock is
        // advanced. Disable unused runtime trace retention. Our trace is bounded
        // above and records every fault/time/delivery decision without payloads.
        config.trace_capacity = 0;
        let trace = Vec::with_capacity(limits.max_trace_events);
        let metrics = Metrics {
            queue_bytes: slot_bytes,
            peak_queue_bytes: slot_bytes,
            trace_capacity_bytes: trace.capacity() * size_of::<TraceEvent>(),
            ..Metrics::default()
        };
        Ok(Self {
            seed,
            limits,
            runtime: LabRuntime::new(config),
            rng: DetRng::new(seed),
            packets: (0..limits.max_packets).map(|_| None).collect(),
            trace,
            metrics,
            epoch: 0,
            closed: false,
        })
    }

    pub fn now(&self) -> HostInstant {
        HostInstant::from_micros(self.runtime.now().as_nanos() / 1000)
    }

    pub fn metrics(&self) -> Metrics {
        self.metrics
    }
    pub fn trace(&self) -> &[TraceEvent] {
        &self.trace
    }

    fn failure(&self, reason: Refusal) -> Failure {
        Failure {
            seed: self.seed,
            reason,
            trace: self.trace.clone(),
        }
    }

    fn room(&self, events: usize) -> Result<(), Failure> {
        if events > self.limits.max_trace_events - self.trace.len() {
            Err(self.failure(Refusal::TraceBudget))
        } else {
            Ok(())
        }
    }

    fn record(&mut self, event: Event) {
        self.trace.push(TraceEvent {
            at: self.now(),
            event,
        });
    }

    fn due(&self, delay: HostDuration) -> Result<HostInstant, Failure> {
        self.now()
            .checked_add(delay)
            .filter(|time| time.as_micros() <= u64::MAX / 1000)
            .ok_or_else(|| self.failure(Refusal::ClockOverflow))
    }

    fn sample(
        &mut self,
        fault: Fault,
    ) -> Result<(Option<HostInstant>, Option<HostInstant>), Failure> {
        let delays = match fault {
            Fault::After(delay) => (Some(delay), None),
            Fault::Drop => (None, None),
            Fault::Duplicate { first, second } => (Some(first), Some(second)),
            Fault::Seeded {
                max_delay,
                loss_per_million,
                duplicate_per_million,
            } => {
                if loss_per_million > 1_000_000 || duplicate_per_million > 1_000_000 {
                    return Err(self.failure(Refusal::InvalidFault));
                }
                self.due(max_delay)?;
                if self.rng.next_u64() % 1_000_000 < u64::from(loss_per_million) {
                    (None, None)
                } else {
                    let range = max_delay.as_micros() + 1; // due() bounded this first
                    let first = HostDuration::from_micros(self.rng.next_u64() % range);
                    let second = (self.rng.next_u64() % 1_000_000
                        < u64::from(duplicate_per_million))
                    .then(|| HostDuration::from_micros(self.rng.next_u64() % range));
                    (Some(first), second)
                }
            }
        };
        Ok((
            delays.0.map(|d| self.due(d)).transpose()?,
            delays.1.map(|d| self.due(d)).transpose()?,
        ))
    }

    /// Commits zero, one or two datagrams atomically after admission. Duplicate
    /// payloads and their slots are both charged. A dropped send is still traced.
    pub fn send(&mut self, to: Destination, payload: &[u8], fault: Fault) -> Result<u64, Failure> {
        self.room(1)?;
        if self.closed {
            return Err(self.failure(Refusal::ChannelClosed));
        }
        if payload.len() > self.limits.max_packet_bytes {
            return Err(self.failure(Refusal::PacketTooLarge));
        }
        let (first_due, second_due) = self.sample(fault)?;
        let copies = usize::from(first_due.is_some()) + usize::from(second_due.is_some());
        if copies > self.limits.max_packets - self.metrics.queued_packets {
            return Err(self.failure(Refusal::PacketBudget));
        }
        let added_bytes = payload
            .len()
            .checked_mul(copies)
            .ok_or_else(|| self.failure(Refusal::ByteBudget))?;
        if added_bytes > self.limits.max_queue_bytes - self.metrics.queue_bytes {
            return Err(self.failure(Refusal::ByteBudget));
        }
        let id = self.trace.len() as u64;
        for (copy, due) in [first_due, second_due].into_iter().enumerate() {
            if let Some(due) = due {
                let slot = self
                    .packets
                    .iter_mut()
                    .find(|slot| slot.is_none())
                    .expect("admitted slot count");
                *slot = Some(Packet {
                    id,
                    copy: u8::try_from(copy).expect("at most two copies"),
                    to,
                    epoch: self.epoch,
                    due,
                    payload: payload.into(),
                });
            }
        }
        self.metrics.queued_packets += copies;
        self.metrics.queue_bytes += added_bytes;
        self.metrics.peak_packets = self.metrics.peak_packets.max(self.metrics.queued_packets);
        self.metrics.peak_queue_bytes = self.metrics.peak_queue_bytes.max(self.metrics.queue_bytes);
        self.record(Event::Sent {
            id,
            to,
            bytes: payload.len(),
            epoch: self.epoch,
            fault,
            first_due,
            second_due,
        });
        Ok(id)
    }

    /// Advances the lab monotonic clock without running the receiver. Use this
    /// for scheduler stalls/suspend. Then apply the production boundary and drain.
    pub fn elapse(&mut self, duration: HostDuration) -> Result<(), Failure> {
        self.room(1)?;
        let until = self.due(duration)?;
        self.runtime
            .advance_time_to(Time::from_nanos(until.as_micros() * 1000));
        self.record(Event::Elapsed { until });
        Ok(())
    }

    /// Normal progression: service each due time before advancing to the end.
    /// Contrast with `elapse` followed by `drain`, which injects a receiver stall.
    /// Reserves all required trace space before advancing or invoking a callback.
    pub fn advance(
        &mut self,
        duration: HostDuration,
        mut receive: impl FnMut(Delivery<'_>) -> u16,
    ) -> Result<usize, Failure> {
        let until = self.due(duration)?;
        let ready = self
            .packets
            .iter()
            .flatten()
            .filter(|p| p.due <= until)
            .count();
        self.room(3 * ready + 1)?; // max_packets <= 4096
        let mut delivered = 0;
        while let Some(next) = self
            .packets
            .iter()
            .flatten()
            .map(|p| p.due)
            .filter(|due| *due <= until)
            .min()
        {
            let now = self.now();
            if next > now {
                self.elapse(next.checked_duration_since(now).expect("ordered instants"))?;
            }
            delivered += self.drain(&mut receive)?;
        }
        if until > self.now() {
            self.elapse(
                until
                    .checked_duration_since(self.now())
                    .expect("bounded target"),
            )?;
        }
        Ok(delivered)
    }

    /// Delivers ready datagrams in (due time, send ordinal, copy ordinal) order.
    /// Outcomes are caller-defined bounded codes; never pass secret-bearing text.
    /// No callback can change this adapter while its payload is borrowed. A panic
    /// consumes the packet and closes the adapter: unknown callback effects are
    /// never retried automatically after `catch_unwind`.
    pub fn drain(
        &mut self,
        mut receive: impl FnMut(Delivery<'_>) -> u16,
    ) -> Result<usize, Failure> {
        let now = self.now();
        let ready = self
            .packets
            .iter()
            .flatten()
            .filter(|p| p.due <= now)
            .count();
        self.room(2 * ready)?; // Dispatching plus completion; stale drops need only one.
        for _ in 0..ready {
            let index = self
                .packets
                .iter()
                .enumerate()
                .filter_map(|(i, p)| p.as_ref().map(|p| (i, p)))
                .filter(|(_, p)| p.due <= now)
                .min_by_key(|(_, p)| (p.due, p.id, p.copy))
                .map(|(i, _)| i)
                .expect("counted ready packet");
            let packet = self.packets[index].take().expect("selected occupied slot");
            self.metrics.queued_packets -= 1;
            self.metrics.queue_bytes -= packet.payload.len();
            let event = if packet.epoch == self.epoch && !self.closed {
                self.closed = true; // Remains closed if the callback unwinds.
                self.record(Event::Dispatching {
                    id: packet.id,
                    to: packet.to,
                });
                let outcome = receive(Delivery {
                    id: packet.id,
                    to: packet.to,
                    payload: &packet.payload,
                    now,
                });
                self.closed = false;
                Event::Delivered {
                    id: packet.id,
                    to: packet.to,
                    outcome,
                }
            } else {
                Event::Stale {
                    id: packet.id,
                    epoch: packet.epoch,
                }
            };
            self.record(event);
        }
        Ok(ready)
    }

    /// Fences committed transport work; does not revoke production authority on
    /// its own. Call the production local-revoke/close operation first.
    pub fn fence(&mut self) -> Result<(), Failure> {
        self.room(1)?;
        self.epoch = self
            .epoch
            .checked_add(1)
            .ok_or_else(|| self.failure(Refusal::EpochExhausted))?;
        self.closed = true;
        self.record(Event::Fenced { epoch: self.epoch });
        Ok(())
    }

    /// A reconnect is a new transport epoch. The caller must also create fresh
    /// production session/lease identities; queued old bytes cannot attach to it.
    pub fn reopen(&mut self) -> Result<(), Failure> {
        self.room(1)?;
        self.epoch = self
            .epoch
            .checked_add(1)
            .ok_or_else(|| self.failure(Refusal::EpochExhausted))?;
        self.closed = false;
        self.record(Event::Reopened { epoch: self.epoch });
        Ok(())
    }
}
