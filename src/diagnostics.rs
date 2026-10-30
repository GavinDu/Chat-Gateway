use crate::browser::pool::TestAdapterReport;

/// A provider's live adapter diagnostic result, independent of HTTP startup wiring.
#[derive(serde::Serialize)]
pub(crate) struct AdapterDiagnostic {
    pub(crate) provider: String,
    pub(crate) queue_ms: Option<u128>,
    pub(crate) report: Option<TestAdapterReport>,
    pub(crate) models: Option<Vec<String>>,
    pub(crate) selectors: Option<String>,
    pub(crate) errors: Vec<String>,
    pub(crate) healthy: bool,
}

pub(crate) fn adapter_report_is_healthy(report: &TestAdapterReport) -> bool {
    report.errors.is_empty() && report.session_ok && report.input_found && report.response_token
}
