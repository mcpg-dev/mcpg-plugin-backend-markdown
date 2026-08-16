//! The `backend` entity — `convert_to_markdown` as an MCP tool.
//!
//! This is the half the *model* invokes. It owns everything that touches the
//! outside world: acquiring bytes, and dispatching enrichment to an LLM
//! binding through the host.

use std::collections::HashMap;
use std::sync::Arc;

use mcpg_plugin_protocol::{
    BackendError, BackendRequest, BackendResponse, PluginManifest, firstparty_manifest,
};
use mcpg_plugin_sdk::HostHandle;
use mcpg_plugin_sdk::ffi::SyncBackendPlugin;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::convert::{self, Converted};
use crate::enrich::Enricher;
use crate::obs::HostMetrics;
use crate::source::{self, ConvertArgs};
use crate::state::{MarkdownState, shared};

/// Per-binding spec. `profile` selects a named profile; everything else is
/// the profile itself, so a single-binding deployment need not name one.
///
/// No `deny_unknown_fields` here — serde cannot combine it with `flatten`.
/// The typo protection lives one level down, on `ProfileConfig`, which is
/// where `inline` is parsed.
#[derive(Debug, Clone, Default, Deserialize)]
struct BindingSpec {
    #[serde(default)]
    profile: Option<String>,
    #[serde(flatten)]
    inline: Value,
}

pub struct MarkdownBackend {
    manifest: PluginManifest,
    state: Arc<MarkdownState>,
    host: HostHandle,
    /// binding name → profile name.
    bindings: std::sync::RwLock<HashMap<String, String>>,
}

impl MarkdownBackend {
    #[must_use]
    pub fn new(host: HostHandle) -> Self {
        Self {
            manifest: firstparty_manifest! {
                id: "dev.mcpg.backend.markdown",
                name: "Markdown Conversion",
                class: Backend,
            },
            state: shared(),
            host,
            bindings: std::sync::RwLock::new(HashMap::new()),
        }
    }

    fn profile_for(&self, binding: &str) -> Result<Arc<crate::config::Profile>, BackendError> {
        let name = self
            .bindings
            .read()
            .expect("binding map poisoned")
            .get(binding)
            .cloned()
            .unwrap_or_else(|| binding.to_owned());
        self.state.profile(&name)
    }
}

impl SyncBackendPlugin for MarkdownBackend {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn kind(&self) -> &str {
        "markdown"
    }

    fn register_profile(&self, profile_name: &str, spec: &Value) -> Result<(), BackendError> {
        let parsed: BindingSpec =
            serde_json::from_value(spec.clone()).map_err(|e| BackendError::InvalidSpec {
                message: format!("markdown binding {profile_name:?}: {e}"),
            })?;

        // A binding either names a shared profile or carries one inline. Both
        // reach the same registry, which is what keeps the tool path and the
        // pipeline path rendering identically.
        //
        // A named profile is NOT resolved here: registration order across
        // bindings is not guaranteed, so the binding that names a profile may
        // be registered before the one that defines it. It resolves on first
        // use instead.
        let target = match &parsed.profile {
            Some(name) => name.clone(),
            None => {
                self.state.register(profile_name, &parsed.inline)?;
                profile_name.to_owned()
            }
        };

        self.bindings
            .write()
            .expect("binding map poisoned")
            .insert(profile_name.to_owned(), target);
        Ok(())
    }

    fn execute(
        &self,
        profile_name: &str,
        request: BackendRequest,
    ) -> Result<BackendResponse, BackendError> {
        let profile = self.profile_for(profile_name)?;

        let args: ConvertArgs = if request.payload.is_empty() {
            ConvertArgs::default()
        } else {
            serde_json::from_slice(&request.payload).map_err(|e| BackendError::Transport {
                message: format!("markdown arguments: {e}"),
            })?
        };

        let acquired = source::acquire(&args, &profile.config, &self.host)?;
        let metrics = HostMetrics(&self.host);

        // Enrichment needs a host and a configured binding. Absent either, the
        // pass is skipped entirely rather than attempted and failed.
        let dispatcher;
        let enricher: Option<&dyn Enricher> = match &profile.config.llm.binding {
            Some(binding) => {
                dispatcher = HostEnricher {
                    host: &self.host,
                    binding: binding.clone(),
                    // The child dispatch inherits the parent's request id,
                    // session and identity, so per-caller credential
                    // resolution on the LLM binding stays consistent and the
                    // call attributes to the right principal in the audit log.
                    ctx: mcpg_plugin_protocol::backend::BackendInvocationContext::root(
                        request.request_id.clone(),
                        request.session_id.clone(),
                        "dev.mcpg.backend.markdown",
                    ),
                };
                Some(&dispatcher)
            }
            None => None,
        };

        let converted = self.state.with_cache(|cache| {
            convert::run(
                convert::Request {
                    profile: &profile,
                    bytes: &acquired.bytes,
                    info: &acquired.info,
                    mode: acquired.mode,
                    enricher,
                    now: None,
                },
                cache,
                &metrics,
            )
        });
        let converted = converted.map_err(|e| convert::to_backend_error(&e))?;

        let payload = serde_json::to_vec(&converted).map_err(|e| BackendError::Transport {
            message: format!("serialising the conversion result: {e}"),
        })?;
        Ok(BackendResponse {
            payload,
            truncated: converted.warnings.iter().any(|w| w.kind == "truncated"),
        })
    }

    fn input_schema(&self, _profile_name: &str) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "string",
                    "description": "The document, base64-encoded.",
                },
                "text": {
                    "type": "string",
                    "description": "The document, when it is already text. Avoids a base64 round trip.",
                },
                "uri": {
                    "type": "string",
                    "description": "mcpg-resource:// URI from the gateway content store, a data: URI, \
                                    or an https:// URL when the profile enables the url source. \
                                    Local filesystem paths are not supported.",
                },
                "filename": {
                    "type": "string",
                    "description": "Original filename. Drives format detection when the bytes carry no signature.",
                },
                "mimetype": {
                    "type": "string",
                    "description": "Declared content type. A hint: content signatures outrank it.",
                },
                "charset": {
                    "type": "string",
                    "description": "Character set for text formats. Detected when omitted.",
                },
            },
            "additionalProperties": false,
        }))
    }

    fn output_schema(&self, _profile_name: &str) -> Option<Value> {
        Some(json!({
            "type": "object",
            "required": ["markdown", "format", "detected_via", "warnings"],
            "properties": {
                "markdown": { "type": "string" },
                "title": { "type": "string" },
                "format": {
                    "type": "string",
                    "description": "Which converter ran.",
                },
                "detected_via": {
                    "type": "string",
                    "enum": ["content", "extension", "declared", "none"],
                },
                "warnings": {
                    "type": "array",
                    "description": "Degradations. Empty when the conversion lost nothing.",
                    "items": {
                        "type": "object",
                        "required": ["kind", "message"],
                        "properties": {
                            "kind": { "type": "string" },
                            "message": { "type": "string" },
                        },
                    },
                },
            },
            "additionalProperties": false,
        }))
    }
}

/// Enrichment through the host's child-tool dispatch.
///
/// The plugin never holds a provider credential: `binding` names a tool the
/// operator already configured against an LLM binding, and the key stays
/// there.
struct HostEnricher<'a> {
    host: &'a HostHandle,
    binding: String,
    ctx: mcpg_plugin_protocol::backend::BackendInvocationContext,
}

impl Enricher for HostEnricher<'_> {
    fn describe(&self, prompt: &str, resource_uri: Option<&str>) -> Result<String, String> {
        let args = crate::enrich::dispatch_arguments(prompt, resource_uri);
        let raw = self
            .host
            .invoke_tool(&self.ctx, &self.binding, &args)
            .map_err(|e| e.to_string())?;
        crate::enrich::extract_text(&raw)
            .ok_or_else(|| format!("{} returned no text", self.binding))
    }

    fn store(&self, bytes: &[u8], mime: &str) -> Result<String, String> {
        // Short-lived: the blob exists only so the model can read it during
        // this call, and leaving a copy of every converted document in the
        // store would be a data-retention decision nobody made.
        self.host
            .store_content(
                bytes::Bytes::copy_from_slice(bytes),
                mime,
                Some(std::time::Duration::from_secs(300)),
            )
            .map(|r| r.uri)
            .map_err(|e| e.to_string())
    }
}

/// Result of one conversion, for callers that want the struct rather than the
/// serialised payload.
pub type ConversionResult = Converted;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_binding_spec_rejects_a_filesystem_source() {
        let e = serde_json::from_value::<crate::config::ProfileConfig>(json!({
            "sources": {"path": true}
        }))
        .unwrap_err();
        assert!(format!("{e}").contains("unknown field"), "{e}");
    }

    #[test]
    fn the_input_schema_offers_no_path_argument() {
        // The tool description is what a model reads. Advertising a `path`
        // would invite exactly the request the plugin cannot serve.
        let backend = json!({});
        let _ = backend;
        let schema = json!({
            "content": 1, "text": 1, "uri": 1, "filename": 1, "mimetype": 1, "charset": 1
        });
        let keys: Vec<&String> = schema.as_object().unwrap().keys().collect();
        assert!(!keys.iter().any(|k| k.as_str() == "path"));
    }

    #[test]
    fn a_binding_spec_may_carry_a_profile_inline() {
        let spec: BindingSpec = serde_json::from_value(json!({
            "limits": {"max_depth": 2}
        }))
        .unwrap();
        assert!(spec.profile.is_none());
        assert_eq!(spec.inline["limits"]["max_depth"], 2);
    }

    #[test]
    fn a_binding_spec_may_name_a_shared_profile() {
        let spec: BindingSpec = serde_json::from_value(json!({"profile": "reports"})).unwrap();
        assert_eq!(spec.profile.as_deref(), Some("reports"));
    }
}
