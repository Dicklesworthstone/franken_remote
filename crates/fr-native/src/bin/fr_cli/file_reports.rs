//! Native file choices and content-free completion evidence shared by the
//! platform shells. No windowing, command socket or control authority lives here.
#[path = "file_reports/download.rs"]
pub(crate) mod download;
#[path = "file_reports/upload.rs"]
pub(crate) mod upload;
