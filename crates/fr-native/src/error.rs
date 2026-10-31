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
impl core::fmt::Display for NativeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for NativeError {}
