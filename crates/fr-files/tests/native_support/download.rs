//! Genuine accepted-grant authority plus the existing real UDP/TLS attachment.
//! Admission and presentation are explicit local fixtures, not a live desktop.
use super::*;
use fr_client::{
    control_grant::RequestControl,
    input::{ClientInstant, InputClient, Policy as InputPolicy, PresentedObservation},
};
use fr_files::session::Authority;
use fr_media::freshness::{ClockCorrelation, ClockPolicy, ClockSample};
use fr_wire::{
    control::{self, Granted, Request, Target},
    input::{InputDelivery, InputDirection},
    sync_job_state,
};

impl Link {
    /// Real upload attachment and the original accepted controller grant.
    /// Both legacy upload-only and independently bound duplex upload lanes
    /// exercise the same production folder owners.
    pub async fn upload_sync(
        &mut self,
        cx: &Cx,
        sync: bool,
        duplex: bool,
    ) -> (
        FilesChannel,
        FilesChannel,
        InputSession,
        InputClient,
        Authority,
    ) {
        self.upload_sync_version(cx, sync.then_some(sync_job_state::UPLOAD_VERSION), duplex)
            .await
    }

    pub async fn upload_sync_version(
        &mut self,
        cx: &Cx,
        version: Option<u16>,
        duplex: bool,
    ) -> (
        FilesChannel,
        FilesChannel,
        InputSession,
        InputClient,
        Authority,
    ) {
        self.selection.role = Role::RequestControl;
        for name in [
            attachment::INPUT_CAPABILITY,
            attachment::FILES_CAPABILITY,
            files::CAPABILITY,
            files::CHANNEL_SCOPE_CAPABILITY,
        ] {
            self.selection.capabilities.push(Capability {
                name: name.into(),
                version: 1,
                required: true,
            });
        }
        if duplex {
            for name in [files::DOWNLOAD_CAPABILITY, files::DUPLEX_CAPABILITY] {
                self.selection.capabilities.push(Capability {
                    name: name.into(),
                    version: 1,
                    required: true,
                });
            }
        }
        if let Some(version) = version {
            self.selection.capabilities.push(Capability {
                name: sync_job_state::UPLOAD_CAPABILITY.into(),
                version,
                required: false,
            });
        }
        self.selection
            .capabilities
            .sort_by(|a, b| a.name.cmp(&b.name));
        let _input = channel(self, cx, 8, MediaRole::Input).await;
        let (host, client) = channel(self, cx, 9, MediaRole::Files).await;
        let handle = 9;
        let host = FilesChannel::new(&self.h, host, InputLeaseId::from_raw(2), handle).unwrap();
        let client = FilesChannel::new(&self.c, client, InputLeaseId::from_raw(2), handle).unwrap();
        let input = owner(cx);
        let (client_input, authority) = accepted(cx);
        (host, client, input, client_input, authority)
    }

    pub async fn download(
        &mut self,
        cx: &Cx,
        sync: bool,
    ) -> (
        FilesChannel,
        FilesChannel,
        InputSession,
        InputClient,
        Authority,
    ) {
        self.download_sync_version(cx, sync.then_some(sync_job_state::VERSION))
            .await
    }

    pub async fn download_sync_version(
        &mut self,
        cx: &Cx,
        version: Option<u16>,
    ) -> (
        FilesChannel,
        FilesChannel,
        InputSession,
        InputClient,
        Authority,
    ) {
        self.selection.role = Role::RequestControl;
        for name in [
            attachment::INPUT_CAPABILITY,
            attachment::FILES_CAPABILITY,
            files::CAPABILITY,
            files::DOWNLOAD_CAPABILITY,
        ] {
            self.selection.capabilities.push(Capability {
                name: name.into(),
                version: 1,
                required: true,
            });
        }
        if let Some(version) = version {
            self.selection.capabilities.push(Capability {
                name: sync_job_state::CAPABILITY.into(),
                version,
                required: false,
            });
        }
        self.selection
            .capabilities
            .sort_by(|a, b| a.name.cmp(&b.name));
        let _input = channel(self, cx, 8, MediaRole::Input).await;
        let (host, client) = channel(self, cx, 9, MediaRole::Files).await;
        let host = FilesChannel::new(&self.h, host, InputLeaseId::from_raw(2), 456).unwrap();
        let client = FilesChannel::new(&self.c, client, InputLeaseId::from_raw(2), 456).unwrap();
        let input = owner(cx);
        let (client_input, authority) = accepted(cx);
        (host, client, input, client_input, authority)
    }
}

pub(crate) fn accepted(cx: &Cx) -> (InputClient, Authority) {
    let now = clock(cx);
    let at = ClientInstant(now);
    let target = binding(14);
    let grant = Granted {
        request: Request {
            parent: parent(),
            sequence: 1,
            target: Target {
                display_binding: 14,
                view: fr_core::input::InputView {
                    geometry: target.geometry,
                    viewport: target.viewport,
                    configuration: target.configuration,
                    recovery: target.recovery,
                },
                bounds: InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
                capabilities: Capabilities::default()
                    .with(fr_core::input_submission::Capability::Keys),
            },
        },
        input_channel: 8,
        lease: InputLeaseId::from_raw(2),
        ticket: InputTicketId::from_raw(3),
        issued_at_us: now,
        lease_until_us: now + 3_000_000,
        ticket_until_us: now + 1_000_000,
        first_action: 0,
        first_pointer: 0,
    };
    let mut request = RequestControl::new(grant.request, 8, ProtocolLimits::ABSOLUTE, at).unwrap();
    request.sent(at).unwrap();
    let mut bytes = [0; control::GRANTED_BYTES];
    control::encode_granted(
        grant,
        &mut bytes,
        &ProtocolLimits::ABSOLUTE,
        InputDirection::HostToViewer,
        InputDelivery::Reliable,
    )
    .unwrap();
    let clock = ClockCorrelation::new(
        ClockSample {
            host_boot: parent().host_boot,
            client_sent_us: now,
            client_received_us: now,
            host_sample_us: now,
        },
        ClockPolicy {
            drift_ppm: 0,
            ..ClockPolicy::default()
        },
    )
    .unwrap();
    let mut input = request
        .accept(
            &bytes,
            clock,
            InputPolicy {
                view_age_us: 1_500_000,
                receipt_timeout_us: 500_000,
            },
            at,
        )
        .unwrap()
        .1;
    let credentials = grant.credentials();
    input
        .confirm_mapping(credentials.session, credentials.view, at)
        .unwrap();
    input
        .presented(
            PresentedObservation {
                session: credentials.session,
                serial: 0,
                view: credentials.view,
                received_at: at,
                source_age_upper_us: 0,
            },
            at,
        )
        .unwrap();
    let authority = input
        .file_receive_authority(parent(), 9, grant.lease, at)
        .unwrap();
    (input, Authority::from_controller(authority))
}
