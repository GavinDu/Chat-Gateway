use crate::browser::pool::TestAdapterReport;

/// A provider's live adapter diagnostic result, independent of HTTP startup wiring.
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

#[cfg(test)]
mod tests {
    use super::adapter_report_is_healthy;
    use crate::browser::pool::TestAdapterReport;

    fn passing_report() -> TestAdapterReport {
        TestAdapterReport {
            session_ok: true,
            probe: vec![("input".to_string(), Some("#prompt".to_string()))],
            input_found: true,
            send_clicked: true,
            response_token: true,
            errors: Vec::new(),
        }
    }

    #[test]
    fn healthy_report_requires_all_critical_phases() {
        assert!(adapter_report_is_healthy(&passing_report()));

        let mut report = passing_report();
        report.session_ok = false;
        assert!(!adapter_report_is_healthy(&report));

        let mut report = passing_report();
        report.input_found = false;
        assert!(!adapter_report_is_healthy(&report));

        let mut report = passing_report();
        report.response_token = false;
        assert!(!adapter_report_is_healthy(&report));
    }

    #[test]
    fn any_report_error_marks_adapter_unhealthy() {
        let mut report = passing_report();
        report.errors.push("send failed".to_string());
        assert!(!adapter_report_is_healthy(&report));
    }
}
