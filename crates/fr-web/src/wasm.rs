//! Thin exported ABI: JavaScript supplies browser I/O and monotonic time; Rust
//! owns the protocol, attempt identity, bounds and lifecycle decisions.

use crate::adapter::{
    BROWSER_ABI_VERSION, BrowserAdapterError, BrowserAttempt, BrowserConnectionAdapter,
    MAX_BOOTSTRAP_BYTES, MAX_CONTROL_RECORD_BYTES,
};
use wasm_bindgen::prelude::{JsValue, wasm_bindgen};

#[wasm_bindgen(js_name = browserAbiVersion)]
pub fn browser_abi_version() -> u32 {
    BROWSER_ABI_VERSION
}

/// Check this before wasm-bindgen copies a response body into linear memory.
#[wasm_bindgen(js_name = maxBootstrapBytes)]
pub fn max_bootstrap_bytes() -> usize {
    MAX_BOOTSTRAP_BYTES
}

/// Check this before wasm-bindgen copies a control message into linear memory.
#[wasm_bindgen(js_name = maxControlRecordBytes)]
pub fn max_control_record_bytes() -> usize {
    MAX_CONTROL_RECORD_BYTES
}

#[wasm_bindgen(js_name = maxVideoRecordBytes)]
pub fn max_video_record_bytes() -> usize {
    fr_wire::negotiation::browser::MAX_VIDEO_RECORD_BYTES
}

#[wasm_bindgen]
pub struct BrowserAdapter {
    inner: BrowserConnectionAdapter,
}

impl Default for BrowserAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[wasm_bindgen]
impl BrowserAdapter {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Self {
        Self {
            inner: BrowserConnectionAdapter::new(),
        }
    }

    pub fn begin(&mut self, role: &str, now_ms: f64) -> Result<u32, JsValue> {
        let instant = microseconds(now_ms).map_err(js_error)?;
        self.inner
            .begin(role, instant)
            .map(BrowserAttempt::get)
            .map_err(js_error)
    }

    #[wasm_bindgen(js_name = bootstrapRequest)]
    pub fn bootstrap_request(&self, attempt: u32) -> Result<String, JsValue> {
        self.inner
            .bootstrap_request(BrowserAttempt(attempt))
            .map(str::to_owned)
            .map_err(js_error)
    }

    #[wasm_bindgen(js_name = acceptBootstrap)]
    pub fn accept_bootstrap(
        &mut self,
        attempt: u32,
        body: &str,
        now_ms: f64,
    ) -> Result<(), JsValue> {
        let attempt = BrowserAttempt(attempt);
        let instant = self.timestamp(attempt, now_ms)?;
        self.inner
            .accept_bootstrap(attempt, body, instant)
            .map_err(js_error)
    }

    #[wasm_bindgen(js_name = clientHello)]
    pub fn client_hello(&mut self, attempt: u32, now_ms: f64) -> Result<Vec<u8>, JsValue> {
        let attempt = BrowserAttempt(attempt);
        let instant = self.timestamp(attempt, now_ms)?;
        self.inner.client_hello(attempt, instant).map_err(js_error)
    }

    #[wasm_bindgen(js_name = receiveControl)]
    pub fn receive_control(
        &mut self,
        attempt: u32,
        bytes: &[u8],
        now_ms: f64,
    ) -> Result<String, JsValue> {
        let attempt = BrowserAttempt(attempt);
        let instant = self.timestamp(attempt, now_ms)?;
        self.inner
            .receive_control(attempt, bytes, instant)
            .map(str::to_owned)
            .map_err(js_error)
    }

    #[wasm_bindgen(js_name = takeControl)]
    pub fn take_control(&mut self, attempt: u32, now_ms: f64) -> Result<Option<Vec<u8>>, JsValue> {
        let attempt = BrowserAttempt(attempt);
        let now = self.timestamp(attempt, now_ms)?;
        self.inner.take_control(attempt, now).map_err(js_error)
    }
    #[wasm_bindgen(js_name = controlQueued)]
    pub fn control_queued(&mut self, attempt: u32, now_ms: f64) -> Result<(), JsValue> {
        let attempt = BrowserAttempt(attempt);
        let now = self.timestamp(attempt, now_ms)?;
        self.inner.control_queued(attempt, now).map_err(js_error)
    }
    #[wasm_bindgen(js_name = needsVideo)]
    pub fn needs_video(&self) -> bool {
        self.inner.needs_video()
    }
    #[wasm_bindgen(js_name = videoAttach)]
    pub fn video_attach(&mut self, attempt: u32, now_ms: f64) -> Result<Vec<u8>, JsValue> {
        let attempt = BrowserAttempt(attempt);
        let now = self.timestamp(attempt, now_ms)?;
        self.inner.video_attach(attempt, now).map_err(js_error)
    }
    #[wasm_bindgen(js_name = receiveVideo)]
    pub fn receive_video(
        &mut self,
        attempt: u32,
        bytes: &[u8],
        now_ms: f64,
    ) -> Result<String, JsValue> {
        let attempt = BrowserAttempt(attempt);
        let now = self.timestamp(attempt, now_ms)?;
        self.inner
            .receive_video(attempt, bytes, now)
            .map(str::to_owned)
            .map_err(js_error)
    }
    #[wasm_bindgen(js_name = takeVideoFeedback)]
    pub fn take_video_feedback(
        &mut self,
        attempt: u32,
        now_ms: f64,
    ) -> Result<Option<Vec<u8>>, JsValue> {
        let attempt = BrowserAttempt(attempt);
        let now = self.timestamp(attempt, now_ms)?;
        self.inner
            .take_video_feedback(attempt, now)
            .map_err(js_error)
    }
    #[wasm_bindgen(js_name = configurationReady)]
    pub fn configuration_ready(&self) -> bool {
        self.inner.configuration_ready()
    }
    #[wasm_bindgen(js_name = configurationParameters)]
    pub fn configuration_parameters(&self) -> Result<String, JsValue> {
        self.inner.configuration_parameters().map_err(js_error)
    }
    #[wasm_bindgen(js_name = prepareDesktopConfiguration)]
    pub fn prepare_desktop_configuration(&mut self) -> Result<String, JsValue> {
        media_json(
            &self
                .inner
                .prepare_desktop_configuration()
                .map_err(js_error)?,
        )
    }
    #[wasm_bindgen(js_name = desktopConfigured)]
    pub fn desktop_configured(&mut self, decoder: u64, now_ms: f64) -> Result<(), JsValue> {
        self.inner
            .desktop_configured(decoder, microseconds(now_ms).map_err(js_error)?)
            .map_err(js_error)
    }
    #[wasm_bindgen(js_name = takeVideoChunk)]
    pub fn take_video_chunk(
        &mut self,
        attempt: u32,
        now_ms: f64,
    ) -> Result<Option<BrowserPreparedChunk>, JsValue> {
        let attempt = BrowserAttempt(attempt);
        let now = self.timestamp(attempt, now_ms)?;
        self.inner
            .take_video_chunk(attempt, now)
            .map(|chunk| chunk.map(|inner| BrowserPreparedChunk { inner }))
            .map_err(js_error)
    }
    #[wasm_bindgen(js_name = desktopOutput)]
    pub fn desktop_output(
        &mut self,
        decoder: u64,
        callback: u64,
        now_ms: f64,
    ) -> Result<String, JsValue> {
        media_json(
            &self
                .inner
                .desktop_output(decoder, callback, microseconds(now_ms).map_err(js_error)?)
                .map_err(js_error)?,
        )
    }
    #[wasm_bindgen(js_name = desktopPresented)]
    pub fn desktop_presented(
        &mut self,
        decoder: u64,
        callback: u64,
        now_ms: f64,
        visible: bool,
    ) -> Result<(), JsValue> {
        self.inner
            .desktop_presented(
                decoder,
                callback,
                microseconds(now_ms).map_err(js_error)?,
                visible,
            )
            .map_err(js_error)
    }
    #[wasm_bindgen(js_name = desktopFrameClosed)]
    pub fn desktop_frame_closed(
        &mut self,
        decoder: u64,
        callback: u64,
        now_ms: f64,
    ) -> Result<String, JsValue> {
        media_json(
            &self
                .inner
                .desktop_frame_closed(decoder, callback, microseconds(now_ms).map_err(js_error)?)
                .map_err(js_error)?,
        )
    }
    #[wasm_bindgen(js_name = desktopDecoderClosed)]
    pub fn desktop_decoder_closed(&mut self, decoder: u64) -> Result<String, JsValue> {
        media_json(
            &self
                .inner
                .desktop_decoder_closed(decoder)
                .map_err(js_error)?,
        )
    }
    #[wasm_bindgen(js_name = desktopReclaim)]
    pub fn desktop_reclaim(&mut self, decoder: u64, now_ms: f64) -> Result<(), JsValue> {
        self.inner
            .desktop_reclaim(decoder, microseconds(now_ms).map_err(js_error)?)
            .map_err(js_error)
    }
    #[wasm_bindgen(js_name = desktopRetire)]
    pub fn desktop_retire(&mut self) {
        self.inner.desktop_retire();
    }
    #[wasm_bindgen(js_name = desktopQuarantine)]
    pub fn desktop_quarantine(&mut self) {
        self.inner.desktop_quarantine();
    }
    #[wasm_bindgen(js_name = desktopSnapshot)]
    pub fn desktop_snapshot(&self) -> Result<String, JsValue> {
        media_json(&self.inner.desktop_snapshot())
    }

    pub fn lifecycle(&mut self, attempt: u32, event: &str, now_ms: f64) -> Result<(), JsValue> {
        let attempt = BrowserAttempt(attempt);
        let instant = self.timestamp(attempt, now_ms)?;
        self.inner
            .lifecycle(attempt, event, instant)
            .map_err(js_error)
    }

    pub fn tick(&mut self, attempt: u32, now_ms: f64) -> Result<(), JsValue> {
        let attempt = BrowserAttempt(attempt);
        let instant = self.timestamp(attempt, now_ms)?;
        self.inner.tick(attempt, instant).map_err(js_error)
    }

    pub fn disconnect(&mut self, attempt: u32) -> Result<(), JsValue> {
        self.inner
            .disconnect(BrowserAttempt(attempt))
            .map_err(js_error)
    }

    pub fn snapshot(&self) -> Result<String, JsValue> {
        serde_json::to_string(&self.inner.snapshot())
            .map_err(|_| JsValue::from_str("browser_snapshot_failed"))
    }
}

impl BrowserAdapter {
    fn timestamp(&mut self, attempt: BrowserAttempt, now_ms: f64) -> Result<u64, JsValue> {
        self.inner.require_attempt(attempt).map_err(js_error)?;
        microseconds(now_ms).map_err(|_| js_error(self.inner.invalid_clock(attempt)))
    }
}

fn js_error(error: BrowserAdapterError) -> JsValue {
    JsValue::from_str(error.code())
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn microseconds(now_ms: f64) -> Result<u64, BrowserAdapterError> {
    // All accepted values are finite, nonnegative, and within the exact integer
    // range of JavaScript numbers. Flooring sub-microsecond precision cannot
    // extend an elapsed deadline. Invalid times never saturate into a timestamp.
    let value = (now_ms * 1000.0).floor();
    if !now_ms.is_finite() || now_ms < 0.0 || value > 9_007_199_254_740_991.0 {
        return Err(BrowserAdapterError::InvalidClock);
    }
    Ok(value as u64)
}

/// Media-only ownership. Constructed once per player and retained until every
/// original decoder/output closes; this object cannot grant a browser session.
#[wasm_bindgen]
pub struct BrowserMediaAdapter {
    inner: crate::media_adapter::MediaAdapter,
}

#[wasm_bindgen(js_name = browserMediaAbiVersion)]
pub fn browser_media_abi_version() -> u32 {
    crate::media_adapter::MEDIA_ABI_VERSION
}

#[wasm_bindgen]
impl BrowserMediaAdapter {
    #[wasm_bindgen(constructor)]
    pub fn new(max_frames: u32, max_bytes: u64) -> Self {
        Self {
            inner: crate::media_adapter::MediaAdapter::new(max_frames as usize, max_bytes),
        }
    }

    #[wasm_bindgen(js_name = maxConfigurationBytes)]
    #[expect(
        clippy::unused_self,
        reason = "the WASM owner exposes its pre-copy ABI limit"
    )]
    pub fn max_configuration_bytes(&self) -> usize {
        crate::media_adapter::MAX_CONFIGURATION_BYTES
    }

    #[wasm_bindgen(js_name = maxAccessUnitBytes)]
    #[expect(
        clippy::unused_self,
        reason = "the WASM owner exposes its pre-copy ABI limit"
    )]
    pub fn max_access_unit_bytes(&self) -> usize {
        crate::media_adapter::MAX_ACCESS_UNIT_BYTES
    }

    #[wasm_bindgen(js_name = prepareConfiguration)]
    pub fn prepare_configuration(
        &mut self,
        metadata: &str,
        hvcc: &[u8],
    ) -> Result<String, JsValue> {
        let config = self
            .inner
            .prepare_configuration(metadata, hvcc)
            .map_err(media_error)?;
        media_json(&config)
    }

    #[wasm_bindgen(js_name = decoderConfigured)]
    pub fn decoder_configured(&mut self, decoder: u64) -> Result<(), JsValue> {
        self.inner.configured(decoder).map_err(media_error)
    }

    #[wasm_bindgen(js_name = prepareChunk)]
    pub fn prepare_chunk(
        &mut self,
        decoder: u64,
        frame: u64,
        source: u64,
        key: bool,
        bytes: &[u8],
    ) -> Result<BrowserPreparedChunk, JsValue> {
        self.inner
            .prepare_chunk(decoder, frame, source, key, bytes)
            .map(|inner| BrowserPreparedChunk { inner })
            .map_err(media_error)
    }

    pub fn output(&mut self, decoder: u64, callback: u64) -> Result<String, JsValue> {
        media_json(&self.inner.output(decoder, callback).map_err(media_error)?)
    }

    pub fn presented(
        &mut self,
        decoder: u64,
        callback: u64,
        now_ms: f64,
        visible: bool,
    ) -> Result<(), JsValue> {
        let now = microseconds(now_ms)
            .map_err(|_| media_error(crate::media_adapter::MediaAdapterError::Clock))?;
        self.inner
            .presented(decoder, callback, now, visible)
            .map_err(media_error)
    }

    #[wasm_bindgen(js_name = frameClosed)]
    pub fn frame_closed(&mut self, decoder: u64, callback: u64) -> Result<String, JsValue> {
        media_json(
            &self
                .inner
                .frame_closed(decoder, callback)
                .map_err(media_error)?,
        )
    }

    pub fn reclaim(&mut self, decoder: u64) -> Result<(), JsValue> {
        self.inner.reclaim(decoder).map_err(media_error)
    }

    pub fn retire(&mut self) {
        self.inner.retire();
    }

    pub fn quarantine(&mut self) {
        self.inner.quarantine();
    }

    #[wasm_bindgen(js_name = decoderClosed)]
    pub fn decoder_closed(&mut self, decoder: u64) -> Result<String, JsValue> {
        media_json(&self.inner.decoder_closed(decoder).map_err(media_error)?)
    }

    #[wasm_bindgen(js_name = resetSession)]
    pub fn reset_session(&mut self) -> Result<(), JsValue> {
        self.inner.reset_session().map_err(media_error)
    }

    pub fn snapshot(&self) -> Result<String, JsValue> {
        media_json(&self.inner.snapshot())
    }
}

#[wasm_bindgen]
pub struct BrowserPreparedChunk {
    inner: crate::media_adapter::PreparedChunk,
}

#[wasm_bindgen]
impl BrowserPreparedChunk {
    pub fn decoder(&self) -> u64 {
        self.inner.inner.decoder.serial
    }
    pub fn generation(&self) -> u64 {
        self.inner.inner.generation.as_raw()
    }
    /// Exact JS-safe local timestamp used only to correlate native outputs.
    /// Original host timestamps stay in Rust and never become freshness here.
    pub fn timestamp(&self) -> u64 {
        self.inner.callback
    }

    #[wasm_bindgen(js_name = isKey)]
    pub fn is_key(&self) -> bool {
        self.inner.inner.is_key
    }

    #[wasm_bindgen(js_name = takeBytes)]
    pub fn take_bytes(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.inner.inner.hvc1_bytes)
    }
}

fn media_error(error: crate::media_adapter::MediaAdapterError) -> JsValue {
    JsValue::from_str(error.code())
}

fn media_json(value: &impl serde::Serialize) -> Result<String, JsValue> {
    serde_json::to_string(value).map_err(|_| JsValue::from_str("browser_media_snapshot_failed"))
}
