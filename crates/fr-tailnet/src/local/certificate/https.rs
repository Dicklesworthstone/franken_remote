//! HTTPS handshakes retain the original installed credential lifetime.
use super::{
    Error, HTTPS_ALPN, HTTPS_HANDSHAKE_BUDGET, NativeServerIdentity, NodeIdentity, Pair, bounded,
    now, wall_now,
};
use asupersync::{cx::Cx, net::TcpStream, tls::TlsStream};
use std::{
    future::{Future, poll_fn},
    pin::pin,
    sync::Arc,
};

impl NativeServerIdentity {
    /// Accept TLS 1.3 with exactly HTTP/1.1 and this installed node's SNI.
    /// The socket must already belong to the caller's protected TCP listener.
    /// This authenticates the host only: original peer admission, exact Origin,
    /// local observation approval and session teardown remain separate gates.
    ///
    /// Obtain a fresh node snapshot for every handshake. The original node and
    /// selected certificate remain checked while TLS waits; rotation cannot
    /// extend an old certificate's validity. No key or reusable acceptor leaves
    /// this owner. Failure, cancellation or dropping this future closes its
    /// unfinished socket before any application session can acquire it.
    pub async fn https_accept(
        &self,
        cx: &Cx,
        node: &NodeIdentity,
        tcp: TcpStream,
    ) -> Result<TlsStream<TcpStream>, Error> {
        let pair = self.https_pair(cx, node)?;
        let mut handshake = pin!(pair.https.accept(tcp));
        let checked = poll_fn(|task| {
            self.check_https_pair(cx, node, &pair)?;
            let result = handshake
                .as_mut()
                .poll(task)
                .map_err(|_| Error::HttpsHandshake);
            self.check_https_pair(cx, node, &pair)?;
            result
        });
        let stream =
            bounded(cx, HTTPS_HANDSHAKE_BUDGET, checked)
                .await
                .map_err(|error| match error {
                    Error::Timeout => Error::HandshakeTimeout,
                    error => error,
                })?;
        if stream.alpn_protocol() != Some(HTTPS_ALPN)
            || stream.protocol_version() != Some(0x0304u16.into())
            || !stream
                .sni_hostname()
                .is_some_and(|name| name.eq_ignore_ascii_case(node.certificate_name()))
        {
            return Err(Error::HttpsHandshake);
        }
        pair.verify(&self.verifier, node.certificate_name())?;
        self.check_https_pair(cx, node, &pair)?;
        Ok(stream)
    }

    fn https_pair(&self, cx: &Cx, node: &NodeIdentity) -> Result<Arc<Pair>, Error> {
        self.api.check_node(cx, node)?;
        let pair = {
            let mut state = self.state()?;
            state.check(now(cx)?, wall_now()?)?;
            if !state.anchor.same_node(node) {
                state.active = None;
                return Err(Error::IdentityChanged);
            }
            state.active.as_ref().ok_or(Error::Revoked)?.clone()
        };
        pair.verify(&self.verifier, node.certificate_name())?;
        self.check_https_pair(cx, node, &pair)?;
        Ok(pair)
    }

    fn check_https_pair(&self, cx: &Cx, node: &NodeIdentity, pair: &Pair) -> Result<(), Error> {
        self.api.check_node(cx, node)?;
        let mut state = self.state()?;
        state.check(now(cx)?, wall_now()?)?;
        if !state.anchor.same_node(node) {
            state.active = None;
            return Err(Error::IdentityChanged);
        }
        // A different, newly rotated active certificate does not renew this
        // handshake's selected leaf. Use the owner's conservative wall clock.
        if state.last_wall_us >= pair.not_after_wall_us {
            return Err(Error::Expired);
        }
        Ok(())
    }
}
