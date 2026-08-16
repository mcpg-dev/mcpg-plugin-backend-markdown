//! State shared by the two entities.
//!
//! One `.so` carries both a `backend` and a `transform` entity, and the host
//! constructs each independently. They share this state so the converter
//! registry, the compiled templates and the enrichment cache are built once —
//! and, more importantly, so a tool call and a pipeline step cannot render the
//! same document differently.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use mcpg_markdown_convert::Engine;
use mcpg_plugin_protocol::BackendError;

use crate::config::{Profile, ProfileConfig};

/// Process-global shared state. One `.so` instance per gateway process, so a
/// single shared `Arc` is both correct and minimal — the same shape
/// `backend.twilio` uses for its three entities.
pub fn shared() -> Arc<MarkdownState> {
    static STATE: OnceLock<Arc<MarkdownState>> = OnceLock::new();
    STATE
        .get_or_init(|| Arc::new(MarkdownState::default()))
        .clone()
}

#[derive(Default)]
pub struct MarkdownState {
    profiles: RwLock<HashMap<String, Arc<Profile>>>,
    /// Enrichment results, keyed by content hash + prompt. Shared across
    /// profiles and calls: the same image described twice costs one call.
    enrichment_cache: Mutex<HashMap<String, String>>,
}

impl std::fmt::Debug for MarkdownState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MarkdownState")
            .field("profiles", &self.profile_names())
            .finish()
    }
}

impl MarkdownState {
    /// Validate a profile and compile its engine.
    ///
    /// Everything that can fail does so here, at boot: a template that will
    /// not parse, a `formats.enable` entry naming no converter, a limit set
    /// to zero. An operator hears about a broken profile at startup rather
    /// than on the first call that needs it.
    pub fn register(&self, name: &str, spec: &serde_json::Value) -> Result<(), BackendError> {
        let config: ProfileConfig =
            serde_json::from_value(spec.clone()).map_err(|e| BackendError::InvalidSpec {
                message: format!("markdown profile {name:?}: {e}"),
            })?;

        validate(name, &config)?;

        let engine =
            Engine::new(config.convert.clone()).map_err(|e| BackendError::InvalidSpec {
                message: format!("markdown profile {name:?}: {e}"),
            })?;

        let profile = Arc::new(Profile {
            name: name.to_owned(),
            engine,
            config,
        });
        self.profiles
            .write()
            .expect("profile map poisoned")
            .insert(name.to_owned(), profile);
        Ok(())
    }

    pub fn profile(&self, name: &str) -> Result<Arc<Profile>, BackendError> {
        self.profiles
            .read()
            .expect("profile map poisoned")
            .get(name)
            .cloned()
            .ok_or_else(|| BackendError::ProfileNotFound {
                backend_name: format!(
                    "{name} (registered markdown profiles: {})",
                    self.profile_names().join(", ")
                ),
            })
    }

    /// The profile a transform step should use when it names none. A single
    /// registered profile is unambiguous; more than one is not, and guessing
    /// would silently apply the wrong template.
    pub fn sole_profile(&self) -> Option<Arc<Profile>> {
        let map = self.profiles.read().expect("profile map poisoned");
        if map.len() == 1 {
            map.values().next().cloned()
        } else {
            None
        }
    }

    pub fn profile_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .profiles
            .read()
            .expect("profile map poisoned")
            .keys()
            .cloned()
            .collect();
        names.sort();
        names
    }

    /// Run `f` with the enrichment cache. Held only for the duration of the
    /// pass, never across a dispatch.
    pub fn with_cache<T>(&self, f: impl FnOnce(&mut HashMap<String, String>) -> T) -> T {
        let mut guard = self.enrichment_cache.lock().expect("cache poisoned");
        f(&mut guard)
    }
}

/// Cross-field checks the serde derive cannot express.
fn validate(name: &str, config: &ProfileConfig) -> Result<(), BackendError> {
    let bad = |message: String| BackendError::InvalidSpec {
        message: format!("markdown profile {name:?}: {message}"),
    };

    if !config.sources.inline && !config.sources.resource && !config.sources.url {
        return Err(bad(
            "every source is disabled, so the profile can never convert anything".to_owned(),
        ));
    }

    let limits = &config.convert.limits;
    for (label, value) in [
        ("max_input_bytes", limits.max_input_bytes),
        ("max_output_bytes", limits.max_output_bytes),
        ("max_expanded_bytes", limits.max_expanded_bytes),
        ("timeout_ms", limits.timeout_ms),
    ] {
        if value == 0 {
            return Err(bad(format!("limits.{label} must be greater than zero")));
        }
    }
    if limits.max_expanded_bytes < limits.max_input_bytes {
        return Err(bad(format!(
            "limits.max_expanded_bytes ({}) is below max_input_bytes ({}); \
             an archive can never expand to less than itself",
            limits.max_expanded_bytes, limits.max_input_bytes
        )));
    }
    if limits.max_depth == 0 {
        return Err(bad(
            "limits.max_depth must be at least 1 (0 would refuse every archive)".to_owned(),
        ));
    }

    // Enrichment without a binding is the operator's most likely mistake, and
    // silently doing nothing is the worst possible response to it.
    let wants_enrichment = config.llm.enrich.images.is_on()
        || config.llm.enrich.audio.is_on()
        || config.llm.enrich.pdf.is_on();
    if wants_enrichment && config.llm.binding.is_none() {
        return Err(bad(
            "llm.enrich is set but llm.binding names no LLM binding, so enrichment \
             would silently never run"
                .to_owned(),
        ));
    }
    if config.llm.binding.is_some() && !wants_enrichment {
        return Err(bad(
            "llm.binding is set but llm.enrich is off, so the binding would never be \
             called; set llm.enrich.images or llm.enrich.audio, or drop llm.binding"
                .to_owned(),
        ));
    }
    if wants_enrichment && config.llm.max_calls_per_document == 0 {
        return Err(bad(
            "llm.max_calls_per_document is 0, which disables the enrichment it enables".to_owned(),
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn state() -> MarkdownState {
        MarkdownState::default()
    }

    fn register(spec: serde_json::Value) -> Result<(), BackendError> {
        state().register("default", &spec)
    }

    #[test]
    fn an_empty_spec_registers_the_defaults() {
        assert!(register(json!({})).is_ok());
    }

    #[test]
    fn a_registered_profile_is_retrievable() {
        let s = state();
        s.register("default", &json!({})).unwrap();
        assert!(s.profile("default").is_ok());
        assert_eq!(s.profile_names(), vec!["default"]);
    }

    #[test]
    fn an_unknown_profile_names_the_ones_that_exist() {
        let s = state();
        s.register("a", &json!({})).unwrap();
        let e = s.profile("b").unwrap_err();
        let text = format!("{e:?}");
        assert!(text.contains('b'), "{text}");
        assert!(text.contains("registered markdown profiles: a"), "{text}");
    }

    #[test]
    fn a_broken_template_fails_at_registration_not_at_call_time() {
        let e = register(json!({"templates": {"document": "{% for x in %}"}})).unwrap_err();
        assert!(matches!(e, BackendError::InvalidSpec { .. }), "{e:?}");
    }

    #[test]
    fn an_unknown_format_name_fails_at_registration() {
        let e = register(json!({"formats": {"enable": ["nonesuch"]}})).unwrap_err();
        assert!(format!("{e:?}").contains("nonesuch"), "{e:?}");
    }

    #[test]
    fn disabling_every_source_is_rejected() {
        let e = register(json!({
            "sources": {"inline": false, "resource": false, "url": false}
        }))
        .unwrap_err();
        assert!(format!("{e:?}").contains("never convert"), "{e:?}");
    }

    #[test]
    fn zero_limits_are_rejected() {
        for field in [
            "max_input_bytes",
            "max_output_bytes",
            "max_expanded_bytes",
            "timeout_ms",
        ] {
            let e = register(json!({"limits": {field: 0}})).unwrap_err();
            assert!(format!("{e:?}").contains(field), "{field}: {e:?}");
        }
        let e = register(json!({"limits": {"max_depth": 0}})).unwrap_err();
        assert!(format!("{e:?}").contains("max_depth"), "{e:?}");
    }

    #[test]
    fn an_expansion_ceiling_below_the_input_ceiling_is_rejected() {
        let e = register(json!({
            "limits": {"max_input_bytes": 1000, "max_expanded_bytes": 100}
        }))
        .unwrap_err();
        assert!(format!("{e:?}").contains("expand to less"), "{e:?}");
    }

    #[test]
    fn enrichment_without_a_binding_is_rejected_rather_than_ignored() {
        let e = register(json!({"llm": {"enrich": {"images": "caption"}}})).unwrap_err();
        assert!(format!("{e:?}").contains("silently never run"), "{e:?}");
    }

    #[test]
    fn a_binding_with_nothing_to_enrich_is_rejected() {
        let e = register(json!({"llm": {"binding": "vision"}})).unwrap_err();
        assert!(format!("{e:?}").contains("never be"), "{e:?}");
    }

    #[test]
    fn a_valid_enrichment_profile_registers() {
        assert!(
            register(json!({
                "llm": {"binding": "vision", "enrich": {"images": "caption"}}
            }))
            .is_ok()
        );
    }

    #[test]
    fn zero_enrichment_calls_contradicts_enabling_enrichment() {
        let e = register(json!({
            "llm": {
                "binding": "vision",
                "enrich": {"images": "caption"},
                "max_calls_per_document": 0
            }
        }))
        .unwrap_err();
        assert!(format!("{e:?}").contains("max_calls_per_document"), "{e:?}");
    }

    #[test]
    fn the_sole_profile_is_only_implied_when_it_is_unambiguous() {
        let s = state();
        assert!(s.sole_profile().is_none());
        s.register("only", &json!({})).unwrap();
        assert!(s.sole_profile().is_some());
        s.register("second", &json!({})).unwrap();
        assert!(
            s.sole_profile().is_none(),
            "guessing between profiles would silently apply the wrong template"
        );
    }

    #[test]
    fn re_registering_a_name_replaces_it() {
        let s = state();
        s.register("p", &json!({"limits": {"max_depth": 1}}))
            .unwrap();
        s.register("p", &json!({"limits": {"max_depth": 2}}))
            .unwrap();
        assert_eq!(s.profile("p").unwrap().config.convert.limits.max_depth, 2);
    }
}
