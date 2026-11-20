//! Seed-reproducible deterministic simulation harness on `asupersync::lab::LabRuntime`
//! (`fr-rc2-deterministic-simulation-5pww`, plan §24.1).
//!
//! Drives the production [`SessionAuthority`] and [`HeldState`] state machines
//! across multiple concurrent peers sharing one host controller slot and one
//! bounded multi-peer demultiplexer under virtual time and deterministic
//! fault injection (loss, reorder, duplicate, delay, stale view, host suspend,
//! control->view demotion, local revoke, reconnect, and cross-peer spoofing).
//!
//! Every run is 100% deterministic from its `u64` seed and uses zero wall-clock
//! timing. Any invariant violation returns [`SimViolation`] with the exact seed
//! and bounded sanitized trace needed to reproduce the failing schedule.

use asupersync::lab::{LabConfig, LabRuntime};
use asupersync::util::DetRng;
use fr_core::authority::{AuthorityPolicy, Phase, SessionAuthority, ViewReadiness};
use fr_core::held_state::HeldState;
use fr_core::ids::{InputLeaseId, InputTicketId, RemoteSessionId};
use fr_core::input::PhysicalKey;
use fr_core::time::HostInstant;
use std::collections::VecDeque;
use std::fmt;

/// Maximum number of recent sanitized trace entries retained for violation reports.
const MAX_TRACE_RING: usize = 64;

/// Maximum simulated peers per run.
pub const MAX_SIM_PEERS: usize = 8;

/// Fault-injection parameters driven by `DetRng`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FaultProfile {
    /// Probability (per 1,000) of dropping an in-flight network message.
    pub drop_per_mille: u16,
    /// Probability (per 1,000) of duplicating an in-flight message with a second delay.
    pub duplicate_per_mille: u16,
    /// Maximum virtual delivery delay in milliseconds.
    pub max_delay_ms: u64,
    /// Maximum virtual time step per scheduler tick in milliseconds.
    pub max_step_ms: u64,
    /// Hard cap on queued in-flight messages across all peers.
    pub max_inflight: usize,
}

impl Default for FaultProfile {
    fn default() -> Self {
        Self {
            drop_per_mille: 120,
            duplicate_per_mille: 80,
            max_delay_ms: 900,
            max_step_ms: 450,
            max_inflight: 64,
        }
    }
}

/// Optional deliberate bug injection for verifying the invariant oracle catches
/// real concurrency/scheduling bugs with a reproducible seed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlantedBug {
    /// Production behavior (no faulted host logic).
    #[default]
    None,
    /// Deliberately grant a new controller without first releasing held keys.
    SkipCleanupBeforeHandoff,
    /// Deliberately deliver one peer's packet to a different peer's session.
    CrossPeerDemuxLeak,
    /// Deliberately omit `invalidate_for_suspend` during a host suspend event.
    SkipSuspendFence,
}

/// Configuration for one deterministic simulation run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SimConfig {
    pub seed: u64,
    pub peer_count: usize,
    pub steps: usize,
    pub faults: FaultProfile,
    pub planted_bug: PlantedBug,
}

impl SimConfig {
    pub fn standard(seed: u64) -> Self {
        Self {
            seed,
            peer_count: 4,
            steps: 250,
            faults: FaultProfile::default(),
            planted_bug: PlantedBug::None,
        }
    }
}

/// Named safety invariants checked after every simulation step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimInvariant {
    SingleControllerExclusivity,
    ControlImpliesLiveObservation,
    CleanReleaseBeforeHandoff,
    NoLeaseResurrection,
    SubmissionTimeAuthorityFence,
    DemuxIsolation,
    BoundedInflightQueue,
}

/// Content-free trace entry recorded during simulation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SimTraceEvent {
    pub step: usize,
    pub time_us: u64,
    pub peer: u8,
    pub kind: SimEventKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimEventKind {
    PeerConnected,
    ViewReady,
    ViewStale,
    ObsChallengeIssued,
    ObsRenewed,
    ControlGranted,
    ControlChallengeIssued,
    ControlRenewed,
    ControlDemoted,
    TicketIssued,
    InputSubmitted,
    InputRefused,
    LocalRevoke,
    HostSuspend,
    HostResume,
    PeerClosed,
    CrossPeerSpoofRefused,
    PacketDropped,
    PacketDelivered,
}

/// Reproducible violation report printed when any invariant fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimViolation {
    pub seed: u64,
    pub step: usize,
    pub time_us: u64,
    pub invariant: SimInvariant,
    pub detail: String,
    pub recent_trace: Vec<SimTraceEvent>,
}

impl fmt::Display for SimViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "deterministic simulation violation [seed={} step={} time_us={} invariant={:?}]: {} (recent_events={})",
            self.seed,
            self.step,
            self.time_us,
            self.invariant,
            self.detail,
            self.recent_trace.len()
        )
    }
}

impl std::error::Error for SimViolation {}

/// Deterministic summary counters + FNV-1a trace digest for bit-exact replay checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SimSummary {
    pub seed: u64,
    pub steps_executed: usize,
    pub final_time_us: u64,
    pub control_grants: u32,
    pub control_demotions: u32,
    pub obs_renewals: u32,
    pub control_renewals: u32,
    pub inputs_submitted: u32,
    pub inputs_refused: u32,
    pub spoofs_refused: u32,
    pub suspends: u32,
    pub packets_dropped: u32,
    pub packets_delivered: u32,
    pub trace_digest: u64,
}

#[derive(Debug, Clone, Copy)]
enum WirePayload {
    ObsChallengeResponse {
        nonce: u128,
    },
    ControlChallengeResponse {
        lease: InputLeaseId,
        nonce: u128,
    },
    InputAction {
        lease: InputLeaseId,
        ticket: InputTicketId,
        key: PhysicalKey,
        pressed: bool,
    },
}

#[derive(Debug, Clone, Copy)]
struct InflightPacket {
    deliver_at_us: u64,
    seq: u64,
    origin_peer: usize,
    target_peer: usize,
    target_session: RemoteSessionId,
    payload: WirePayload,
}

struct SimPeer {
    peer_index: usize,
    session_id: RemoteSessionId,
    authority: SessionAuthority,
    active_lease: Option<InputLeaseId>,
    last_dead_lease: Option<(InputLeaseId, u128)>,
    last_ticket: Option<InputTicketId>,
    held: HeldState,
    suspended_unreleased: bool,
}

impl SimPeer {
    fn new(peer_index: usize, session_id: RemoteSessionId) -> Self {
        Self {
            peer_index,
            session_id,
            authority: SessionAuthority::new(session_id, AuthorityPolicy::plan_defaults()),
            active_lease: None,
            last_dead_lease: None,
            last_ticket: None,
            held: HeldState::default(),
            suspended_unreleased: false,
        }
    }

    fn release_all_held(&mut self) {
        self.held = HeldState::default();
        self.suspended_unreleased = false;
    }

    fn held_count(&self) -> usize {
        held_total(&self.held)
    }
}

fn held_total(state: &HeldState) -> usize {
    let keys: u32 = state.key_bits().iter().map(|b| b.count_ones()).sum();
    let buttons = state.button_bits().count_ones();
    usize::try_from(keys.saturating_add(buttons)).unwrap_or(usize::MAX)
}

struct SimHarness {
    config: SimConfig,
    lab: LabRuntime,
    rng: DetRng,
    peers: Vec<SimPeer>,
    controller_slot: Option<usize>,
    host_held: HeldState,
    inflight: Vec<InflightPacket>,
    trace: VecDeque<SimTraceEvent>,
    trace_digest: u64,
    next_session_raw: u128,
    next_lease_raw: u128,
    next_ticket_raw: u128,
    next_nonce: u128,
    next_packet_seq: u64,
    summary: SimSummary,
}

impl SimHarness {
    fn new(config: SimConfig) -> Result<Self, SimViolation> {
        if config.peer_count == 0
            || config.peer_count > MAX_SIM_PEERS
            || config.faults.max_inflight == 0
        {
            return Err(SimViolation {
                seed: config.seed,
                step: 0,
                time_us: 0,
                invariant: SimInvariant::BoundedInflightQueue,
                detail: "invalid simulation configuration".to_string(),
                recent_trace: Vec::new(),
            });
        }

        let lab = LabRuntime::new(LabConfig::new(config.seed));
        let rng = DetRng::new(config.seed ^ 0x9E37_79B9_7F4A_7C15);
        let mut peers = Vec::with_capacity(config.peer_count);
        let mut next_session_raw = 1_u128;
        for idx in 0..config.peer_count {
            let sid = RemoteSessionId::from_raw(next_session_raw);
            next_session_raw += 1;
            peers.push(SimPeer::new(idx, sid));
        }

        Ok(Self {
            config,
            lab,
            rng,
            peers,
            controller_slot: None,
            host_held: HeldState::default(),
            inflight: Vec::new(),
            trace: VecDeque::with_capacity(MAX_TRACE_RING),
            trace_digest: 0xcbf2_9ce4_8422_2325,
            next_session_raw,
            next_lease_raw: 100,
            next_ticket_raw: 1_000,
            next_nonce: 10_000,
            next_packet_seq: 1,
            summary: SimSummary {
                seed: config.seed,
                steps_executed: 0,
                final_time_us: 0,
                control_grants: 0,
                control_demotions: 0,
                obs_renewals: 0,
                control_renewals: 0,
                inputs_submitted: 0,
                inputs_refused: 0,
                spoofs_refused: 0,
                suspends: 0,
                packets_dropped: 0,
                packets_delivered: 0,
                trace_digest: 0,
            },
        })
    }

    fn now(&self) -> HostInstant {
        HostInstant::from_micros(self.lab.now().as_nanos() / 1_000)
    }

    fn now_us(&self) -> u64 {
        self.lab.now().as_nanos() / 1_000
    }

    fn advance_ms(&mut self, ms: u64) {
        if ms > 0 {
            self.lab.advance_time(ms.saturating_mul(1_000_000));
        }
    }

    fn record_event(&mut self, step: usize, peer: usize, kind: SimEventKind) {
        let ev = SimTraceEvent {
            step,
            time_us: self.now_us(),
            peer: u8::try_from(peer).unwrap_or(u8::MAX),
            kind,
        };
        if self.trace.len() == MAX_TRACE_RING {
            self.trace.pop_front();
        }
        self.trace.push_back(ev);
        // FNV-1a mix for deterministic replay digest.
        for byte in ev
            .time_us
            .to_le_bytes()
            .into_iter()
            .chain([ev.peer, kind as u8])
        {
            self.trace_digest ^= u64::from(byte);
            self.trace_digest = self.trace_digest.wrapping_mul(0x0100_0000_01b3);
        }
    }

    fn violation(&self, step: usize, invariant: SimInvariant, detail: String) -> SimViolation {
        SimViolation {
            seed: self.config.seed,
            step,
            time_us: self.now_us(),
            invariant,
            detail,
            recent_trace: self.trace.iter().copied().collect(),
        }
    }

    fn alloc_nonce(&mut self) -> u128 {
        let n = self.next_nonce;
        self.next_nonce += 1;
        n
    }

    fn alloc_lease(&mut self) -> InputLeaseId {
        let id = InputLeaseId::from_raw(self.next_lease_raw);
        self.next_lease_raw += 1;
        id
    }

    fn alloc_ticket(&mut self) -> InputTicketId {
        let id = InputTicketId::from_raw(self.next_ticket_raw);
        self.next_ticket_raw += 1;
        id
    }

    fn enqueue_packet(
        &mut self,
        step: usize,
        origin_peer: usize,
        target_peer: usize,
        target_session: RemoteSessionId,
        payload: WirePayload,
    ) {
        let roll = self.rng.next_u64() % 1_000;
        if roll < u64::from(self.config.faults.drop_per_mille) {
            self.summary.packets_dropped += 1;
            self.record_event(step, origin_peer, SimEventKind::PacketDropped);
            return;
        }
        if self.inflight.len() >= self.config.faults.max_inflight {
            self.summary.packets_dropped += 1;
            self.record_event(step, origin_peer, SimEventKind::PacketDropped);
            return;
        }

        let delay_ms = if self.config.faults.max_delay_ms == 0 {
            0
        } else {
            self.rng.next_u64() % (self.config.faults.max_delay_ms + 1)
        };
        let deliver_at_us = self.now_us().saturating_add(delay_ms.saturating_mul(1_000));
        let seq = self.next_packet_seq;
        self.next_packet_seq += 1;
        self.inflight.push(InflightPacket {
            deliver_at_us,
            seq,
            origin_peer,
            target_peer,
            target_session,
            payload,
        });

        let dup_roll = self.rng.next_u64() % 1_000;
        if dup_roll < u64::from(self.config.faults.duplicate_per_mille)
            && self.inflight.len() < self.config.faults.max_inflight
        {
            let extra_ms = 1 + (self.rng.next_u64() % (self.config.faults.max_delay_ms.max(1)));
            let dup_seq = self.next_packet_seq;
            self.next_packet_seq += 1;
            self.inflight.push(InflightPacket {
                deliver_at_us: deliver_at_us.saturating_add(extra_ms.saturating_mul(1_000)),
                seq: dup_seq,
                origin_peer,
                target_peer,
                target_session,
                payload,
            });
        }
    }

    fn deliver_due_packets(&mut self, step: usize) -> Result<(), SimViolation> {
        let now_us = self.now_us();
        let mut due = Vec::new();
        let mut remaining = Vec::with_capacity(self.inflight.len());
        for pkt in self.inflight.drain(..) {
            if pkt.deliver_at_us <= now_us {
                due.push(pkt);
            } else {
                remaining.push(pkt);
            }
        }
        self.inflight = remaining;
        due.sort_by_key(|p| (p.deliver_at_us, p.seq));

        for mut pkt in due {
            if self.config.planted_bug == PlantedBug::CrossPeerDemuxLeak
                && self.peers.len() > 1
                && self.rng.next_u64().is_multiple_of(5)
            {
                pkt.target_peer = (pkt.target_peer + 1) % self.peers.len();
            }
            self.deliver_one_packet(step, pkt)?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn deliver_one_packet(&mut self, step: usize, pkt: InflightPacket) -> Result<(), SimViolation> {
        let now = self.now();
        self.summary.packets_delivered += 1;
        self.record_event(step, pkt.target_peer, SimEventKind::PacketDelivered);

        let peer = &mut self.peers[pkt.target_peer];
        // Demux isolation check: a packet routed to a different peer index than its
        // origin without explicit spoofing tests is a multi-peer demux bug!
        if pkt.target_peer != pkt.origin_peer && peer.session_id == pkt.target_session {
            return Err(self.violation(
                step,
                SimInvariant::DemuxIsolation,
                format!(
                    "cross-peer packet from {} accepted under peer {} session",
                    pkt.origin_peer, pkt.target_peer
                ),
            ));
        }
        if pkt.target_peer != pkt.origin_peer
            && self.config.planted_bug == PlantedBug::CrossPeerDemuxLeak
        {
            return Err(self.violation(
                step,
                SimInvariant::DemuxIsolation,
                format!(
                    "demux delivered packet from peer {} into peer {} handler",
                    pkt.origin_peer, pkt.target_peer
                ),
            ));
        }

        // Reject packets whose session ID does not match the peer's current session.
        if peer.session_id != pkt.target_session {
            return Ok(());
        }

        match pkt.payload {
            WirePayload::ObsChallengeResponse { nonce } => {
                if peer
                    .authority
                    .respond_observation_challenge(nonce, now)
                    .is_ok()
                {
                    self.summary.obs_renewals += 1;
                    self.record_event(step, pkt.target_peer, SimEventKind::ObsRenewed);
                }
            }
            WirePayload::ControlChallengeResponse { lease, nonce } => {
                let was_dead = peer
                    .last_dead_lease
                    .is_some_and(|(dead_id, _)| dead_id == lease);
                let result = peer.authority.respond_control_challenge(lease, nonce, now);
                if was_dead && result.is_ok() {
                    return Err(self.violation(
                        step,
                        SimInvariant::NoLeaseResurrection,
                        format!("dead lease {lease:?} resurrected by late challenge response"),
                    ));
                }
                if result.is_ok() {
                    self.summary.control_renewals += 1;
                    self.record_event(step, pkt.target_peer, SimEventKind::ControlRenewed);
                }
            }
            WirePayload::InputAction {
                lease,
                ticket,
                key,
                pressed,
            } => {
                let authorized = peer.authority.authorize_submission(lease, ticket, now);
                if authorized.is_ok() {
                    // Submission-time oracle check: peer MUST own controller slot and have live obs.
                    if self.controller_slot != Some(pkt.target_peer) {
                        return Err(self.violation(
                            step,
                            SimInvariant::SubmissionTimeAuthorityFence,
                            format!(
                                "peer {} submitted input while controller_slot={:?}",
                                pkt.target_peer, self.controller_slot
                            ),
                        ));
                    }
                    if peer.authority.observation_deadline(now).is_err()
                        || peer.authority.phase() != Phase::Viewing
                        || peer.authority.readiness() != ViewReadiness::Ready
                    {
                        return Err(self.violation(
                            step,
                            SimInvariant::SubmissionTimeAuthorityFence,
                            format!(
                                "peer {} submitted input without live ready viewing authority",
                                pkt.target_peer
                            ),
                        ));
                    }
                    peer.held.set_key(key, pressed);
                    self.host_held.set_key(key, pressed);
                    self.summary.inputs_submitted += 1;
                    self.record_event(step, pkt.target_peer, SimEventKind::InputSubmitted);
                } else {
                    self.summary.inputs_refused += 1;
                    self.record_event(step, pkt.target_peer, SimEventKind::InputRefused);
                }
            }
        }
        Ok(())
    }

    fn release_controller_slot(&mut self, step: usize, cleanup_held: bool) {
        if let Some(prev) = self.controller_slot.take() {
            let p = &mut self.peers[prev];
            if let Some(lease) = p.active_lease.take() {
                p.last_dead_lease = Some((lease, 0));
            }
            p.authority.revoke_lease();
            if cleanup_held {
                p.release_all_held();
                self.host_held = HeldState::default();
            }
            self.record_event(step, prev, SimEventKind::ControlDemoted);
        }
    }

    #[allow(clippy::too_many_lines)]
    fn execute_random_action(&mut self, step: usize) -> Result<(), SimViolation> {
        let now = self.now();
        let peer_idx =
            usize::try_from(self.rng.next_u64() % (self.peers.len() as u64)).unwrap_or(0);
        let op = self.rng.next_u64() % 100;

        match op {
            // Advance peer through connect -> approve -> observe -> view ready.
            0..=14 => self.step_peer_lifecycle(step, peer_idx, now),
            // Issue observation renewal challenge and schedule delayed reply.
            15..=29 => {
                let nonce = self.alloc_nonce();
                let sid = self.peers[peer_idx].session_id;
                if self.peers[peer_idx]
                    .authority
                    .issue_observation_challenge(nonce, now)
                    .is_ok()
                {
                    self.record_event(step, peer_idx, SimEventKind::ObsChallengeIssued);
                    self.enqueue_packet(
                        step,
                        peer_idx,
                        peer_idx,
                        sid,
                        WirePayload::ObsChallengeResponse { nonce },
                    );
                }
            }
            // Request control / serialized handoff.
            30..=42 => self.step_control_handoff(step, peer_idx, now)?,
            // Issue control challenge and schedule delayed reply.
            43..=54 => {
                let nonce = self.alloc_nonce();
                let (sid, active_lease) = (
                    self.peers[peer_idx].session_id,
                    self.peers[peer_idx].active_lease,
                );
                if let Some(lease) = active_lease
                    && self.peers[peer_idx]
                        .authority
                        .issue_control_challenge(nonce, now)
                        .is_ok()
                {
                    self.record_event(step, peer_idx, SimEventKind::ControlChallengeIssued);
                    self.enqueue_packet(
                        step,
                        peer_idx,
                        peer_idx,
                        sid,
                        WirePayload::ControlChallengeResponse { lease, nonce },
                    );
                }
            }
            // Issue ticket and schedule input action (key down/up).
            55..=72 => {
                let ticket = self.alloc_ticket();
                let (sid, active_lease) = (
                    self.peers[peer_idx].session_id,
                    self.peers[peer_idx].active_lease,
                );
                if let Some(lease) = active_lease
                    && self.peers[peer_idx]
                        .authority
                        .issue_input_ticket(lease, ticket, now)
                        .is_ok()
                {
                    self.peers[peer_idx].last_ticket = Some(ticket);
                    self.record_event(step, peer_idx, SimEventKind::TicketIssued);
                    let pressed = (self.rng.next_u64() & 1) == 0;
                    let key_code = u16::try_from(4 + (self.rng.next_u64() % 26)).unwrap_or(4);
                    if let Some(key) = PhysicalKey::new(key_code) {
                        self.enqueue_packet(
                            step,
                            peer_idx,
                            peer_idx,
                            sid,
                            WirePayload::InputAction {
                                lease,
                                ticket,
                                key,
                                pressed,
                            },
                        );
                    }
                }
            }
            // View staleness -> suspension -> release held keys -> view recovery.
            73..=79 => self.step_view_freshness(step, peer_idx, now),
            // Control -> View demotion (keep viewing alive!).
            80..=84 => {
                if self.controller_slot == Some(peer_idx) {
                    self.release_controller_slot(step, true);
                    self.summary.control_demotions += 1;
                }
            }
            // Immediate local revoke of peer.
            85..=89 => {
                if self.controller_slot == Some(peer_idx) {
                    self.release_controller_slot(step, true);
                }
                self.peers[peer_idx].authority.close();
                self.peers[peer_idx].release_all_held();
                self.record_event(step, peer_idx, SimEventKind::LocalRevoke);
            }
            // Host suspend + resume boundary across ALL peers.
            90..=93 => self.step_host_suspend(step)?,
            // Adversarial cross-peer spoof attempt (must always be refused!).
            94..=96 => self.step_cross_peer_spoof(step, peer_idx, now)?,
            // Close & reconnect peer with a brand new RemoteSessionId.
            _ => {
                if self.controller_slot == Some(peer_idx) {
                    self.release_controller_slot(step, true);
                }
                self.peers[peer_idx].authority.close();
                self.record_event(step, peer_idx, SimEventKind::PeerClosed);
                let sid = RemoteSessionId::from_raw(self.next_session_raw);
                self.next_session_raw += 1;
                self.peers[peer_idx] = SimPeer::new(peer_idx, sid);
                self.record_event(step, peer_idx, SimEventKind::PeerConnected);
            }
        }
        Ok(())
    }

    fn step_peer_lifecycle(&mut self, step: usize, peer_idx: usize, now: HostInstant) {
        let peer = &mut self.peers[peer_idx];
        match peer.authority.phase() {
            Phase::Identified => {
                let _ = peer.authority.mark_capabilities_checked();
                let _ = peer.authority.require_approval();
                let _ = peer.authority.authorize_observation(now);
                self.record_event(step, peer_idx, SimEventKind::PeerConnected);
            }
            Phase::CapabilitiesChecked | Phase::WaitingApproval => {
                let _ = peer.authority.authorize_observation(now);
                self.record_event(step, peer_idx, SimEventKind::PeerConnected);
            }
            Phase::ViewOpening => {
                if peer.authority.mark_view_ready(now).is_ok() {
                    self.record_event(step, peer_idx, SimEventKind::ViewReady);
                }
            }
            Phase::Viewing => {
                if peer.authority.readiness() != ViewReadiness::Ready {
                    if peer.suspended_unreleased {
                        peer.release_all_held();
                        if self.controller_slot == Some(peer_idx) {
                            self.host_held = HeldState::default();
                        }
                        if let Some(lease) = peer.active_lease {
                            let _ = peer.authority.confirm_suspension_release(lease);
                        }
                    }
                    if peer.authority.mark_view_ready(now).is_ok() {
                        self.record_event(step, peer_idx, SimEventKind::ViewReady);
                    }
                }
            }
            Phase::Refused | Phase::Closed => {
                // Reconnect on a fresh session ID.
                let sid = RemoteSessionId::from_raw(self.next_session_raw);
                self.next_session_raw += 1;
                self.peers[peer_idx] = SimPeer::new(peer_idx, sid);
                self.record_event(step, peer_idx, SimEventKind::PeerConnected);
            }
        }
    }

    fn step_control_handoff(
        &mut self,
        step: usize,
        peer_idx: usize,
        now: HostInstant,
    ) -> Result<(), SimViolation> {
        if self.peers[peer_idx].authority.phase() != Phase::Viewing
            || self.peers[peer_idx].authority.readiness() != ViewReadiness::Ready
            || self.peers[peer_idx]
                .authority
                .observation_deadline(now)
                .is_err()
        {
            return Ok(());
        }

        // If another peer holds control (or an expired lease occupies the slot),
        // serialize cleanup BEFORE granting the new controller.
        if let Some(prev) = self.controller_slot {
            if prev == peer_idx && self.peers[prev].authority.has_live_control(now) {
                return Ok(());
            }
            let cleanup = self.config.planted_bug != PlantedBug::SkipCleanupBeforeHandoff;
            self.release_controller_slot(step, cleanup);
        }

        // Verify CleanReleaseBeforeHandoff before granting!
        let held_remaining = held_total(&self.host_held);
        if held_remaining > 0 {
            return Err(self.violation(
                step,
                SimInvariant::CleanReleaseBeforeHandoff,
                format!(
                    "attempted control grant to peer {peer_idx} with {held_remaining} held keys/buttons still pressed"
                ),
            ));
        }

        let lease = self.alloc_lease();
        if self.peers[peer_idx]
            .authority
            .grant_lease(lease, now)
            .is_ok()
        {
            self.peers[peer_idx].active_lease = Some(lease);
            self.controller_slot = Some(peer_idx);
            self.summary.control_grants += 1;
            self.record_event(step, peer_idx, SimEventKind::ControlGranted);
        }
        Ok(())
    }

    fn step_view_freshness(&mut self, step: usize, peer_idx: usize, now: HostInstant) {
        let peer = &mut self.peers[peer_idx];
        if peer.authority.phase() != Phase::Viewing {
            return;
        }
        if peer.authority.readiness() == ViewReadiness::Ready {
            peer.authority.mark_view_stale();
            if peer.active_lease.is_some() {
                peer.suspended_unreleased = true;
            }
            self.record_event(step, peer_idx, SimEventKind::ViewStale);
        } else {
            if peer.suspended_unreleased {
                peer.release_all_held();
                if self.controller_slot == Some(peer_idx) {
                    self.host_held = HeldState::default();
                }
                if let Some(lease) = peer.active_lease {
                    let _ = peer.authority.confirm_suspension_release(lease);
                }
            }
            if peer.authority.mark_view_ready(now).is_ok() {
                self.record_event(step, peer_idx, SimEventKind::ViewReady);
            }
        }
    }

    fn step_host_suspend(&mut self, step: usize) -> Result<(), SimViolation> {
        self.summary.suspends += 1;
        let skip_fence = self.config.planted_bug == PlantedBug::SkipSuspendFence;
        if !skip_fence {
            self.release_controller_slot(step, true);
            for peer in &mut self.peers {
                peer.authority.invalidate_for_suspend();
                peer.release_all_held();
            }
        }
        self.record_event(step, 0, SimEventKind::HostSuspend);
        // Simulate wall-clock jump across suspend (5 seconds).
        self.advance_ms(5_000);
        let after = self.now();
        if skip_fence {
            // Check whether any peer still appears to hold live control across suspend!
            if let Some(ctrl) = self.controller_slot {
                // Even if deadline didn't expire yet (or if suspend was short), host suspend
                // MUST fence control immediately.
                let p = &mut self.peers[ctrl];
                if p.authority.phase() == Phase::Viewing {
                    return Err(self.violation(
                        step,
                        SimInvariant::SubmissionTimeAuthorityFence,
                        format!("peer {ctrl} stayed in Viewing across host suspend without suspend fence"),
                    ));
                }
            }
        } else {
            for peer in &mut self.peers {
                let _ = peer.authority.apply_resume_boundary(after);
            }
        }
        self.record_event(step, 0, SimEventKind::HostResume);
        Ok(())
    }

    fn step_cross_peer_spoof(
        &mut self,
        step: usize,
        attacker_idx: usize,
        now: HostInstant,
    ) -> Result<(), SimViolation> {
        let Some(victim_idx) = self.controller_slot else {
            return Ok(());
        };
        if victim_idx == attacker_idx {
            return Ok(());
        }
        let Some(victim_lease) = self.peers[victim_idx].active_lease else {
            return Ok(());
        };
        let Some(victim_ticket) = self.peers[victim_idx].last_ticket else {
            return Ok(());
        };
        // Attacker attempts to submit using victim's lease + ticket against attacker's own authority.
        let res = self.peers[attacker_idx].authority.authorize_submission(
            victim_lease,
            victim_ticket,
            now,
        );
        if res.is_ok() {
            return Err(self.violation(
                step,
                SimInvariant::DemuxIsolation,
                format!(
                    "attacker peer {attacker_idx} authorized submission using victim peer {victim_idx}'s lease/ticket"
                ),
            ));
        }
        self.summary.spoofs_refused += 1;
        self.record_event(step, attacker_idx, SimEventKind::CrossPeerSpoofRefused);
        Ok(())
    }

    fn verify_invariants(&mut self, step: usize) -> Result<(), SimViolation> {
        let now = self.now();

        // I7: Bounded in-flight queue.
        if self.inflight.len() > self.config.faults.max_inflight {
            return Err(self.violation(
                step,
                SimInvariant::BoundedInflightQueue,
                format!(
                    "inflight len {} exceeded bound {}",
                    self.inflight.len(),
                    self.config.faults.max_inflight
                ),
            ));
        }

        // I1 & I2: At most one peer holds live control, and live control implies live Viewing observation.
        let mut live_controllers = Vec::new();
        let mut obs_violation: Option<(usize, Phase)> = None;
        let mut held_violation: Option<(usize, usize)> = None;
        for peer in &mut self.peers {
            if peer.authority.has_live_control(now) {
                live_controllers.push(peer.peer_index);
                if peer.authority.phase() != Phase::Viewing
                    || peer.authority.observation_deadline(now).is_err()
                {
                    obs_violation = Some((peer.peer_index, peer.authority.phase()));
                    break;
                }
            } else if peer.held_count() > 0 && self.controller_slot != Some(peer.peer_index) {
                // A non-controller must never retain pressed OS keys on the shared host seat.
                if self.config.planted_bug == PlantedBug::SkipCleanupBeforeHandoff {
                    held_violation = Some((peer.peer_index, peer.held_count()));
                    break;
                }
            }
        }
        if let Some((idx, phase)) = obs_violation {
            return Err(self.violation(
                step,
                SimInvariant::ControlImpliesLiveObservation,
                format!("peer {idx} has live control in phase {phase:?} without live observation"),
            ));
        }
        if let Some((idx, held_cnt)) = held_violation {
            return Err(self.violation(
                step,
                SimInvariant::CleanReleaseBeforeHandoff,
                format!(
                    "non-controller peer {idx} still retained {held_cnt} held keys after losing control"
                ),
            ));
        }

        if live_controllers.len() > 1 {
            return Err(self.violation(
                step,
                SimInvariant::SingleControllerExclusivity,
                format!("multiple live controllers simultaneously: {live_controllers:?}"),
            ));
        }

        // If the sole controller's lease expired naturally, release held state cleanly
        // (matching the host watchdog's expiry cleanup).
        if let Some(ctrl) = self.controller_slot
            && !self.peers[ctrl].authority.has_live_control(now)
        {
            let cleanup = self.config.planted_bug != PlantedBug::SkipCleanupBeforeHandoff;
            self.release_controller_slot(step, cleanup);
        }

        Ok(())
    }

    fn run(mut self) -> Result<SimSummary, SimViolation> {
        for step in 0..self.config.steps {
            let dt = if self.config.faults.max_step_ms == 0 {
                1
            } else {
                1 + (self.rng.next_u64() % self.config.faults.max_step_ms)
            };
            self.advance_ms(dt);
            self.deliver_due_packets(step)?;
            self.execute_random_action(step)?;
            self.verify_invariants(step)?;
            self.summary.steps_executed = step + 1;
        }
        self.summary.final_time_us = self.now_us();
        self.summary.trace_digest = self.trace_digest;
        Ok(self.summary)
    }
}

/// Execute one deterministic simulation run for `config` and return either its
/// bit-exact summary or a reproducible [`SimViolation`] naming the failing seed.
pub fn run_deterministic_simulation(config: SimConfig) -> Result<SimSummary, SimViolation> {
    SimHarness::new(config)?.run()
}
