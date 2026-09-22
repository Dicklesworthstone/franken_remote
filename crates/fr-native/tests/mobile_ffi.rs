//! Unit tests for the mobile FFI C-ABI and lifecycle boundaries.
//!
//! Per ADR 0003 and Plan §16.2:
//! - Lifecycle-safe handle semantics (generation checked).
//! - Stale handles return FR_ERR_STALE_HANDLE (-2).
//! - Double-free safely rejected without undefined behavior.
//! - Callbacks, surface attachments, and input submission validated.

use fr_native::mobile_ffi::*;
use std::ffi::{c_void, CString};
use std::ptr;

#[test]
fn test_mobile_session_lifecycle_and_stale_handles() {
    assert_eq!(fr_mobile_init(), FR_OK);

    let host = CString::new("100.64.0.1:4710").unwrap();
    let token = CString::new("test-token-1234").unwrap();
    let mut session_handle: u64 = 0;

    // 1. Valid creation
    unsafe {
        assert_eq!(
            fr_session_create(host.as_ptr(), token.as_ptr(), &raw mut session_handle),
            FR_OK
        );
    }
    assert_ne!(session_handle, 0);

    // 2. Connect
    assert_eq!(fr_session_connect(session_handle), FR_OK);

    // 3. Input submission on connected session
    assert_eq!(fr_session_send_pointer(session_handle, 100, 200, 0, 1), FR_OK);
    assert_eq!(fr_session_send_key(session_handle, 65, 0), FR_OK);
    assert_eq!(fr_session_send_scroll(session_handle, 0, -10), FR_OK);

    // 4. Invalid input parameters rejected
    assert_eq!(
        fr_session_send_pointer(session_handle, 100, 200, 99, 1),
        FR_ERR_INVALID_ARGUMENT
    );
    assert_eq!(
        fr_session_send_key(session_handle, 65, 5),
        FR_ERR_INVALID_ARGUMENT
    );

    // 5. Text submission
    let text = CString::new("hello world").unwrap();
    unsafe {
        assert_eq!(fr_session_send_text(session_handle, text.as_ptr()), FR_OK);
        assert_eq!(
            fr_session_send_text(session_handle, ptr::null()),
            FR_ERR_INVALID_ARGUMENT
        );
    }

    // 6. Surface attachments (zero-copy presentation pointers)
    let dummy_metal_layer = 0x1234_5678 as *mut c_void;
    let dummy_native_window = 0x8765_4321 as *mut c_void;
    unsafe {
        assert_eq!(
            fr_session_attach_metal_layer(session_handle, dummy_metal_layer),
            FR_OK
        );
        assert_eq!(
            fr_session_attach_native_window(session_handle, dummy_native_window),
            FR_OK
        );
    }

    // 7. Mic toggle
    assert_eq!(fr_session_set_mic_enabled(session_handle, true), FR_OK);
    assert_eq!(fr_session_set_mic_enabled(session_handle, false), FR_OK);

    // 8. Quality query
    let mut quality = FrConnectionQuality::default();
    unsafe {
        assert_eq!(
            fr_session_get_quality(session_handle, &raw mut quality),
            FR_OK
        );
        assert_eq!(
            fr_session_get_quality(session_handle, ptr::null_mut()),
            FR_ERR_INVALID_ARGUMENT
        );
    }

    // 9. Disconnect
    assert_eq!(fr_session_disconnect(session_handle), FR_OK);

    // 10. Input after disconnect rejected
    assert_eq!(
        fr_session_send_pointer(session_handle, 10, 20, 0, 0),
        FR_ERR_ALREADY_CLOSED
    );

    // 11. Destroy session
    assert_eq!(fr_session_destroy(session_handle), FR_OK);

    // 12. Stale handle access returns FR_ERR_STALE_HANDLE (-2)
    assert_eq!(fr_session_connect(session_handle), FR_ERR_STALE_HANDLE);
    assert_eq!(fr_session_disconnect(session_handle), FR_ERR_STALE_HANDLE);
    assert_eq!(
        fr_session_send_pointer(session_handle, 10, 20, 0, 0),
        FR_ERR_STALE_HANDLE
    );
    unsafe {
        assert_eq!(
            fr_session_attach_metal_layer(session_handle, dummy_metal_layer),
            FR_ERR_STALE_HANDLE
        );
    }

    // 13. Double-free safety: calling destroy twice returns FR_ERR_STALE_HANDLE without panic
    assert_eq!(fr_session_destroy(session_handle), FR_ERR_STALE_HANDLE);
}

#[test]
fn test_mobile_session_creation_invalid_args() {
    let mut handle: u64 = 0;
    unsafe {
        // Null host pointer rejected
        assert_eq!(
            fr_session_create(ptr::null(), ptr::null(), &raw mut handle),
            FR_ERR_INVALID_ARGUMENT
        );
        // Null out_session pointer rejected
        let host = CString::new("100.64.0.1:4710").unwrap();
        assert_eq!(
            fr_session_create(host.as_ptr(), ptr::null(), ptr::null_mut()),
            FR_ERR_INVALID_ARGUMENT
        );
        // Empty host string rejected
        let empty_host = CString::new("").unwrap();
        assert_eq!(
            fr_session_create(empty_host.as_ptr(), ptr::null(), &raw mut handle),
            FR_ERR_INVALID_ARGUMENT
        );
    }
}
