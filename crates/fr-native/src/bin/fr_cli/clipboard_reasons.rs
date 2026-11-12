//! Shared content-free reason names for native clipboard completion.

/// An active clipboard retired before its original session ended.
pub(crate) fn ended(reason: Option<frd::clipboard_quic::Error>) -> &'static str {
    use fr_core::clipboard::Error as Item;
    use fr_wire::clipboard::session::SessionError;
    use frd::clipboard_quic::Error;
    match reason {
        Some(Error::ReceiveExpired) => "clipboard_receive_expired",
        Some(Error::SendExpired) => "clipboard_send_expired",
        Some(Error::HandoffExpired | Error::Session(SessionError::Clipboard(Item::Expired))) => {
            "clipboard_item_expired"
        }
        Some(Error::SwitchChanged | Error::Session(SessionError::Clipboard(Item::Disabled))) => {
            "clipboard_disabled"
        }
        Some(
            Error::Controller(_) | Error::Session(SessionError::Clipboard(Item::Authority(_))),
        ) => "clipboard_authority_ended",
        _ => "clipboard_ended",
    }
}

/// A requested clipboard ended without becoming active.
pub(crate) fn absence(reason: Option<frd::clipboard_quic::Error>) -> &'static str {
    use frd::clipboard_quic::Error;
    match reason {
        Some(Error::ReceiveExpired) => "clipboard_receive_expired",
        Some(Error::SendExpired) => "clipboard_send_expired",
        Some(Error::NotNegotiated) => "host_clipboard_unavailable",
        Some(Error::ConsentRequired) => "host_clipboard_refused",
        Some(Error::NativeSetup(_)) => "local_clipboard_unavailable",
        _ => "clipboard_ended",
    }
}
