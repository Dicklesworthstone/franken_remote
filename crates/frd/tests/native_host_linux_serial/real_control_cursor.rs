//! The host's cursor in the CONTROLLED share, through the shipped path. On
//! the host Xvfb an independent X client defines a 16x16 magenta ARGB cursor
//! and, later, warps the host's own pointer (never through `FrankenRemote`
//! input). The real `fr-media-worker` observes it through XFIXES, `frd run
//! --input-agent` forwards shape + position over real UDP/TLS/QUIC, and the
//! shipped `fr connect --control` renders it ONCE on a second Xvfb:
//!
//! - while the host follows the controller's own pointer, the LOCAL pointer is
//!   the owner: the viewer window's X cursor is the host's shape (read back by
//!   an independent client with `XFixesGetCursorImage` on the VIEWER display)
//!   and no overlay is composited;
//! - when the host moves its pointer itself, or the local pointer leaves the
//!   window, the composited overlay at the confirmed position is the owner and
//!   the local pointer over the window is blank.
//!
//! Scope: the tailnet `LocalAPI`, CA and ingress firewall are the existing
//! namespace FIXTURES; the harness plays the window manager and the user.
//! Xvfb is neither a GPU compositor nor `HiDPI` nor Wayland.
use super::real_control::{Controlled, eventually, host_pointer};
use super::real_cursor::{HostPointer, MAGENTA, pixels_at};
use super::real_media::{close_window, near};
use super::shipped_client::wait_for;
use super::*;
use std::process::Command;

const BLACK: (i32, i32, i32) = (0, 0, 0);
/// The host cursor image side; its hotspot is the top-left pixel.
const SIZE: i32 = 16;

/// The current cursor image on `display` as an independent XFIXES client sees
/// it: (width, height, root position, distinct ARGB pixels).
type CursorImage = (u32, u32, (i32, i32), Vec<u32>);
fn cursor_image(display: &str) -> Option<CursorImage> {
    const PEER: &str = r#"
import ctypes as c, sys
x = c.CDLL("libX11.so.6")
xf = c.CDLL("libXfixes.so.3")
D = c.c_void_p
class Image(c.Structure):
    _fields_ = [("x", c.c_short), ("y", c.c_short), ("width", c.c_ushort),
                ("height", c.c_ushort), ("xhot", c.c_ushort), ("yhot", c.c_ushort),
                ("serial", c.c_ulong), ("pixels", c.POINTER(c.c_ulong))]
x.XOpenDisplay.restype = D; x.XOpenDisplay.argtypes = [c.c_char_p]
x.XFree.argtypes = [c.c_void_p]
xf.XFixesQueryVersion.argtypes = [D, c.POINTER(c.c_int), c.POINTER(c.c_int)]
xf.XFixesGetCursorImage.restype = c.POINTER(Image); xf.XFixesGetCursorImage.argtypes = [D]
d = x.XOpenDisplay(sys.argv[1].encode())
assert d
major, minor = c.c_int(5), c.c_int(0)
assert xf.XFixesQueryVersion(d, c.byref(major), c.byref(minor))
image = xf.XFixesGetCursorImage(d)
assert image
i = image.contents
pixels = sorted({i.pixels[k] & 0xffffffff for k in range(i.width * i.height)})
print(i.width, i.height, i.x, i.y, *("%08x" % p for p in pixels))
x.XFree(image)
"#;
    let output = Command::new("python3")
        .args(["-c", PEER, display])
        .output()
        .ok()?;
    let line = String::from_utf8_lossy(&output.stdout).into_owned();
    let words: Vec<_> = line.split_whitespace().collect();
    if words.len() < 5 {
        return None;
    }
    Some((
        words[0].parse().ok()?,
        words[1].parse().ok()?,
        (words[2].parse().ok()?, words[3].parse().ok()?),
        words[4..]
            .iter()
            .map(|w| u32::from_str_radix(w, 16))
            .collect::<Result<_, _>>()
            .ok()?,
    ))
}
/// The host's 16x16 opaque magenta image, exactly.
fn remote_shape(image: &CursorImage) -> bool {
    image.0 == 16 && image.1 == 16 && image.3 == [0xffff_00ff]
}
/// Nothing drawn: every pixel fully transparent.
fn blank(image: &CursorImage) -> bool {
    image.3.iter().all(|p| p >> 24 == 0)
}

impl Controlled {
    /// Poll the viewer window's pixels until `accept`; `Err` has the last sample.
    fn await_pixels(
        &mut self,
        points: &[(i32, i32)],
        accept: impl Fn(&[u32]) -> bool,
    ) -> Result<Vec<u32>, Vec<Option<u32>>> {
        let window = self.window.0;
        let mut last = Vec::new();
        let seen = eventually(Duration::from_secs(20), || {
            assert!(self.client.try_wait().unwrap().is_none(), "fr exited early");
            last = pixels_at(&self.viewer.display, window, points);
            last.iter()
                .copied()
                .collect::<Option<Vec<u32>>>()
                .is_some_and(|p| accept(&p))
        });
        if seen {
            Ok(last.into_iter().map(Option::unwrap).collect())
        } else {
            Err(last)
        }
    }
    /// Poll the viewer display's current cursor image until `accept`.
    fn await_cursor(
        &self,
        accept: impl Fn(&CursorImage) -> bool,
    ) -> Result<CursorImage, Option<CursorImage>> {
        let mut last = None;
        let seen = eventually(Duration::from_secs(20), || {
            last = cursor_image(&self.viewer.display);
            last.as_ref().is_some_and(&accept)
        });
        match last {
            Some(image) if seen => Ok(image),
            other => Err(other),
        }
    }
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress; two Xvfb displays; real input agent"]
#[allow(clippy::too_many_lines)]
fn fr_connect_control_draws_the_host_cursor_once_as_the_local_pointer_or_the_overlay() {
    let mut s = Controlled::start_with(false, false);
    let mut pointer = HostPointer::start(&s.host.display);
    pointer.cursor(MAGENTA);
    let (_, (wx, wy)) = s.window;
    // Inside the 16x16 image, just past its corner.
    let at = |(x, y): (i32, i32)| [(x + 4, y + 4), (x + SIZE + 4, y + SIZE + 4)];
    let drawn = |p: &[u32]| near(p[0], MAGENTA) && near(p[1], BLACK);
    let absent = |p: &[u32]| p.iter().all(|&p| near(p, BLACK));

    // 1. The host follows the controller's pointer: the LOCAL pointer carries
    //    the host's shape; no overlay is composited under it.
    assert!(
        s.pointer_follows((200, 150), Duration::from_secs(10)),
        "{}",
        s.daemon.dump()
    );
    let (followed, _) = host_pointer(&mut s.observer);
    let local = s.await_cursor(remote_shape);
    let (lx, ly) = local.as_ref().map_or((0, 0), |image| image.2);
    assert!(
        local.is_ok() && (lx, ly) == (wx + followed.0, wy + followed.1),
        "viewer window cursor is not the host shape at the local pointer: {local:x?}; host: {}",
        s.daemon.dump()
    );
    let no_overlay = s.await_pixels(&at(followed), absent);
    assert!(
        no_overlay.is_ok(),
        "a second, composited cursor: {no_overlay:x?}"
    );
    println!(
        "local owner: host pointer {followed:?}; viewer cursor {local:x?}; window pixels {no_overlay:x?}"
    );

    // 2. The host moves its OWN pointer (no FrankenRemote input): the overlay
    //    at the confirmed position takes over and the local pointer is blank.
    pointer.warp(420, 300);
    let mut points = at((420, 300)).to_vec();
    points.extend(at(followed));
    let overlay = s.await_pixels(&points, |p| drawn(&p[..2]) && absent(&p[2..]));
    assert!(
        overlay.is_ok(),
        "no overlay at the host-moved pointer: {overlay:x?}; host: {}",
        s.daemon.dump()
    );
    let hidden = s.await_cursor(blank);
    assert!(
        hidden.is_ok(),
        "the local pointer still draws beside the overlay: {hidden:x?}"
    );
    println!("host-moved: window pixels {overlay:x?}; viewer cursor {hidden:x?}");
    // The viewer pointer did not move and the host pointer stays where the
    // host put it: the confirmed position is not an input command.
    assert_eq!(host_pointer(&mut s.observer).0, (420, 300));

    // 3. The controller moves again: the host follows, the overlay goes and
    //    the local pointer takes the shape back.
    assert!(s.pointer_follows((250, 200), Duration::from_secs(10)));
    let (back, _) = host_pointer(&mut s.observer);
    let gone = s.await_pixels(&at((420, 300)), absent);
    assert!(
        gone.is_ok(),
        "overlay residue after the host followed: {gone:x?}"
    );
    let restored = s.await_cursor(remote_shape);
    assert!(
        restored.is_ok(),
        "local pointer shape not restored: {restored:x?}"
    );
    println!(
        "followed again: host pointer {back:?}; pixels at the old overlay {gone:x?}; viewer cursor {restored:x?}"
    );

    // 4. The local pointer leaves the window: the host pointer stays put and
    //    the overlay shows it there (the local pointer is not over the window).
    let outside = (660, 500);
    assert!(
        wx + outside.0 < 800 && wy + outside.1 < 600,
        "window at {wx},{wy}"
    );
    s.viewer_move(outside);
    let left = s.await_pixels(&at(back), drawn);
    assert!(
        left.is_ok(),
        "no overlay after the local pointer left: {left:x?}; host: {}",
        s.daemon.dump()
    );
    assert_eq!(host_pointer(&mut s.observer).0, back);
    println!("local pointer left: window pixels at {back:?} {left:x?}");

    drop(pointer);
    close_window(&s.viewer.display, s.window.0);
    let output = wait_for(s.client, Duration::from_secs(30));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let report: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|_| {
        panic!(
            "fr: {stdout} {}; host: {}",
            String::from_utf8_lossy(&output.stderr),
            s.daemon.dump()
        )
    });
    assert_eq!(report["outcome"], "stopped", "{report}");
    assert_eq!(report["control_granted"], true, "{report}");
    s.daemon.finish();
}
