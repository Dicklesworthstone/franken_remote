use core::{
    ffi::{c_char, c_int, c_void},
    fmt,
    marker::PhantomData,
    ptr::NonNull,
};
use fr_core::{ids::RecoveryGeneration, limits::ProtocolLimits};
use fr_media::{
    access_unit::{EncodedAccessUnit, FrameId, FrameKind},
    config::{CodecConfiguration, ColorInfo},
    hevc::{HevcGuard, framing::annex_b_to_length_prefixed},
};
use std::{collections::VecDeque, ffi::CString, rc::Rc};

unsafe extern "C" {
    fn fr_native_quiet();
    fn fr_native_avcodec_version() -> u32;
    fn fr_native_compiled_avcodec_version() -> u32;
    fn fr_encoder_new(
        backend: c_int,
        w: c_int,
        h: c_int,
        fps: c_int,
        bitrate: c_int,
        gop: c_int,
        out: *mut *mut c_void,
    ) -> c_int;
    fn fr_encoder_free(p: *mut c_void);
    fn fr_encoder_send(p: *mut c_void, bytes: *const u8, len: usize, pts: i64, idr: c_int)
    -> c_int;
    fn fr_encoder_drain(p: *mut c_void) -> c_int;
    fn fr_encoder_peek(p: *mut c_void, len: *mut usize, pts: *mut i64) -> c_int;
    fn fr_encoder_take(p: *mut c_void, bytes: *mut u8, len: usize) -> c_int;
    fn fr_decoder_new(
        w: c_int,
        h: c_int,
        coded_w: c_int,
        coded_h: c_int,
        configuration: *const u8,
        configuration_len: usize,
        out: *mut *mut c_void,
    ) -> c_int;
    fn fr_decoder_free(p: *mut c_void);
    fn fr_decoder_send(p: *mut c_void, bytes: *const u8, len: usize, pts: i64) -> c_int;
    fn fr_decoder_receive(p: *mut c_void, bytes: *mut u8, len: usize, pts: *mut i64) -> c_int;
    fn fr_x11_new(
        display: *const c_char,
        presenter: c_int,
        w: c_int,
        h: c_int,
        out: *mut *mut c_void,
        width: *mut c_int,
        height: *mut c_int,
    ) -> c_int;
    fn fr_x11_free(p: *mut c_void);
    fn fr_x11_capture(p: *mut c_void, bytes: *mut u8, len: usize) -> c_int;
    fn fr_x11_present(p: *mut c_void, bytes: *const u8, len: usize) -> c_int;
}

/// No native error string or screen content crosses into ordinary diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeError {
    InvalidConfiguration,
    Unavailable,
    Allocation,
    Codec,
    GeometryChanged,
    DisplayUnavailable,
    NeedDrain,
    NeedInput,
    EndOfStream,
    StaleGeneration,
    UnsupportedBitstream,
    Closed,
}
impl fmt::Display for NativeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for NativeError {}
fn status(code: c_int) -> Result<(), NativeError> {
    match code {
        0 => Ok(()),
        1 => Err(NativeError::NeedInput),
        2 => Err(NativeError::EndOfStream),
        -1 => Err(NativeError::InvalidConfiguration),
        -2 => Err(NativeError::Unavailable),
        -3 => Err(NativeError::Allocation),
        -5 => Err(NativeError::GeometryChanged),
        -6 => Err(NativeError::DisplayUnavailable),
        _ => Err(NativeError::Codec),
    }
}
fn zeroed(len: usize) -> Result<Vec<u8>, NativeError> {
    let mut v = Vec::new();
    v.try_reserve_exact(len)
        .map_err(|_| NativeError::Allocation)?;
    v.resize(len, 0);
    Ok(v)
}
fn frame_len(w: u32, h: u32, limits: &ProtocolLimits) -> Result<usize, NativeError> {
    limits
        .validate_coded_dimensions(w, h)
        .map_err(|_| NativeError::InvalidConfiguration)?;
    if w < 16 || h < 16 || !w.is_multiple_of(2) || !h.is_multiple_of(2) {
        return Err(NativeError::InvalidConfiguration);
    }
    usize::try_from(u64::from(w) * u64::from(h) * 4).map_err(|_| NativeError::Allocation)
}
fn initialize() -> Result<(), NativeError> {
    // SAFETY: version queries touch no caller memory. This process owns native logging.
    static LOG_INIT: std::sync::Once = std::sync::Once::new();
    LOG_INIT.call_once(|| unsafe { fr_native_quiet() });
    let (built, loaded) = unsafe {
        (
            fr_native_compiled_avcodec_version(),
            fr_native_avcodec_version(),
        )
    };
    if built >> 16 != loaded >> 16 {
        return Err(NativeError::Unavailable);
    }
    Ok(())
}
/// CPU-staging is explicit; this is not an opaque GPU or zero-copy frame.
pub struct BgraFrame {
    width: u32,
    height: u32,
    bytes: Vec<u8>,
}
impl fmt::Debug for BgraFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BgraFrame")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("byte_len", &self.bytes.len())
            .finish_non_exhaustive()
    }
}
impl BgraFrame {
    pub fn new(
        width: u32,
        height: u32,
        bytes: Vec<u8>,
        limits: &ProtocolLimits,
    ) -> Result<Self, NativeError> {
        if bytes.len() != frame_len(width, height, limits)? {
            return Err(NativeError::InvalidConfiguration);
        }
        Ok(Self {
            width,
            height,
            bytes,
        })
    }
    pub const fn width(&self) -> u32 {
        self.width
    }
    pub const fn height(&self) -> u32 {
        self.height
    }
    pub fn pixels(&self) -> &[u8] {
        &self.bytes
    }
}
/// Backend choice is local policy. Unavailable hardware NEVER selects software.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum EncodeBackend {
    Nvenc = 0,
    Vaapi = 1,
    SoftwareExplicit = 2,
}
struct Pending {
    frame: FrameId,
    capture: u64,
    force_idr: bool,
}
/// Thread-confined `FFmpeg` owner. Run in the supervised media process, not `frd`.
pub struct HevcEncoder {
    raw: NonNull<c_void>,
    config: CodecConfiguration,
    limits: ProtocolLimits,
    admission: HevcGuard,
    pending: VecDeque<Pending>,
    last_submitted: Option<FrameId>,
    last_output: Option<FrameId>,
    draining: bool,
    closed: bool,
    _thread: PhantomData<Rc<()>>,
}
impl HevcEncoder {
    pub fn new(
        config: CodecConfiguration,
        limits: ProtocolLimits,
        backend: EncodeBackend,
        fps: u32,
        bitrate: u32,
    ) -> Result<Self, NativeError> {
        initialize()?;
        let g = config.geometry();
        frame_len(g.coded_width(), g.coded_height(), &limits)?;
        frame_len(g.crop_width(), g.crop_height(), &limits)?;
        if config.color() != ColorInfo::sdr_bt709()
            || !(1..=240).contains(&fps)
            || !(10_000..=200_000_000).contains(&bitrate)
            || config.gop().max_gop_frames() > 480
        {
            return Err(NativeError::InvalidConfiguration);
        }
        let admission =
            HevcGuard::new(config, limits, 4).map_err(|_| NativeError::InvalidConfiguration)?;
        let mut ptr = core::ptr::null_mut();
        // SAFETY: validated integer ranges; bridge writes only the out pointer, owns all codec allocations.
        status(unsafe {
            fr_encoder_new(
                backend as c_int,
                c_int::try_from(g.crop_width()).map_err(|_| NativeError::InvalidConfiguration)?,
                c_int::try_from(g.crop_height()).map_err(|_| NativeError::InvalidConfiguration)?,
                c_int::try_from(fps).map_err(|_| NativeError::InvalidConfiguration)?,
                c_int::try_from(bitrate).map_err(|_| NativeError::InvalidConfiguration)?,
                c_int::try_from(config.gop().max_gop_frames())
                    .map_err(|_| NativeError::InvalidConfiguration)?,
                &raw mut ptr,
            )
        })?;
        Ok(Self {
            raw: NonNull::new(ptr).ok_or(NativeError::Allocation)?,
            config,
            limits,
            admission,
            pending: VecDeque::with_capacity(4),
            last_submitted: None,
            last_output: None,
            draining: false,
            closed: false,
            _thread: PhantomData,
        })
    }
    pub fn submit(
        &mut self,
        frame: &BgraFrame,
        id: FrameId,
        capture_micros: u64,
        force_idr: bool,
    ) -> Result<(), NativeError> {
        if self.closed || self.draining {
            return Err(NativeError::Closed);
        }
        let g = self.config.geometry();
        if frame.width != g.crop_width() || frame.height != g.crop_height() {
            return Err(NativeError::GeometryChanged);
        }
        if self.last_submitted.is_some_and(|last| id <= last) || id.as_raw() > i64::MAX as u64 {
            return Err(NativeError::StaleGeneration);
        }
        if self.pending.len() >= 4 {
            return Err(NativeError::NeedDrain);
        }
        let idr = force_idr || self.last_submitted.is_none();
        // SAFETY: frame is immutable and complete; bridge copies it before return and retains no Rust pointer.
        match status(unsafe {
            fr_encoder_send(
                self.raw.as_ptr(),
                frame.bytes.as_ptr(),
                frame.bytes.len(),
                i64::try_from(id.as_raw()).map_err(|_| NativeError::StaleGeneration)?,
                c_int::from(idr),
            )
        }) {
            Ok(()) => {}
            Err(NativeError::NeedInput) => return Err(NativeError::NeedDrain),
            Err(e) => {
                self.closed = true;
                return Err(e);
            }
        }
        self.pending.push_back(Pending {
            frame: id,
            capture: capture_micros,
            force_idr: idr,
        });
        self.last_submitted = Some(id);
        Ok(())
    }
    pub fn poll_output(&mut self) -> Result<EncodedAccessUnit, NativeError> {
        if self.closed {
            return Err(NativeError::Closed);
        }
        let mut len = 0;
        let mut pts = 0;
        // SAFETY: live unique context and valid out parameters; packet remains bridge-owned until take.
        status(unsafe { fr_encoder_peek(self.raw.as_ptr(), &raw mut len, &raw mut pts) })?;
        if len == 0
            || self.limits.validate_access_unit_len(len).is_err()
            || self
                .pending
                .front()
                .is_none_or(|p| p.frame.as_raw() != pts.cast_unsigned())
        {
            self.closed = true;
            return Err(NativeError::UnsupportedBitstream);
        }
        let mut bytes = zeroed(len)?;
        // SAFETY: destination has exactly the size returned by peek on this exclusively owned context.
        status(unsafe { fr_encoder_take(self.raw.as_ptr(), bytes.as_mut_ptr(), len) })?;
        let p = self.pending.pop_front().ok_or(NativeError::Codec)?;
        let idr = match annex_b_idr(&bytes) {
            Ok(idr) => idr,
            Err(error) => {
                self.closed = true;
                return Err(error);
            }
        };
        if (p.force_idr && !idr) || self.admission.validate_annex_b(&bytes, idr).is_err() {
            self.closed = true;
            return Err(NativeError::UnsupportedBitstream);
        }
        // Annex B is a native API detail. Only canonical four-byte-length NALs
        // leave this adapter for delivery, IPC or browser sample preparation.
        let Ok(bytes) = annex_b_to_length_prefixed(&bytes, self.limits) else {
            self.closed = true;
            return Err(NativeError::UnsupportedBitstream);
        };
        let kind = if idr {
            FrameKind::Idr {
                recovery: RecoveryGeneration::INITIAL,
            }
        } else {
            FrameKind::Predicted {
                references: self.last_output.ok_or(NativeError::UnsupportedBitstream)?,
            }
        };
        let au = EncodedAccessUnit::new(
            &self.limits,
            p.frame,
            kind,
            self.config.generation(),
            p.capture,
            bytes,
        )
        .map_err(|_| NativeError::Codec)?;
        self.last_output = Some(p.frame);
        Ok(au)
    }
    pub fn drain(&mut self) -> Result<(), NativeError> {
        if self.closed {
            return Err(NativeError::Closed);
        }
        // SAFETY: null-frame draining is inside the bridge, on the unique context.
        match status(unsafe { fr_encoder_drain(self.raw.as_ptr()) }) {
            Err(NativeError::NeedInput) => Err(NativeError::NeedDrain),
            Ok(()) => {
                self.draining = true;
                Ok(())
            }
            other => other,
        }
    }
}
impl Drop for HevcEncoder {
    fn drop(&mut self) {
        // SAFETY: unique native context; bridge releases frames before backing device refs.
        unsafe { fr_encoder_free(self.raw.as_ptr()) };
    }
}

/// Software decode with fixed admitted dimensions and padded packet ownership.
/// The pure-Rust HEVC guard admits parameter sets and complete slice headers
/// before foreign submission. This is not CABAC validation or an OS sandbox;
/// native decoding still belongs in the deadline-supervised media process.
pub struct HevcDecoder {
    raw: NonNull<c_void>,
    config: CodecConfiguration,
    limits: ProtocolLimits,
    admission: HevcGuard,
    pending: VecDeque<FrameId>,
    last: Option<FrameId>,
    closed: bool,
    _thread: PhantomData<Rc<()>>,
}
impl HevcDecoder {
    pub fn new(
        config: CodecConfiguration,
        limits: ProtocolLimits,
        record: &[u8],
    ) -> Result<Self, NativeError> {
        let g = config.geometry();
        frame_len(g.coded_width(), g.coded_height(), &limits)?;
        frame_len(g.crop_width(), g.crop_height(), &limits)?;
        if config.color() != ColorInfo::sdr_bt709() {
            return Err(NativeError::InvalidConfiguration);
        }
        let admission = HevcGuard::from_decoder_record(config, limits, 4, record)
            .map_err(|_| NativeError::InvalidConfiguration)?;
        initialize()?;
        let mut ptr = core::ptr::null_mut();
        // SAFETY: dimensions/parameter sets/resource demands admitted before FFI.
        // Bridge copies the exact record into padded native-owned extradata.
        status(unsafe {
            fr_decoder_new(
                c_int::try_from(g.crop_width()).map_err(|_| NativeError::InvalidConfiguration)?,
                c_int::try_from(g.crop_height()).map_err(|_| NativeError::InvalidConfiguration)?,
                c_int::try_from(g.coded_width()).map_err(|_| NativeError::InvalidConfiguration)?,
                c_int::try_from(g.coded_height()).map_err(|_| NativeError::InvalidConfiguration)?,
                record.as_ptr(),
                record.len(),
                &raw mut ptr,
            )
        })?;
        Ok(Self {
            raw: NonNull::new(ptr).ok_or(NativeError::Allocation)?,
            config,
            limits,
            admission,
            pending: VecDeque::with_capacity(4),
            last: None,
            closed: false,
            _thread: PhantomData,
        })
    }
    pub fn submit(&mut self, unit: &EncodedAccessUnit) -> Result<(), NativeError> {
        if self.closed {
            return Err(NativeError::Closed);
        }
        if unit.config_generation() != self.config.generation()
            || unit.frame().as_raw() > i64::MAX as u64
            || self.last.is_some_and(|n| unit.frame() <= n)
        {
            return Err(NativeError::StaleGeneration);
        }
        self.limits
            .validate_access_unit_len(unit.bytes().len())
            .map_err(|_| NativeError::UnsupportedBitstream)?;
        let idr = unit.is_idr();
        if !idr
            && unit.kind()
                != (FrameKind::Predicted {
                    references: self.last.ok_or(NativeError::UnsupportedBitstream)?,
                })
        {
            return Err(NativeError::UnsupportedBitstream);
        }
        if self.pending.len() >= 4 {
            return Err(NativeError::NeedDrain);
        }
        // Validation and codec acceptance form one transaction. A native EAGAIN
        // must not consume a picture or make the retry look like a replay.
        let mut admission = self.admission.clone();
        admission
            .validate_length_prefixed(unit.bytes(), idr)
            .map_err(|_| NativeError::UnsupportedBitstream)?;
        // hvcC selects length-prefixed packets in FFmpeg. Preserve canonical
        // four-byte framing rather than switching this configured stream to Annex B.
        let bytes = unit.bytes();
        // SAFETY: bridge copies into av_new_packet's padded reference-counted storage before returning.
        match status(unsafe {
            fr_decoder_send(
                self.raw.as_ptr(),
                bytes.as_ptr(),
                bytes.len(),
                i64::try_from(unit.frame().as_raw()).map_err(|_| NativeError::StaleGeneration)?,
            )
        }) {
            Ok(()) => {}
            Err(NativeError::NeedInput) => return Err(NativeError::NeedDrain),
            Err(e) => {
                self.closed = true;
                return Err(e);
            }
        }
        self.admission = admission;
        self.pending.push_back(unit.frame());
        self.last = Some(unit.frame());
        Ok(())
    }
    pub fn poll_output(&mut self) -> Result<(FrameId, BgraFrame), NativeError> {
        if self.closed {
            return Err(NativeError::Closed);
        }
        let g = self.config.geometry();
        let mut bytes = zeroed(frame_len(g.crop_width(), g.crop_height(), &self.limits)?)?;
        let mut pts = 0;
        // SAFETY: packed BGRA destination is exactly the admitted size; bridge rejects changed geometry.
        status(unsafe {
            fr_decoder_receive(
                self.raw.as_ptr(),
                bytes.as_mut_ptr(),
                bytes.len(),
                &raw mut pts,
            )
        })?;
        let id = self.pending.pop_front().ok_or(NativeError::Codec)?;
        if id.as_raw() != pts.cast_unsigned() {
            self.closed = true;
            return Err(NativeError::UnsupportedBitstream);
        }
        Ok((
            id,
            BgraFrame {
                width: g.crop_width(),
                height: g.crop_height(),
                bytes,
            },
        ))
    }
}
impl Drop for HevcDecoder {
    fn drop(&mut self) {
        // SAFETY: uniquely owned context; outstanding codec refs are released internally.
        unsafe { fr_decoder_free(self.raw.as_ptr()) };
    }
}

/// An explicit X11 session connection. No Wayland permission or isolation claim.
/// Xlib can terminate on display-server failure: keep this in the media worker.
pub struct X11Surface {
    raw: NonNull<c_void>,
    width: u32,
    height: u32,
    limits: ProtocolLimits,
    _thread: PhantomData<Rc<()>>,
}
impl X11Surface {
    pub fn capture(display: Option<&str>, limits: ProtocolLimits) -> Result<Self, NativeError> {
        Self::open(display, None, limits)
    }
    pub fn presenter(
        display: Option<&str>,
        width: u32,
        height: u32,
        limits: ProtocolLimits,
    ) -> Result<Self, NativeError> {
        frame_len(width, height, &limits)?;
        Self::open(display, Some((width, height)), limits)
    }
    fn open(
        display: Option<&str>,
        present: Option<(u32, u32)>,
        limits: ProtocolLimits,
    ) -> Result<Self, NativeError> {
        crate::xlib::initialize_threads().map_err(|_| NativeError::DisplayUnavailable)?;
        let name = display
            .map(CString::new)
            .transpose()
            .map_err(|_| NativeError::DisplayUnavailable)?;
        let mut ptr = core::ptr::null_mut();
        let (mut w, mut h) = (0, 0);
        let (pw, ph) = present.unwrap_or((0, 0));
        // SAFETY: NUL-terminated display name lives through the call; all three out pointers are writable.
        status(unsafe {
            fr_x11_new(
                name.as_ref().map_or(core::ptr::null(), |n| n.as_ptr()),
                c_int::from(present.is_some()),
                c_int::try_from(pw).map_err(|_| NativeError::InvalidConfiguration)?,
                c_int::try_from(ph).map_err(|_| NativeError::InvalidConfiguration)?,
                &raw mut ptr,
                &raw mut w,
                &raw mut h,
            )
        })?;
        let raw = NonNull::new(ptr).ok_or(NativeError::DisplayUnavailable)?;
        let surface = Self {
            raw,
            width: w.cast_unsigned(),
            height: h.cast_unsigned(),
            limits,
            _thread: PhantomData,
        };
        frame_len(surface.width, surface.height, &limits)?;
        Ok(surface)
    }
    pub const fn width(&self) -> u32 {
        self.width
    }
    pub const fn height(&self) -> u32 {
        self.height
    }
    pub fn snapshot(&mut self) -> Result<BgraFrame, NativeError> {
        let mut bytes = zeroed(frame_len(self.width, self.height, &self.limits)?)?;
        // SAFETY: bridge owns the XImage and copies only into this exact-size output; no borrowed pixels escape.
        status(unsafe { fr_x11_capture(self.raw.as_ptr(), bytes.as_mut_ptr(), bytes.len()) })?;
        Ok(BgraFrame {
            width: self.width,
            height: self.height,
            bytes,
        })
    }
    pub fn present(&mut self, frame: &BgraFrame) -> Result<(), NativeError> {
        if frame.width != self.width || frame.height != self.height {
            return Err(NativeError::GeometryChanged);
        }
        // SAFETY: bridge copies immutable pixels to its own XImage before XPutImage; destroys that image once.
        status(unsafe {
            fr_x11_present(self.raw.as_ptr(), frame.bytes.as_ptr(), frame.bytes.len())
        })
    }
}
impl Drop for X11Surface {
    fn drop(&mut self) {
        // SAFETY: context is unique, thread-confined, and no XImage survives a call.
        unsafe { fr_x11_free(self.raw.as_ptr()) };
    }
}

fn annex_b_idr(bytes: &[u8]) -> Result<bool, NativeError> {
    let mut slices = 0;
    let mut idr = false;
    let mut i = 0;
    while i + 3 < bytes.len() {
        let n = if bytes[i..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if bytes[i..].starts_with(&[0, 0, 1]) {
            3
        } else {
            i += 1;
            continue;
        };
        let h = bytes
            .get(i + n..i + n + 2)
            .ok_or(NativeError::UnsupportedBitstream)?;
        if h[0] & 0x80 != 0 || (h[0] & 1) != 0 || h[1] != 1 {
            return Err(NativeError::UnsupportedBitstream);
        }
        let typ = h[0] >> 1;
        if typ < 32 {
            if !matches!(typ, 1 | 19 | 20) {
                return Err(NativeError::UnsupportedBitstream);
            }
            slices += 1;
            idr = matches!(typ, 19 | 20);
        }
        i += n + 2;
    }
    if slices != 1 {
        return Err(NativeError::UnsupportedBitstream);
    }
    Ok(idr)
}
#[cfg(test)]
mod tests {
    use super::*;
    use fr_core::ids::CodecConfigurationGeneration;
    use fr_media::config::{CodedGeometry, GopPolicy};
    fn config() -> CodecConfiguration {
        CodecConfiguration::new_baseline(
            CodecConfigurationGeneration::INITIAL,
            CodedGeometry::new(&ProtocolLimits::ABSOLUTE, 64, 64, 64, 64, 2).unwrap(),
            ColorInfo::sdr_bt709(),
            GopPolicy::baseline_for_frame_rate(30).unwrap(),
        )
        .unwrap()
    }
    #[test]
    fn real_software_hevc_encode_decode_keeps_frame_identity() {
        let configuration = config();
        let limits = ProtocolLimits::ABSOLUTE;
        let mut encoder = HevcEncoder::new(
            configuration,
            limits,
            EncodeBackend::SoftwareExplicit,
            30,
            1_000_000,
        )
        .unwrap();
        let mut decoder = None;
        for n in 0..4 {
            let source = BgraFrame::new(
                64,
                64,
                vec![u8::try_from(n * 20).unwrap(); 64 * 64 * 4],
                &limits,
            )
            .unwrap();
            encoder
                .submit(&source, FrameId::from_raw(n), n * 33_333, n == 2)
                .unwrap();
            let au = encoder.poll_output().unwrap();
            assert_eq!(au.frame().as_raw(), n);
            assert_eq!(au.is_idr(), n == 0 || n == 2);
            if decoder.is_none() {
                let mut admission = HevcGuard::new(configuration, limits, 4).unwrap();
                admission
                    .validate_length_prefixed(au.bytes(), true)
                    .unwrap();
                decoder = Some(
                    HevcDecoder::new(
                        configuration,
                        limits,
                        admission.decoder_record().unwrap().bytes(),
                    )
                    .unwrap(),
                );
            }
            let decoder = decoder.as_mut().unwrap();
            decoder.submit(&au).unwrap();
            let (id, pixels) = decoder.poll_output().unwrap();
            assert_eq!(id, au.frame());
            assert_eq!(pixels.pixels().len(), 64 * 64 * 4);
        }
        encoder.drain().unwrap();
        assert!(matches!(
            encoder.poll_output(),
            Err(NativeError::EndOfStream)
        ));
    }
    #[test]
    fn invalid_geometry_and_stale_frames_refuse_before_native_submission() {
        let l = ProtocolLimits::ABSOLUTE;
        assert!(BgraFrame::new(63, 64, vec![], &l).is_err());
        assert!(BgraFrame::new(64, 64, vec![0; 1], &l).is_err());
        let mut e =
            HevcEncoder::new(config(), l, EncodeBackend::SoftwareExplicit, 30, 1_000_000).unwrap();
        let f = BgraFrame::new(64, 64, vec![0; 64 * 64 * 4], &l).unwrap();
        e.submit(&f, FrameId::FIRST, 0, false).unwrap();
        assert_eq!(
            e.submit(&f, FrameId::FIRST, 0, false),
            Err(NativeError::StaleGeneration)
        );
    }
    #[test]
    fn malformed_annex_b_and_non_baseline_vcl_are_refused() {
        for b in [
            &[0, 0, 1, 2, 1][..],
            &[0, 0, 1, 4, 1][..],
            &[0, 0, 1, 38, 2][..],
            &[1, 2, 3][..],
        ] {
            if b == [0, 0, 1, 2, 1] {
                assert_eq!(annex_b_idr(b), Ok(false));
            } else {
                assert!(annex_b_idr(b).is_err());
            }
        }
    }
}

#[cfg(test)]
mod admission_tests;
