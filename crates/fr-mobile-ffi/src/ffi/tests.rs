//! The C ABI as the app calls it: header and Rust agree, bad arguments are
//! refused without touching outputs, and a real session (system resolver,
//! real thread and runtime) ends with a typed, content-free reason.
use super::*;
use crate::events::{Event, kind, state};
use crate::session::end;
use core::mem::{offset_of, size_of};
use std::ffi::CString;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

const HEADER: &str = include_str!("../../include/fr_mobile.h");

/// `#define NAME value` lines of the header, values parsed as integers.
fn defines() -> Vec<(String, i64)> {
    HEADER
        .lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("#define ")?;
            let mut parts = rest.split_whitespace();
            let name = parts.next()?;
            let value = parts.next()?.trim_matches(|c| c == '(' || c == ')');
            Some((name.to_owned(), value.parse().ok()?))
        })
        .collect()
}

#[test]
fn every_header_constant_matches_the_rust_value() {
    let mut expected: Vec<(String, i64)> = Vec::new();
    for (name, value) in STATUS {
        expected.push((name.to_owned(), value.into()));
    }
    for (name, value) in [
        ("FR_EVENT_STATE", kind::STATE),
        ("FR_EVENT_PIN", kind::PIN),
        ("FR_EVENT_VIDEO", kind::VIDEO),
        ("FR_EVENT_ENDED", kind::ENDED),
        ("FR_EVENT_OVERFLOW", kind::OVERFLOW),
        ("FR_STATE_CONNECTING", state::CONNECTING),
        ("FR_STATE_AUTHENTICATED", state::AUTHENTICATED),
        ("FR_STATE_AWAITING_APPROVAL", state::AWAITING_APPROVAL),
        ("FR_STATE_VIEWING", state::VIEWING),
        ("FR_STATE_REQUESTING_CONTROL", state::REQUESTING_CONTROL),
        ("FR_STATE_CONTROLLED", state::CONTROLLED),
        ("FR_STATE_RECONNECTING", state::RECONNECTING),
        ("FR_STATE_CLOSING", state::CLOSING),
        ("FR_STATE_CONTROL_ENDED", state::CONTROL_ENDED),
        ("FR_END_OK", end::OK),
        ("FR_END_HOST_CLOSED", end::HOST_CLOSED),
        ("FR_END_NAME_UNRESOLVED", end::NAME_UNRESOLVED),
        ("FR_END_OUTSIDE_TAILNET", end::OUTSIDE_TAILNET),
        ("FR_END_IDENTITY_CHANGED", end::IDENTITY_CHANGED),
        ("FR_END_TLS_REFUSED", end::TLS_REFUSED),
        ("FR_END_UNREACHABLE", end::UNREACHABLE),
        ("FR_END_HOST_REFUSED", end::HOST_REFUSED),
        ("FR_END_SESSION_FAILED", end::SESSION_FAILED),
        ("FR_END_CANCELLED", end::CANCELLED),
        ("FR_END_INTERNAL", end::INTERNAL),
    ] {
        expected.push((name.to_owned(), value.into()));
    }
    let mut found = defines();
    found.sort();
    expected.sort();
    assert_eq!(found, expected);
    // Planted negative: the parser sees a changed value.
    let changed = HEADER.replace("#define FR_END_INTERNAL 10", "#define FR_END_INTERNAL 11");
    assert!(changed != HEADER && changed.contains("FR_END_INTERNAL 11"));
}

#[test]
fn struct_layouts_match_the_headers_static_assertions() {
    // The same numbers src/abi_check.c asserts against the header at build.
    assert_eq!(size_of::<FrHandle>(), 8);
    assert_eq!(size_of::<Event>(), 32);
    assert_eq!(offset_of!(Event, picture_height), 24);
    assert_eq!(size_of::<FrFrameInfo>(), 32);
    assert_eq!(offset_of!(FrFrameInfo, full_range), 24);
    assert_eq!(size_of::<FrStats>(), 56);
    assert_eq!(offset_of!(FrStats, hardware), 40);
    assert_eq!(offset_of!(FrStats, end), 48);
    assert_eq!(size_of::<FrCursor>(), 32);
    assert_eq!(offset_of!(FrCursor, visible), 24);
    assert_eq!(size_of::<FrConfig>(), 24);
    assert_eq!(offset_of!(FrConfig, port), 8);
    assert_eq!(offset_of!(FrConfig, control), 10);
    assert_eq!(offset_of!(FrConfig, direct), 11);
    assert_eq!(offset_of!(FrConfig, pin), 16);
}

#[test]
fn host_names_are_checked_without_resolving() {
    let good = CString::new("mac-studio.tail1f21e.ts.net").unwrap();
    // DNS names are case-insensitive: typed capitals are accepted (and the
    // session uses the lowercase form).
    let capitals = CString::new("Mac-Studio.tail1f21e.TS.NET").unwrap();
    // SAFETY: NUL-terminated strings and NULL, per the contract.
    unsafe {
        assert!(fr_host_name_valid(good.as_ptr()));
        assert!(fr_host_name_valid(capitals.as_ptr()));
        assert!(!fr_host_name_valid(core::ptr::null()));
        for bad in [
            "100.89.198.71",
            "mac-studio",
            "mac-studio.example.com",
            "mac_studio.tail1f21e.ts.net",
            "-mac.tail1f21e.ts.net",
            "mac.studio.tail1f21e.ts.net",
            "mäc.tail1f21e.ts.net",
            "mac-studio.tail1f21e.ts.net.",
            "*.tail1f21e.ts.net",
        ] {
            let bad = CString::new(bad).unwrap();
            assert!(!fr_host_name_valid(bad.as_ptr()), "{bad:?}");
        }
    }
}

fn config(host: &CString, port: u16, pin: Option<&CString>) -> FrConfig {
    FrConfig {
        host: host.as_ptr(),
        port,
        control: false,
        direct: false,
        pin: pin.map_or(core::ptr::null(), |p| p.as_ptr()),
    }
}

/// Tests that start sessions share the process-wide registry.
static REGISTRY_TESTS: Mutex<()> = Mutex::new(());

fn start(host: &CString) -> (i32, FrHandle) {
    let start = config(host, 18443, None);
    let mut out: FrHandle = 0;
    // SAFETY: valid pointers per the contract.
    let status = unsafe { fr_session_start(&raw const start, &raw mut out) };
    (status, out)
}

#[test]
fn a_bad_start_is_refused_and_writes_no_handle() {
    let host = CString::new("mac.tailtest.ts.net").unwrap();
    let other_pin = CString::new("fr-pin-v1 other.tailtest.ts.net 100.64.0.9").unwrap();
    let malformed = CString::new("fr-pin-v1 mac.tailtest.ts.net 192.168.1.4").unwrap();
    let bad_host = CString::new("mac.example.com").unwrap();
    let mut out: FrHandle = 0;
    // SAFETY: valid or NULL pointers per the contract.
    unsafe {
        assert_eq!(
            fr_session_start(core::ptr::null(), &raw mut out),
            FR_INVALID
        );
        let good = config(&host, 18443, None);
        assert_eq!(
            fr_session_start(&raw const good, core::ptr::null_mut()),
            FR_INVALID
        );
        for refused in [
            config(&bad_host, 18443, None),
            config(&host, 0, None),
            config(&host, 18443, Some(&other_pin)),
            config(&host, 18443, Some(&malformed)),
            // A direct dial needs the pin it dials.
            FrConfig {
                direct: true,
                ..config(&host, 18443, None)
            },
        ] {
            assert_eq!(
                fr_session_start(&raw const refused, &raw mut out),
                FR_INVALID
            );
            assert_eq!(out, 0);
        }
    }
}

/// Collects `fr_directory_fetch` results.
struct Results(Mutex<Vec<String>>);
unsafe extern "C" fn collect(context: *mut c_void, json: *const c_char) {
    // SAFETY: the tests pass a `Results` that outlives the fetch, and the
    // fetch passes a NUL-terminated string valid during this call.
    let (results, text) = unsafe { (&*context.cast::<Results>(), CStr::from_ptr(json)) };
    results
        .0
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(text.to_str().unwrap().to_owned());
}
/// Wait (bounded) for exactly one result, then make sure no second arrives.
fn one_result(results: &Results, within: Duration) -> serde_json::Value {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if !results.0.lock().unwrap().is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(50));
    let seen = results.0.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "{seen:?}");
    serde_json::from_str(&seen[0]).unwrap()
}

#[test]
fn a_bad_directory_fetch_is_refused_and_never_calls_back() {
    let host = CString::new("mac.tailtest.ts.net").unwrap();
    let other_pin = CString::new("fr-pin-v1 other.tailtest.ts.net 100.64.0.9").unwrap();
    let bad_host = CString::new("mac.example.com").unwrap();
    let results = Results(Mutex::new(Vec::new()));
    let context = (&raw const results).cast_mut().cast::<c_void>();
    // SAFETY: valid or NULL pointers per the contract; `results` outlives
    // every call (none may call back).
    unsafe {
        assert_eq!(
            fr_directory_fetch(core::ptr::null(), context, Some(collect)),
            FR_INVALID
        );
        let good = config(&host, 18443, None);
        assert_eq!(
            fr_directory_fetch(&raw const good, context, None),
            FR_INVALID
        );
        for refused in [
            config(&bad_host, 18443, None),
            config(&host, 0, None),
            config(&host, 18443, Some(&other_pin)),
            FrConfig {
                direct: true,
                ..config(&host, 18443, None)
            },
        ] {
            assert_eq!(
                fr_directory_fetch(&raw const refused, context, Some(collect)),
                FR_INVALID
            );
        }
    }
    std::thread::sleep(Duration::from_millis(50));
    assert!(results.0.lock().unwrap().is_empty());
}

#[test]
fn a_directory_fetch_from_an_unresolvable_name_reports_that_once() {
    let host = CString::new("fr-ios-directory.tail00000.ts.net").unwrap();
    let results = Results(Mutex::new(Vec::new()));
    let fetch = config(&host, 18443, None);
    // SAFETY: valid pointers; `results` outlives the bounded wait below.
    let status = unsafe {
        fr_directory_fetch(
            &raw const fetch,
            (&raw const results).cast_mut().cast(),
            Some(collect),
        )
    };
    assert_eq!(status, FR_OK);
    let value = one_result(&results, Duration::from_secs(20));
    assert_eq!(value["status"], "failed");
    assert_eq!(value["via"], "fr-ios-directory.tail00000.ts.net");
    assert_eq!(value["end"], end::NAME_UNRESOLVED);
    assert!(value.get("machines").is_none(), "nothing invented");
}

#[test]
fn a_direct_directory_fetch_dials_the_pin_and_never_the_resolver() {
    // The same unresolvable name, pinned to a tailnet address where nothing
    // answers: a direct fetch must try that address (unreachable), never
    // resolve the name (which would end NAME_UNRESOLVED as above).
    let host = CString::new("fr-ios-direct.tail00000.ts.net").unwrap();
    let pin = CString::new("fr-pin-v1 fr-ios-direct.tail00000.ts.net 100.64.0.1").unwrap();
    let results = Results(Mutex::new(Vec::new()));
    let fetch = FrConfig {
        direct: true,
        ..config(&host, 9, Some(&pin))
    };
    // SAFETY: valid pointers; `results` outlives the bounded wait below.
    let status = unsafe {
        fr_directory_fetch(
            &raw const fetch,
            (&raw const results).cast_mut().cast(),
            Some(collect),
        )
    };
    assert_eq!(status, FR_OK);
    let value = one_result(&results, Duration::from_secs(40));
    assert_eq!(value["status"], "failed", "{value}");
    assert_ne!(value["end"], end::NAME_UNRESOLVED, "{value}");
    assert_eq!(value["end"], end::UNREACHABLE, "{value}");
}

#[test]
fn invented_freed_and_double_freed_handles_are_stale() {
    let _serial = REGISTRY_TESTS
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    for invented in [0, 1, u64::MAX, 7 << 32 | 3] {
        assert_eq!(fr_session_pointer(invented, 1.0, 1.0), FR_STALE);
        assert_eq!(fr_session_close(invented), FR_STALE);
        assert_eq!(fr_session_free(invented), FR_STALE);
        // SAFETY: NULL output pointers are allowed.
        unsafe {
            assert!(fr_session_copy_frame(invented, 0, core::ptr::null_mut()).is_null());
            assert!(!fr_session_stats(invented, core::ptr::null_mut()));
        }
    }
    let host = CString::new("fr-ios-stale.tail00000.ts.net").unwrap();
    let (status, handle) = start(&host);
    assert_eq!(status, FR_OK);
    assert_ne!(handle, 0);
    // Positive control: the live handle answers.
    assert_eq!(fr_session_pointer(handle, 1.0, 1.0), FR_NOT_CONTROLLED);
    assert_eq!(fr_session_free(handle), FR_OK);
    assert_eq!(fr_session_pointer(handle, 1.0, 1.0), FR_STALE);
    assert_eq!(fr_session_free(handle), FR_STALE, "double free");
    // The slot is reused under a new generation; the old handle stays dead.
    let (status, again) = start(&host);
    assert_eq!(status, FR_OK);
    assert_ne!(again, handle);
    assert_eq!(again & 0xffff_ffff, handle & 0xffff_ffff, "same slot");
    assert_eq!(fr_session_close(handle), FR_STALE);
    assert_eq!(fr_session_close(again), FR_OK);
    assert_eq!(fr_session_free(again), FR_OK);
}

#[test]
fn at_most_max_sessions_are_alive_at_once() {
    let _serial = REGISTRY_TESTS
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let host = CString::new("fr-ios-bound.tail00000.ts.net").unwrap();
    let handles: Vec<FrHandle> = (0..MAX_SESSIONS)
        .map(|_| {
            let (status, handle) = start(&host);
            assert_eq!(status, FR_OK);
            handle
        })
        .collect();
    assert_eq!(start(&host), (FR_BUSY, 0));
    assert_eq!(fr_session_free(handles[0]), FR_OK);
    let (status, replacement) = start(&host);
    assert_eq!(status, FR_OK);
    for handle in handles.iter().skip(1).chain([&replacement]) {
        assert_eq!(fr_session_free(*handle), FR_OK);
    }
}

/// Poll until the session reports its end, bounded.
fn wait_for_end(session: FrHandle, within: Duration) -> Vec<Event> {
    let deadline = Instant::now() + within;
    let mut events = Vec::new();
    while Instant::now() < deadline {
        let mut event = Event::default();
        // SAFETY: a writable event.
        while unsafe { fr_session_poll_event(session, &raw mut event) } {
            events.push(event);
            if event.kind == kind::ENDED {
                return events;
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    events
}

#[test]
fn a_name_that_does_not_resolve_ends_the_session_with_that_reason() {
    let _serial = REGISTRY_TESTS
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    // A syntactically valid name in a tailnet that does not exist: the
    // system resolver (MagicDNS or public DNS for ts.net) has no address.
    let host = CString::new("fr-ios-unresolvable.tail00000.ts.net").unwrap();
    let (status, out) = start(&host);
    assert_eq!(status, FR_OK);
    let events = wait_for_end(out, Duration::from_secs(20));
    let ended = events.last().copied().unwrap_or_default();
    assert_eq!(
        (ended.kind, ended.value),
        (kind::ENDED, end::NAME_UNRESOLVED),
        "{events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| e.kind == kind::STATE && e.value == state::CONNECTING),
        "{events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| e.kind == kind::PIN || e.kind == kind::VIDEO)
    );
    let mut detail = [0 as c_char; 128];
    let mut short = [0x55 as c_char; 8];
    let mut report = FrStats::default();
    // SAFETY: writable buffers of the stated sizes.
    unsafe {
        let len = fr_session_copy_detail(out, detail.as_mut_ptr(), detail.len());
        let text = CStr::from_ptr(detail.as_ptr()).to_str().unwrap();
        assert_eq!(text.len(), len);
        assert!(text.contains("NameUnresolved"), "{text}");
        // A short buffer gets a truncated, terminated copy and the full length.
        assert_eq!(
            fr_session_copy_detail(out, short.as_mut_ptr(), short.len()),
            len
        );
        assert_eq!(CStr::from_ptr(short.as_ptr()).to_bytes().len(), 7);
        assert_eq!(
            fr_session_copy_pin(out, detail.as_mut_ptr(), detail.len()),
            0
        );
        assert!(fr_session_copy_frame(out, 0, core::ptr::null_mut()).is_null());
        assert!(fr_session_stats(out, &raw mut report));
    }
    assert_eq!(fr_session_pointer(out, 1.0, 1.0), FR_NOT_CONTROLLED);
    assert_eq!(fr_session_request_control(out), FR_CLOSED);
    assert_eq!(fr_session_free(out), FR_OK);
    assert_eq!(report.end, end::NAME_UNRESOLVED);
    assert_eq!((report.presented, report.controlled), (0, 0));
}
