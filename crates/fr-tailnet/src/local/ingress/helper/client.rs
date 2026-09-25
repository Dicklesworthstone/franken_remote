//! The broker side. The connection is the lease: the helper removes the rule
//! when the last descriptor of it closes. Partial writes and reads persist in
//! this owner, so an abandoned (cancelled) exchange is completed and its reply
//! discarded before the next request instead of desynchronizing the stream.
use super::{
    super::{Configuration, Error, Leaf, protected, validate_rule},
    Install, MAX_RESPONSE, Request, Response, decode_response, encode_request, owned_table,
};
use crate::{Error as IdentityError, local::bounded};
use asupersync::{
    cx::Cx,
    io::{AsyncReadExt, AsyncWriteExt},
    net::unix::UnixStream,
};
use std::{path::Path, time::Duration};

/// Covers the helper's worst case: an interface report plus two nft commands,
/// each bounded to one second.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(4);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);

pub(in crate::local::ingress) struct Link {
    stream: UnixStream,
    generation: u64,
    table: String,
    outbox: Vec<u8>,
    inbox: Vec<u8>,
    outstanding: u8,
    broken: bool,
}
impl Link {
    /// Connect to a root-only socket path, require a root peer, and ask for
    /// the configured rule. The returned descriptor duplicate keeps the rule
    /// alive in every lease; the read-back is checked with `validate_rule`.
    pub(in crate::local::ingress) async fn install(
        cx: &Cx,
        socket: &Path,
        config: &Configuration,
        index: u32,
    ) -> Result<(Self, std::os::unix::net::UnixStream), Error> {
        protected(socket, Leaf::Socket).ok_or(Error::HelperUnavailable)?;
        let stream = Box::pin(bounded(cx, CONNECT_TIMEOUT, async {
            UnixStream::connect(socket)
                .await
                .map_err(|_| IdentityError::LocalApiUnavailable)
        }))
        .await
        .map_err(|_| Error::HelperUnavailable)?;
        let peer = stream.peer_cred().map_err(|_| Error::HelperUnavailable)?;
        if peer.uid != 0 || peer.pid.is_none_or(|pid| pid <= 0) {
            return Err(Error::HelperUnavailable);
        }
        let keepalive = stream
            .as_std()
            .try_clone()
            .map_err(|_| Error::HelperUnavailable)?;
        let mut link = Self {
            stream,
            generation: 0,
            table: String::new(),
            outbox: Vec::new(),
            inbox: Vec::new(),
            outstanding: 0,
            broken: false,
        };
        let request = Request::Install(Install {
            interface: config.interface.clone(),
            address: config.address.ip(),
            port: config.address.port(),
            protocols: config.protocols,
        });
        match link.call(cx, &request).await? {
            Response::Installed {
                generation,
                index: reported,
                table,
                readback,
            } => {
                owned_table(&table).map_err(|_| Error::FirewallMismatch)?;
                if reported != index {
                    return Err(Error::InterfaceChanged);
                }
                validate_rule(
                    &readback,
                    &table,
                    config.address,
                    config.protocols,
                    index,
                    &config.interface,
                )?;
                link.generation = generation;
                link.table = table;
                Ok((link, keepalive))
            }
            Response::Refused(reason) => Err(Error::HelperRefused(reason)),
            _ => Err(Error::HelperUnavailable),
        }
    }
    pub(in crate::local::ingress) fn table(&self) -> &str {
        &self.table
    }
    /// The helper re-qualifies the interface/address and returns its root
    /// read-back of this generation's table, for the caller to validate.
    pub(in crate::local::ingress) async fn renew(&mut self, cx: &Cx) -> Result<Vec<u8>, Error> {
        let generation = self.generation;
        match self.call(cx, &Request::Renew { generation }).await? {
            Response::Renewed {
                generation: renewed,
                readback,
            } if renewed == generation => Ok(readback),
            Response::Refused(reason) => Err(Error::HelperRefused(reason)),
            _ => Err(Error::HelperUnavailable),
        }
    }
    /// Acknowledged removal of this generation's table.
    pub(in crate::local::ingress) async fn remove(&mut self, cx: &Cx) -> Result<(), Error> {
        let generation = self.generation;
        match self.call(cx, &Request::Remove { generation }).await? {
            Response::Removed {
                generation: removed,
            } if removed == generation => Ok(()),
            Response::Refused(reason) => Err(Error::HelperRefused(reason)),
            _ => Err(Error::HelperUnavailable),
        }
    }
    async fn call(&mut self, cx: &Cx, request: &Request) -> Result<Response, Error> {
        if self.broken {
            return Err(Error::HelperUnavailable);
        }
        let body = Box::pin(bounded(cx, EXCHANGE_TIMEOUT, async {
            // Complete what an abandoned call left behind, discarding its reply.
            self.flush().await?;
            while self.outstanding > 0 {
                self.receive().await?;
                self.outstanding -= 1;
            }
            self.outbox.extend(encode_request(request));
            self.outstanding = 1;
            if let Err(error) = self.flush().await {
                // A helper that refuses at accept (SO_PEERCRED, capacity, rate)
                // closes before reading; its typed refusal is still queued.
                return self.receive().await.map_err(|_| error);
            }
            let body = self.receive().await?;
            self.outstanding = 0;
            Ok(body)
        }))
        .await;
        match body {
            Ok(body) => decode_response(&body).map_err(|_| {
                self.broken = true;
                Error::HelperUnavailable
            }),
            // An I/O or framing failure (including EOF: the helper or its
            // connection is gone) is terminal for this link. A timeout or
            // cancellation keeps the persisted state for the next call.
            Err(IdentityError::LocalApiUnavailable | IdentityError::MalformedMetadata) => {
                self.broken = true;
                Err(Error::HelperUnavailable)
            }
            Err(_) => Err(Error::HelperUnavailable),
        }
    }
    async fn flush(&mut self) -> Result<(), IdentityError> {
        while !self.outbox.is_empty() {
            let written = self
                .stream
                .write(&self.outbox)
                .await
                .map_err(|_| IdentityError::LocalApiUnavailable)?;
            if written == 0 {
                return Err(IdentityError::LocalApiUnavailable);
            }
            self.outbox.drain(..written);
        }
        Ok(())
    }
    async fn receive(&mut self) -> Result<Vec<u8>, IdentityError> {
        loop {
            if let Some(header) = self.inbox.get(..4) {
                let length = usize::try_from(u32::from_be_bytes(
                    header
                        .try_into()
                        .map_err(|_| IdentityError::MalformedMetadata)?,
                ))
                .map_err(|_| IdentityError::MalformedMetadata)?;
                if length == 0 || length > MAX_RESPONSE {
                    return Err(IdentityError::MalformedMetadata);
                }
                if self.inbox.len() >= 4 + length {
                    let body = self.inbox[4..4 + length].to_vec();
                    self.inbox.drain(..4 + length);
                    return Ok(body);
                }
            }
            let mut buffer = [0_u8; 4096];
            let read = self
                .stream
                .read(&mut buffer)
                .await
                .map_err(|_| IdentityError::LocalApiUnavailable)?;
            if read == 0 {
                return Err(IdentityError::LocalApiUnavailable);
            }
            if self.inbox.len() + read > 4 + MAX_RESPONSE {
                return Err(IdentityError::MalformedMetadata);
            }
            self.inbox.extend_from_slice(&buffer[..read]);
        }
    }
}
