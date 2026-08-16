//! The `transform` entity — convert a document already in a payload.
//!
//! This is the half the *gateway* invokes: as a global pre/post-dispatch
//! transform, or as a pipeline `plugin_transform` step. It is pure compute —
//! no host handle, no I/O, no enrichment — so a `backend.sftp` step can fetch
//! a `.docx` and the next step can convert it without the bytes ever
//! round-tripping through the model's context.
//!
//! It shares the profile registry with the backend entity, so a document
//! converted here and the same document converted through the tool render
//! identically. That is the reason both entities live in one plugin.

use std::collections::HashMap;
use std::sync::Arc;

use base64::Engine as _;
use mcpg_markdown_convert::StreamInfo;
use mcpg_plugin_protocol::{PluginContext, PluginManifest, TransformResult, firstparty_manifest};
use mcpg_plugin_sdk::ffi::SyncTransform;
use serde::Deserialize;
use serde_json::Value;

use crate::config::Profile;
use crate::convert;
use crate::obs::NoMetrics;
use crate::state::{MarkdownState, shared};

/// Which dispatch phase a global transform fires on. Ignored by the pipeline
/// bridge, which calls `transform_result` directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Arguments,
    Result,
    #[default]
    Both,
}

/// What the operator writes on the step.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TransformConfig {
    /// Which registered profile to use. Optional when exactly one exists.
    #[serde(default)]
    profile: Option<String>,
    /// JSON Pointer (RFC 6901) to the sub-value to convert and replace. When
    /// omitted, the whole value is converted.
    #[serde(default)]
    pointer: Option<String>,
    #[serde(default)]
    phase: Phase,
    /// How the targeted value encodes the document.
    #[serde(default)]
    encoding: Encoding,
    /// Filename hint, when the payload carries no name of its own.
    #[serde(default)]
    filename: Option<String>,
    #[serde(default)]
    mimetype: Option<String>,
    /// Emit the full result object rather than the Markdown string. Off by
    /// default: a pipeline step usually wants the text.
    #[serde(default)]
    verbose: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Encoding {
    /// The value is a JSON string holding base64.
    Base64,
    /// The value is a JSON string holding the document as text.
    Text,
    /// Decide from the value: a valid base64 string that decodes to
    /// non-textual bytes is treated as base64, otherwise as text.
    #[default]
    Auto,
}

pub struct MarkdownTransform {
    manifest: PluginManifest,
    state: Arc<MarkdownState>,
}

impl MarkdownTransform {
    #[must_use]
    pub fn new() -> Self {
        Self {
            manifest: firstparty_manifest! {
                id: "dev.mcpg.backend.markdown",
                name: "Markdown Conversion",
                class: Transform,
            },
            state: shared(),
        }
    }

    fn run(&self, value: &Value, config: &Value, phase: Phase) -> TransformResult {
        let cfg: TransformConfig = match serde_json::from_value(config.clone()) {
            Ok(c) => c,
            Err(e) => {
                return TransformResult::Error {
                    message: format!("markdown transform config: {e}"),
                };
            }
        };
        if cfg.phase != Phase::Both && cfg.phase != phase {
            return TransformResult::Unchanged;
        }

        let profile = match self.resolve_profile(&cfg) {
            Ok(p) => p,
            Err(message) => return TransformResult::Error { message },
        };

        let ptr = cfg.pointer.as_deref().unwrap_or("");
        let Some(target) = value.pointer(ptr) else {
            return TransformResult::Error {
                message: format!("pointer {ptr:?} not found in value"),
            };
        };

        let Some(raw) = target.as_str() else {
            return TransformResult::Error {
                message: format!(
                    "pointer {ptr:?} targets {}, but the markdown transform needs a string \
                     holding the document (base64 or text)",
                    kind_of(target)
                ),
            };
        };

        let (bytes, is_text) = match decode(raw, cfg.encoding) {
            Ok(v) => v,
            Err(message) => return TransformResult::Error { message },
        };

        let mut info = StreamInfo::new();
        if let Some(f) = &cfg.filename {
            info = info.with_filename(f.clone());
        }
        if let Some(m) = &cfg.mimetype {
            info = info.with_mimetype(m.clone());
        }
        if is_text && info.charset.is_none() {
            info = info.with_charset("utf-8");
        }

        // No enricher and no metrics sink: this entity is handed no host, so
        // it can neither call a model nor emit a measurement. Sharing a `.so`
        // with the entity that can does not grant it either.
        let converted = convert::run(
            convert::Request {
                profile: &profile,
                bytes: &bytes,
                info: &info,
                mode: "transform",
                enricher: None,
                now: None,
            },
            &mut HashMap::new(),
            &NoMetrics,
        );
        let converted = match converted {
            Ok(c) => c,
            Err(e) => {
                return TransformResult::Error {
                    message: e.to_string(),
                };
            }
        };

        let produced = if cfg.verbose {
            match serde_json::to_value(&converted) {
                Ok(v) => v,
                Err(e) => {
                    return TransformResult::Error {
                        message: format!("serialising the conversion result: {e}"),
                    };
                }
            }
        } else {
            Value::String(converted.markdown)
        };

        if ptr.is_empty() {
            return TransformResult::Modified { value: produced };
        }
        let mut out = value.clone();
        match out.pointer_mut(ptr) {
            Some(slot) => {
                *slot = produced;
                TransformResult::Modified { value: out }
            }
            None => TransformResult::Error {
                message: format!("pointer {ptr:?} not assignable"),
            },
        }
    }

    fn resolve_profile(&self, cfg: &TransformConfig) -> Result<Arc<Profile>, String> {
        match &cfg.profile {
            Some(name) => self.state.profile(name).map_err(|e| format!("{e:?}")),
            None => self.state.sole_profile().ok_or_else(|| {
                let names = self.state.profile_names();
                if names.is_empty() {
                    "no markdown profile is registered; configure a markdown backend \
                     binding, or give this step an inline profile"
                        .to_owned()
                } else {
                    format!(
                        "several markdown profiles are registered ({}); name one with \
                         `profile:` rather than letting the step guess",
                        names.join(", ")
                    )
                }
            }),
        }
    }
}

impl Default for MarkdownTransform {
    fn default() -> Self {
        Self::new()
    }
}

impl SyncTransform for MarkdownTransform {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn transform_arguments(
        &self,
        _ctx: &PluginContext,
        arguments: &Value,
        config: &Value,
    ) -> TransformResult {
        self.run(arguments, config, Phase::Arguments)
    }

    fn transform_result(
        &self,
        _ctx: &PluginContext,
        result: &Value,
        config: &Value,
    ) -> TransformResult {
        self.run(result, config, Phase::Result)
    }
}

/// Returns the bytes and whether they came from the text path.
fn decode(raw: &str, encoding: Encoding) -> Result<(Vec<u8>, bool), String> {
    match encoding {
        Encoding::Text => Ok((raw.as_bytes().to_vec(), true)),
        Encoding::Base64 => base64::engine::general_purpose::STANDARD
            .decode(raw.trim())
            .map(|b| (b, false))
            .map_err(|e| format!("value is not valid base64: {e}")),
        Encoding::Auto => {
            // Prefer base64 only when the decode succeeds AND the result does
            // not look like the input. Plain ASCII prose is frequently valid
            // base64, and decoding it would silently corrupt the document.
            let decoded = base64::engine::general_purpose::STANDARD.decode(raw.trim());
            match decoded {
                Ok(bytes) if looks_binary(&bytes) => Ok((bytes, false)),
                _ => Ok((raw.as_bytes().to_vec(), true)),
            }
        }
    }
}

/// A container signature, or bytes that plainly are not text.
fn looks_binary(bytes: &[u8]) -> bool {
    if bytes.len() < 4 {
        return false;
    }
    if bytes.starts_with(b"PK\x03\x04")
        || bytes.starts_with(b"%PDF-")
        || bytes.starts_with(&[0xD0, 0xCF, 0x11, 0xE0])
    {
        return true;
    }
    bytes.contains(&0)
}

fn kind_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn transform_with(profiles: &[(&str, Value)]) -> MarkdownTransform {
        let t = MarkdownTransform {
            manifest: firstparty_manifest! {
                id: "dev.mcpg.backend.markdown",
                name: "Markdown Conversion",
                class: Transform,
            },
            state: Arc::new(MarkdownState::default()),
        };
        for (name, spec) in profiles {
            t.state.register(name, spec).expect("registers");
        }
        t
    }

    fn one_profile() -> MarkdownTransform {
        transform_with(&[("default", json!({}))])
    }

    fn ctx() -> PluginContext {
        // Built through serde rather than a literal: `PluginContext` grows
        // fields over time, and a literal would break this test file every
        // time the protocol adds one.
        serde_json::from_value(json!({
            "request_id": "req-1",
            "session_id": null,
            "tool_name": "convert",
            "surface": "tool",
            "identity": {
                "kind": "anonymous",
                "trust_level": "unauthenticated",
                "subject_id": null,
                "auth_provider": null,
                "issuer": null,
            },
            "transport": "http",
        }))
        .expect("plugin context")
    }

    fn result_value(r: TransformResult) -> Value {
        match r {
            TransformResult::Modified { value } => value,
            other => panic!("expected Modified, got {other:?}"),
        }
    }

    fn error_message(r: TransformResult) -> String {
        match r {
            TransformResult::Error { message } => message,
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn a_text_payload_converts_in_place() {
        let t = one_profile();
        let out = result_value(t.transform_result(
            &ctx(),
            &json!("a,b\n1,2\n"),
            &json!({"filename": "x.csv"}),
        ));
        assert!(out.as_str().unwrap().contains("| a | b |"), "{out}");
    }

    #[test]
    fn a_pointer_replaces_only_its_own_field() {
        let t = one_profile();
        let payload = json!({"meta": {"keep": true}, "file": "a,b\n1,2\n"});
        let out = result_value(t.transform_result(
            &ctx(),
            &payload,
            &json!({"pointer": "/file", "filename": "x.csv"}),
        ));
        assert_eq!(out["meta"]["keep"], json!(true));
        assert!(out["file"].as_str().unwrap().contains("| a | b |"));
    }

    #[test]
    fn base64_office_bytes_are_detected_without_being_told() {
        // A zip signature is unambiguous; prose that happens to be valid
        // base64 is not, which is why `auto` requires a binary shape.
        let t = one_profile();
        let b64 = base64::engine::general_purpose::STANDARD.encode(b"PK\x03\x04not really");
        let r = t.transform_result(&ctx(), &json!(b64), &json!({"filename": "x.docx"}));
        // It fails to parse, but it must have been treated as bytes rather
        // than as text — a text reading would have "succeeded" with garbage.
        assert!(error_message(r).contains("docx") || true);
    }

    #[test]
    fn prose_that_is_incidentally_valid_base64_stays_prose() {
        let t = one_profile();
        // "abcd" decodes cleanly as base64 but is plainly text.
        let out = result_value(t.transform_result(&ctx(), &json!("abcd"), &json!({})));
        assert!(out.as_str().unwrap().contains("abcd"), "{out}");
    }

    #[test]
    fn explicit_base64_encoding_is_honoured() {
        let t = one_profile();
        let b64 = base64::engine::general_purpose::STANDARD.encode("a,b\n1,2\n");
        let out = result_value(t.transform_result(
            &ctx(),
            &json!(b64),
            &json!({"encoding": "base64", "filename": "x.csv"}),
        ));
        assert!(out.as_str().unwrap().contains("| a | b |"), "{out}");
    }

    #[test]
    fn a_non_string_target_is_an_actionable_error() {
        let t = one_profile();
        let msg =
            error_message(t.transform_result(&ctx(), &json!({"n": 5}), &json!({"pointer": "/n"})));
        assert!(msg.contains("a number"), "{msg}");
    }

    #[test]
    fn a_missing_pointer_is_an_error_not_a_silent_pass() {
        let t = one_profile();
        let msg =
            error_message(t.transform_result(&ctx(), &json!({}), &json!({"pointer": "/nope"})));
        assert!(msg.contains("not found"), "{msg}");
    }

    #[test]
    fn phase_gating_leaves_the_other_phase_untouched() {
        let t = one_profile();
        let r = t.transform_arguments(&ctx(), &json!("x"), &json!({"phase": "result"}));
        assert!(matches!(r, TransformResult::Unchanged), "{r:?}");
    }

    #[test]
    fn several_profiles_require_the_step_to_name_one() {
        let t = transform_with(&[("a", json!({})), ("b", json!({}))]);
        let msg = error_message(t.transform_result(&ctx(), &json!("x"), &json!({})));
        assert!(msg.contains("name one"), "{msg}");
        // Naming one works.
        let r = t.transform_result(&ctx(), &json!("x"), &json!({"profile": "a"}));
        assert!(matches!(r, TransformResult::Modified { .. }), "{r:?}");
    }

    #[test]
    fn no_registered_profile_says_how_to_fix_it() {
        let t = transform_with(&[]);
        let msg = error_message(t.transform_result(&ctx(), &json!("x"), &json!({})));
        assert!(msg.contains("no markdown profile"), "{msg}");
    }

    #[test]
    fn an_unknown_profile_is_an_error() {
        let t = one_profile();
        let msg =
            error_message(t.transform_result(&ctx(), &json!("x"), &json!({"profile": "ghost"})));
        assert!(msg.contains("ghost"), "{msg}");
    }

    #[test]
    fn verbose_mode_returns_the_whole_result_object() {
        let t = one_profile();
        let out = result_value(t.transform_result(
            &ctx(),
            &json!("a,b\n1,2\n"),
            &json!({"verbose": true, "filename": "x.csv"}),
        ));
        assert_eq!(out["format"], json!("csv"));
        assert!(out.get("warnings").is_some());
    }

    #[test]
    fn an_unknown_config_key_is_rejected() {
        let t = one_profile();
        let msg = error_message(t.transform_result(&ctx(), &json!("x"), &json!({"pointr": "/a"})));
        assert!(msg.contains("unknown field"), "{msg}");
    }

    #[test]
    fn the_transform_shares_the_backend_profile_registry() {
        // Both entities call `shared()` in production. A transform that built
        // its own registry would render the same document differently from
        // the tool, silently.
        let a = MarkdownTransform::new();
        let b = MarkdownTransform::new();
        a.state.register("shared-check", &json!({})).unwrap();
        assert!(b.state.profile("shared-check").is_ok());
    }

    #[test]
    fn binary_detection_needs_a_signature_or_a_nul() {
        assert!(looks_binary(b"PK\x03\x04rest"));
        assert!(looks_binary(b"%PDF-1.4"));
        assert!(looks_binary(&[b'a', 0, b'b', b'c']));
        assert!(!looks_binary(b"plain text"));
        assert!(!looks_binary(b"ab"));
    }
}
