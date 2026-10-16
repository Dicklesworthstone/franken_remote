//! The idle host broker follows the same, positively negotiated media successor.
//! These checks never create input authority or replace the input attachment.
use super::{Error, NegotiatedInput, Routes, recovered_successor};
use crate::media_quic::NegotiatedMedia;
use fr_core::limits::ProtocolLimits;
use fr_transport::quic::{Messages, QuicRecords};
use fr_wire::{attachment::MediaRole, negotiation::ControlBinding};

impl NegotiatedInput {
    pub(in crate::input_quic) fn begin_host_recovery(
        &mut self,
        q: &QuicRecords,
    ) -> Result<(), Error> {
        if !self.reference_recovery || self.recovering {
            return Err(Error::InvalidRoutes);
        }
        // Before retiring configuration, validate the complete original proof.
        self.broker_routes(q, self.parent, self.limits)?;
        self.recovering = true;
        Ok(())
    }
    /// Only broker liveness may use these routes during the configuration gap.
    /// Ordinary grant admission still calls `broker_routes` and therefore refuses.
    pub(in crate::input_quic) fn recovering_host_routes(
        &self,
        q: &QuicRecords,
        parent: ControlBinding,
        limits: ProtocolLimits,
    ) -> Result<Routes, Error> {
        if !self.recovering || parent != self.parent || limits != self.limits {
            return Err(Error::InvalidRoutes);
        }
        let attached = self.checked(q)?;
        Routes::new(attached.inbound, attached.outbound, attached.datagram)
    }
    pub(in crate::input_quic) fn follow_host_recovery(
        &mut self,
        q: &QuicRecords,
        media: &NegotiatedMedia,
        until: u64,
    ) -> Result<(), Error> {
        self.recovering_host_routes(q, self.parent, self.limits)?;
        if *media.limits().protocol() != self.limits {
            return Err(Error::InvalidRoutes);
        }
        // Checks the media's own connection, positive recovery negotiation,
        // exact next generation, live channels and existing decoder contract.
        media
            .recovery_decoder_setup(q, self.configuration.descriptor.binding, until)
            .map_err(|_| Error::InvalidRoutes)?;
        let configuration = media.configuration_channel();
        if configuration.outbound.messages != Messages::MediaConfiguration
            || configuration.inbound.messages != Messages::DecoderReplies
            || configuration.descriptor.role != MediaRole::Configuration
            || configuration.descriptor.binding.parent.id == self.channel_binding()
            || !recovered_successor(
                self.configuration.descriptor.binding,
                configuration.descriptor.binding,
            )
        {
            return Err(Error::InvalidRoutes);
        }
        let previous = (self.configuration, self.recovery, self.recovering);
        self.configuration = configuration;
        self.recovery = configuration.descriptor.binding.recovery;
        self.recovering = false;
        if let Err(error) = self.broker_routes(q, self.parent, self.limits) {
            (self.configuration, self.recovery, self.recovering) = previous;
            return Err(error);
        }
        Ok(())
    }
}
