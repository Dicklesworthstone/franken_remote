use super::{
    Begin, ClipboardSession, ClipboardSink, Error, HostDuration, HostInstant, Incoming,
    MAX_CHUNK_BYTES, Receipt, Stamp, Text,
};

struct Prepared<'a, S: ClipboardSink>(&'a mut S);
impl<S: ClipboardSink> Drop for Prepared<'_, S> {
    fn drop(&mut self) {
        self.0.cancel_prepared();
    }
}
impl ClipboardSession {
    pub fn begin(&mut self, begin: Begin, now: HostInstant) -> Result<(), Error> {
        let authority_deadline = self.check(now)?;
        begin.validate(&self.limits)?;
        if begin.binding != self.binding {
            return Err(Error::Binding);
        }
        if begin.stamp.source != self.local.opposite() {
            return Err(Error::Source);
        }
        if begin.stamp.sequence <= self.received_floor {
            return Err(Error::Replay);
        }
        if self.incoming.is_some() {
            return Err(Error::Busy);
        }
        let deadline = now
            .checked_add(HostDuration::from_micros(3_000_000))
            .ok_or(Error::Clock)?
            .min(authority_deadline);
        // Consume once, including allocation/cancellation failures. A retry is
        // a genuinely new item, never resurrection of this transfer identity.
        self.received_floor = begin.stamp.sequence;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(begin.total_bytes as usize)
            .map_err(|_| Error::Allocation)?;
        self.incoming = Some(Incoming {
            begin,
            bytes: Text(bytes),
            next_chunk: 0,
            deadline,
            local_revision: self.local_revision,
        });
        Ok(())
    }
    pub fn chunk(
        &mut self,
        stamp: Stamp,
        index: u32,
        offset: u32,
        bytes: &[u8],
        now: HostInstant,
    ) -> Result<(), Error> {
        self.maintain(now)?;
        let transfer = self.incoming.as_mut().ok_or(Error::UnknownTransfer)?;
        if transfer.begin.stamp != stamp {
            return Err(Error::UnknownTransfer);
        }
        let end = transfer
            .bytes
            .0
            .len()
            .checked_add(bytes.len())
            .ok_or(Error::Limit)?;
        if index != transfer.next_chunk
            || offset as usize != transfer.bytes.0.len()
            || index >= transfer.begin.chunks
            || bytes.is_empty()
            || bytes.len() > MAX_CHUNK_BYTES
            || end > transfer.begin.total_bytes as usize
        {
            self.incoming = None;
            return Err(Error::ChunkOrder);
        }
        // Begin reserved the entire admitted item; no per-chunk allocation or
        // attacker-controlled metadata collection exists.
        transfer.bytes.0.extend_from_slice(bytes);
        transfer.next_chunk += 1;
        Ok(())
    }
    /// Cancel is release-only and remains valid after local disable/closure.
    /// An unrelated transfer ID cannot discard a newer in-flight item.
    pub fn cancel(&mut self, stamp: Stamp) -> Result<(), Error> {
        if self
            .incoming
            .as_ref()
            .is_some_and(|v| v.begin.stamp == stamp)
        {
            self.incoming = None;
            Ok(())
        } else {
            Err(Error::UnknownTransfer)
        }
    }
    /// Complete UTF-8 and the immutable declared total are checked before any
    /// native call. Every accepted commit is consumed even if the platform
    /// refuses or its external effect is unknown; it is never retried here.
    pub fn commit(
        &mut self,
        stamp: Stamp,
        total_bytes: u32,
        sink: &mut impl ClipboardSink,
        mut clock: impl FnMut() -> HostInstant,
    ) -> Result<Receipt, Error> {
        self.maintain(clock())?;
        if let Some((receipt, total)) = self.published.filter(|(r, _)| r.stamp == stamp) {
            if total != total_bytes {
                return Err(Error::Incomplete);
            }
            return Ok(receipt);
        }
        let pending = self.incoming.as_ref().ok_or(Error::UnknownTransfer)?;
        if pending.begin.stamp != stamp {
            return Err(Error::UnknownTransfer);
        }
        let transfer = self.incoming.take().ok_or(Error::UnknownTransfer)?;
        if total_bytes != transfer.begin.total_bytes
            || total_bytes as usize != transfer.bytes.0.len()
            || transfer.next_chunk != transfer.begin.chunks
        {
            return Err(Error::Incomplete);
        }
        if transfer.local_revision != self.local_revision {
            return Err(Error::LocalChanged);
        }
        let text = core::str::from_utf8(&transfer.bytes.0).map_err(|_| Error::InvalidUtf8)?;
        let prepared = Prepared(sink);
        prepared.0.prepare(text, stamp).map_err(Error::Platform)?;
        let now = clock();
        self.check(now)?;
        if now >= transfer.deadline {
            return Err(Error::Expired);
        }
        let publication = prepared.0.publish(text, stamp);
        let receipt = Receipt { stamp, publication };
        // In particular UnknownEffect is retained, not reclassified as safe to
        // retry. Setting the clipboard says nothing about application paste.
        self.published = Some((receipt, total_bytes));
        Ok(receipt)
    }
}
