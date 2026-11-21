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
