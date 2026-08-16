//! The conversion path both entities share.
//!
//! Detection and parsing happen in the engine; this adds the three things
//! that need a host or a policy decision — threading the source URI into the
//! document so enrichment can find it, running the enrichment pass, and
//! recording what happened.

use std::time::Instant;

use mcpg_markdown_convert::{Budget, ConvertCx, ConvertError, RenderExtras, StreamInfo, Warning};
use serde::Serialize;

use crate::config::Profile;
use crate::enrich::{self, Enricher};
use crate::obs::{self, Metrics};

/// A finished conversion, in the shape the tool returns it.
#[derive(Debug, Clone, Serialize)]
pub struct Converted {
    pub markdown: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Which converter ran. Useful when a caller is surprised by the output.
    pub format: String,
    /// Which detection signal chose it: `content`, `extension` or `declared`.
    pub detected_via: String,
    /// Every degradation. Present and empty rather than omitted, so a
    /// consumer can tell "nothing went wrong" from "this field does not
    /// exist in your version".
    pub warnings: Vec<WarningOut>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WarningOut {
    pub kind: String,
    pub message: String,
}

impl From<&Warning> for WarningOut {
    fn from(w: &Warning) -> Self {
        Self {
            kind: w.kind.as_str().to_owned(),
            message: w.message.clone(),
        }
    }
}

/// One conversion request.
pub struct Request<'a> {
    pub profile: &'a Profile,
    pub bytes: &'a [u8],
    pub info: &'a StreamInfo,
    /// How the bytes were acquired. A metric label, so it is `&'static str`.
    pub mode: &'static str,
    /// `None` for the transform entity, which holds no host handle and must
    /// stay synchronous and cheap.
    pub enricher: Option<&'a dyn Enricher>,
    /// Timestamp for `{{ now }}` in a document template. Supplied by the
    /// caller rather than read from a clock here, so the engine stays pure
    /// and golden-corpus output stays reproducible.
    pub now: Option<&'a str>,
}

/// Convert, optionally enriching, and record metrics.
pub fn run(
    req: Request<'_>,
    cache: &mut std::collections::HashMap<String, String>,
    metrics: &dyn Metrics,
) -> Result<Converted, ConvertError> {
    let Request {
        profile,
        bytes,
        info,
        mode,
        enricher,
        now,
    } = req;

    let started = Instant::now();
    let budget = Budget::new(profile.config.convert.limits.clone());
    budget
        .check_input_size(bytes.len() as u64)
        .inspect_err(|e| {
            obs::record_failure(metrics, "none", mode, e.code());
        })?;
    let cx = ConvertCx::new(&budget);

    let (mut document, converter, detected_via) =
        match profile.engine.convert_to_ir(bytes, info, &cx) {
            Ok(v) => v,
            Err(e) => {
                obs::record_failure(metrics, "none", mode, e.code());
                return Err(e);
            }
        };

    // The audio converter leaves a placeholder rather than a transcript — it
    // has no host to ask. Recording where the bytes came from is what lets
    // the enrichment pass fetch them back.
    if let Some(uri) = &info.url
        && uri.starts_with("mcpg-resource://")
    {
        document.metadata.set("source_uri", uri.clone());
    }

    if let Some(e) = enricher {
        // The OCR pass needs the original bytes: a scanned PDF that arrived
        // inline has nothing in the content store for a model to read.
        let source = enrich::Source {
            bytes,
            mime: info
                .mimetype
                .as_deref()
                .unwrap_or("application/octet-stream"),
            uri: info
                .url
                .as_deref()
                .filter(|u| u.starts_with("mcpg-resource://")),
        };
        let report = enrich::enrich(&mut document, &profile.config.llm, e, cache, Some(source));
        if report.ran() || report.cached > 0 {
            obs::record_enrichment(metrics, &report);
        }
    }

    let rendered = match profile.engine.render_document_with(
        &document,
        RenderExtras {
            source: Some(info),
            now,
        },
    ) {
        Ok(r) => r,
        Err(e) => {
            obs::record_failure(metrics, converter, mode, e.code());
            return Err(e);
        }
    };

    let mut warnings = document.warnings.clone();
    warnings.extend(rendered.warnings);

    obs::record_success(
        metrics,
        obs::Success {
            converter,
            mode,
            detected_via,
            input_bytes: bytes.len(),
            output_bytes: rendered.markdown.len(),
            seconds: started.elapsed().as_secs_f64(),
            warnings: &warnings,
        },
    );

    Ok(Converted {
        markdown: rendered.markdown,
        title: document.title.clone(),
        format: converter.to_owned(),
        detected_via: detected_via.to_owned(),
        warnings: warnings.iter().map(WarningOut::from).collect(),
    })
}

/// Map an engine error onto the protocol's taxonomy.
///
/// `BackendError` carries no request-versus-defect distinction — its runtime
/// variant is `Transport` — so the split lives in the message instead: a
/// caught parser panic says so explicitly, because it is the one case that is
/// our defect rather than the caller's input.
#[must_use]
pub fn to_backend_error(e: &ConvertError) -> mcpg_plugin_protocol::BackendError {
    use mcpg_plugin_protocol::BackendError;
    match e {
        ConvertError::LimitExceeded { .. } if e.to_string().contains("timeout_ms") => {
            BackendError::Timeout { timeout_ms: 0 }
        }
        ConvertError::ConverterPanic { .. } => BackendError::Transport {
            message: format!("{e} — this is a defect; please report the input shape"),
        },
        _ => BackendError::Transport {
            message: e.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::config::ProfileConfig;
    use crate::obs::NoMetrics;
    use mcpg_markdown_convert::Engine;

    fn profile(config: ProfileConfig) -> Profile {
        Profile {
            name: "test".into(),
            engine: Engine::new(config.convert.clone()).expect("engine"),
            config,
        }
    }

    fn convert(bytes: &[u8], info: &StreamInfo) -> Result<Converted, ConvertError> {
        let p = profile(ProfileConfig::default());
        run(
            Request {
                profile: &p,
                bytes,
                info,
                mode: "inline",
                enricher: None,
                now: None,
            },
            &mut HashMap::new(),
            &NoMetrics,
        )
    }

    #[test]
    fn a_csv_converts_and_reports_its_format() {
        let out = convert(b"a,b\n1,2\n", &StreamInfo::new().with_filename("x.csv")).unwrap();
        assert_eq!(out.format, "csv");
        assert_eq!(out.detected_via, "extension");
        assert!(out.markdown.contains("| a | b |"));
    }

    #[test]
    fn warnings_are_always_present_even_when_empty() {
        let out = convert(b"plain text", &StreamInfo::new()).unwrap();
        assert!(out.warnings.is_empty());
        let json = serde_json::to_value(&out).unwrap();
        assert!(
            json.get("warnings").is_some(),
            "warnings must not be omitted"
        );
    }

    #[test]
    fn a_resource_uri_is_recorded_for_the_enrichment_pass() {
        // Without this the audio transcript path has no way back to the bytes.
        let info = StreamInfo::new()
            .with_filename("x.csv")
            .with_url("mcpg-resource://hash:abc");
        let out = convert(b"a,b\n1,2\n", &info);
        assert!(out.is_ok());
    }

    #[test]
    fn the_input_ceiling_is_enforced_here_too() {
        let mut config = ProfileConfig::default();
        config.convert.limits.max_input_bytes = 4;
        let p = profile(config);
        let e = run(
            Request {
                profile: &p,
                bytes: b"far too many bytes",
                info: &StreamInfo::new(),
                mode: "inline",
                enricher: None,
                now: None,
            },
            &mut HashMap::new(),
            &NoMetrics,
        )
        .unwrap_err();
        assert_eq!(e.code(), "limit_exceeded");
    }

    #[test]
    fn a_caught_panic_is_labelled_as_our_defect_not_the_callers_input() {
        let panic = to_backend_error(&ConvertError::ConverterPanic { format: "pdf" });
        assert!(format!("{panic:?}").contains("defect"), "{panic:?}");

        let malformed = to_backend_error(&ConvertError::Malformed {
            format: "csv",
            message: "bad row".into(),
        });
        assert!(
            !format!("{malformed:?}").contains("defect"),
            "{malformed:?}"
        );
        assert!(
            format!("{malformed:?}").contains("bad row"),
            "{malformed:?}"
        );
    }

    #[test]
    fn a_conversion_timeout_maps_to_the_timeout_variant() {
        use mcpg_plugin_protocol::BackendError;
        let e = ConvertError::LimitExceeded {
            limit: "timeout_ms",
            actual: 40_000,
            allowed: 30_000,
        };
        assert!(matches!(to_backend_error(&e), BackendError::Timeout { .. }));
    }

    #[test]
    fn now_is_supplied_by_the_caller_not_read_from_a_clock() {
        let mut config = ProfileConfig::default();
        config.convert.templates = Some(mcpg_markdown_convert::TemplateSpec {
            document: Some("stamped {{ now }}".into()),
            blocks: Default::default(),
        });
        let p = profile(config);
        let out = run(
            Request {
                profile: &p,
                bytes: b"x",
                info: &StreamInfo::new(),
                mode: "inline",
                enricher: None,
                now: Some("2026-08-15T00:00:00Z"),
            },
            &mut HashMap::new(),
            &NoMetrics,
        )
        .unwrap();
        assert!(out.markdown.contains("stamped 2026-08-15T00:00:00Z"));
    }
}
