//! Metrics and spans.
//!
//! Label sets are closed — converter names, warning kinds, error codes and
//! source modes are all `&'static str` from fixed enumerations — so no
//! caller-supplied string can inflate cardinality. A filename in a metric
//! label is an outage waiting for a big enough directory.

use mcpg_markdown_convert::Warning;
use mcpg_plugin_sdk::HostHandle;

/// Somewhere to send measurements. A trait so the conversion path is testable
/// without a gateway, and so the transform entity — which holds no host
/// handle — can record nothing without a special case.
pub trait Metrics {
    fn counter(&self, name: &'static str, value: u64, labels: &[(&str, &str)]);
    fn histogram(&self, name: &'static str, value: f64, labels: &[(&str, &str)]);
}

/// Metrics through the host.
pub struct HostMetrics<'a>(pub &'a HostHandle);

impl Metrics for HostMetrics<'_> {
    fn counter(&self, name: &'static str, value: u64, labels: &[(&str, &str)]) {
        self.0.counter(name, value, labels);
    }
    fn histogram(&self, name: &'static str, value: f64, labels: &[(&str, &str)]) {
        self.0.histogram(name, value, labels);
    }
}

/// Drops everything. Used by the transform entity, which the host gives no
/// handle, and by tests.
pub struct NoMetrics;

impl Metrics for NoMetrics {
    fn counter(&self, _n: &'static str, _v: u64, _l: &[(&str, &str)]) {}
    fn histogram(&self, _n: &'static str, _v: f64, _l: &[(&str, &str)]) {}
}

pub const CONVERSIONS: &str = "mcpg_markdown_conversions_total";
pub const DURATION: &str = "mcpg_markdown_duration_seconds";
pub const INPUT_BYTES: &str = "mcpg_markdown_input_bytes";
pub const OUTPUT_BYTES: &str = "mcpg_markdown_output_bytes";
pub const WARNINGS: &str = "mcpg_markdown_warnings_total";
pub const ENRICHMENT: &str = "mcpg_markdown_enrichment_calls_total";
pub const PANICS: &str = "mcpg_markdown_parser_panics_total";

/// What one completed conversion measured.
pub struct Success<'a> {
    pub converter: &'static str,
    pub mode: &'static str,
    pub detected_via: &'static str,
    pub input_bytes: usize,
    pub output_bytes: usize,
    pub seconds: f64,
    pub warnings: &'a [Warning],
}

/// Record one completed conversion.
pub fn record_success(m: &dyn Metrics, s: Success<'_>) {
    let format = s.converter;
    m.counter(
        CONVERSIONS,
        1,
        &[
            ("format", format),
            ("source", s.mode),
            ("detected_via", s.detected_via),
            ("outcome", "success"),
        ],
    );
    m.histogram(DURATION, s.seconds, &[("format", format)]);
    m.histogram(INPUT_BYTES, s.input_bytes as f64, &[("format", format)]);
    m.histogram(OUTPUT_BYTES, s.output_bytes as f64, &[("format", format)]);
    record_warnings(m, format, s.warnings);
}

/// Record a failed conversion. `format` is the converter that failed where
/// one was chosen, and `"none"` when detection found nothing to try.
pub fn record_failure(
    m: &dyn Metrics,
    format: &'static str,
    mode: &'static str,
    code: &'static str,
) {
    m.counter(
        CONVERSIONS,
        1,
        &[
            ("format", format),
            ("source", mode),
            ("outcome", "error"),
            ("error", code),
        ],
    );
    // A caught parser panic is never expected. It gets its own series so an
    // alert can fire on any non-zero value rather than on a rate change.
    if code == "panic" {
        m.counter(PANICS, 1, &[("format", format)]);
    }
}

pub fn record_warnings(m: &dyn Metrics, converter: &'static str, warnings: &[Warning]) {
    for w in warnings {
        m.counter(
            WARNINGS,
            1,
            &[("format", converter), ("kind", w.kind.as_str())],
        );
    }
}

pub fn record_enrichment(m: &dyn Metrics, report: &crate::enrich::EnrichReport) {
    for (outcome, n) in [
        ("success", report.succeeded),
        ("error", report.failed),
        ("cached", report.cached),
        ("over_budget", report.skipped_over_budget),
    ] {
        if n > 0 {
            m.counter(ENRICHMENT, u64::from(n), &[("outcome", outcome)]);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use mcpg_markdown_convert::WarningKind;

    /// One recorded counter: name, value, labels.
    type Counter = (String, u64, Vec<(String, String)>);

    #[derive(Default)]
    struct Recorder {
        counters: RefCell<Vec<Counter>>,
        histograms: RefCell<Vec<(String, f64)>>,
    }

    impl Metrics for Recorder {
        fn counter(&self, name: &'static str, value: u64, labels: &[(&str, &str)]) {
            self.counters.borrow_mut().push((
                name.to_owned(),
                value,
                labels
                    .iter()
                    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                    .collect(),
            ));
        }
        fn histogram(&self, name: &'static str, value: f64, _l: &[(&str, &str)]) {
            self.histograms.borrow_mut().push((name.to_owned(), value));
        }
    }

    fn success<'a>(
        converter: &'static str,
        mode: &'static str,
        warnings: &'a [Warning],
    ) -> Success<'a> {
        Success {
            converter,
            mode,
            detected_via: "content",
            input_bytes: 100,
            output_bytes: 50,
            seconds: 0.25,
            warnings,
        }
    }

    #[test]
    fn a_success_records_a_conversion_and_three_histograms() {
        let r = Recorder::default();
        record_success(&r, success("docx", "resource", &[]));
        assert_eq!(r.counters.borrow().len(), 1);
        let names: Vec<String> = r
            .histograms
            .borrow()
            .iter()
            .map(|(n, _)| n.clone())
            .collect();
        assert!(names.contains(&DURATION.to_owned()));
        assert!(names.contains(&INPUT_BYTES.to_owned()));
        assert!(names.contains(&OUTPUT_BYTES.to_owned()));
    }

    #[test]
    fn every_warning_is_counted_by_kind() {
        let r = Recorder::default();
        record_warnings(
            &r,
            "pdf",
            &[
                Warning::new(WarningKind::NoTextLayer, "a"),
                Warning::new(WarningKind::Truncated, "b"),
            ],
        );
        let kinds: Vec<String> = r
            .counters
            .borrow()
            .iter()
            .filter(|(n, _, _)| n == WARNINGS)
            .map(|(_, _, l)| {
                l.iter()
                    .find(|(k, _)| k == "kind")
                    .map(|(_, v)| v.clone())
                    .unwrap_or_default()
            })
            .collect();
        assert_eq!(kinds, vec!["no_text_layer", "truncated"]);
    }

    #[test]
    fn a_caught_panic_gets_its_own_series() {
        let r = Recorder::default();
        record_failure(&r, "pdf", "inline", "panic");
        assert!(
            r.counters.borrow().iter().any(|(n, _, _)| n == PANICS),
            "a caught parser panic must be alertable on its own"
        );
    }

    #[test]
    fn an_ordinary_failure_does_not_touch_the_panic_series() {
        let r = Recorder::default();
        record_failure(&r, "pdf", "inline", "malformed");
        assert!(!r.counters.borrow().iter().any(|(n, _, _)| n == PANICS));
    }

    #[test]
    fn label_values_never_come_from_caller_input() {
        // Every label this module emits is a &'static str from a closed set.
        // A filename or a URL in a label is an unbounded-cardinality outage.
        let r = Recorder::default();
        record_success(&r, success("csv", "url", &[]));
        for (_, _, labels) in r.counters.borrow().iter() {
            for (k, v) in labels {
                assert!(!v.contains('/'), "{k}={v} looks like a path");
                assert!(v.len() < 32, "{k}={v} is suspiciously long");
            }
        }
    }

    #[test]
    fn enrichment_outcomes_are_counted_only_when_non_zero() {
        let r = Recorder::default();
        record_enrichment(
            &r,
            &crate::enrich::EnrichReport {
                attempted: 3,
                succeeded: 2,
                failed: 1,
                cached: 0,
                skipped_over_budget: 0,
            },
        );
        assert_eq!(r.counters.borrow().len(), 2);
    }
}
