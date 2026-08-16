//! Profile configuration — the parts of a profile that need a host.
//!
//! Everything the engine understands on its own (limits, output options,
//! format selection, templates) lives in `mcpg_markdown_convert::ConvertOptions`
//! and is flattened in here, so an operator sees one object rather than two.

use mcpg_markdown_convert::{ConvertOptions, Engine};
use serde::{Deserialize, Serialize};

/// One named conversion profile. Both entities read the same map, which is
/// the point of keeping them in one plugin: a tool call and a pipeline step
/// cannot silently render the same document differently.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileConfig {
    /// Which acquisition modes this profile accepts. There is no filesystem
    /// mode — see the crate docs.
    #[serde(default)]
    pub sources: Sources,
    #[serde(default)]
    pub url: UrlOptions,
    #[serde(default)]
    pub llm: LlmOptions,
    /// Engine options: `limits`, `output`, `formats`, `templates`.
    #[serde(flatten)]
    pub convert: ConvertOptions,
}

/// Accepted input modes, in the order the plugin looks for them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sources {
    /// `content` (base64) or `text` in the tool arguments.
    #[serde(default = "yes")]
    pub inline: bool,
    /// `mcpg-resource://…`, read through the host's content store. The
    /// intended production path: the bytes never pass through the model's
    /// context.
    #[serde(default = "yes")]
    pub resource: bool,
    /// `https://…`. Requires the `network_outbound` capability and is refused
    /// at boot if the profile enables it without the grant.
    #[serde(default)]
    pub url: bool,
}

fn yes() -> bool {
    true
}

impl Default for Sources {
    fn default() -> Self {
        Self {
            inline: true,
            resource: true,
            url: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UrlOptions {
    /// Allow fetching from private, loopback and link-local addresses. Off by
    /// default: a caller-supplied URL reaching `169.254.169.254` is the
    /// classic cloud-metadata exfiltration path.
    #[serde(default)]
    pub allow_private_addresses: bool,
    #[serde(default = "d_redirects")]
    pub max_redirects: u32,
    #[serde(default = "d_timeout")]
    pub timeout_ms: u64,
    /// Host allowlist. Empty means any host that passes the address guard.
    #[serde(default)]
    pub allow_hosts: Vec<String>,
}

fn d_redirects() -> u32 {
    3
}
fn d_timeout() -> u64 {
    20_000
}

impl Default for UrlOptions {
    fn default() -> Self {
        Self {
            allow_private_addresses: false,
            max_redirects: d_redirects(),
            timeout_ms: d_timeout(),
            allow_hosts: Vec::new(),
        }
    }
}

/// Optional model-driven enrichment.
///
/// The converter never holds a provider credential: `binding` names an
/// existing LLM binding and every call is dispatched through the host, which
/// is what keeps budgets, retries, caching and audit in one place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LlmOptions {
    /// Name of a configured tool backed by an LLM binding. `None` disables
    /// enrichment entirely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<String>,
    #[serde(default)]
    pub enrich: EnrichOptions,
    #[serde(default = "d_max_calls")]
    pub max_calls_per_document: u32,
    /// Cache captions by content hash. Enrichment is the one part of a
    /// conversion that is neither cheap nor reproducible, so this is on by
    /// default.
    #[serde(default = "yes")]
    pub cache: bool,
    /// Prompt for image captioning. Operator-visible because the useful
    /// caption for an invoice and for a photograph are different documents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_prompt: Option<String>,
    /// Prompt for reading a scanned PDF. A separate knob because the useful
    /// instruction for a scanned invoice and a scanned contract differ.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pdf_prompt: Option<String>,
}

fn d_max_calls() -> u32 {
    8
}

// Written out rather than derived: `#[derive(Default)]` ignores serde's
// `default = "…"` attributes, so a derived Default would disagree with the
// values an operator gets from an empty config block.
impl Default for LlmOptions {
    fn default() -> Self {
        Self {
            binding: None,
            enrich: EnrichOptions::default(),
            max_calls_per_document: d_max_calls(),
            cache: true,
            image_prompt: None,
            audio_prompt: None,
            pdf_prompt: None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnrichOptions {
    #[serde(default)]
    pub images: ImageEnrichment,
    #[serde(default)]
    pub audio: AudioEnrichment,
    #[serde(default)]
    pub pdf: PdfEnrichment,
}

/// What to do with a PDF whose pages carry no text layer.
///
/// A separate knob from `images` because the cost profile is different: this
/// sends a whole document rather than one picture, so an operator who wants
/// image captions does not silently also buy per-document OCR.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PdfEnrichment {
    #[default]
    Off,
    /// Send the document to the vision model and use what it reads back.
    Ocr,
}

impl PdfEnrichment {
    #[must_use]
    pub fn is_on(self) -> bool {
        matches!(self, PdfEnrichment::Ocr)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageEnrichment {
    #[default]
    Off,
    /// Describe what the image depicts.
    Caption,
    /// Read the text in the image. The plugin's answer to OCR — a vision
    /// call, not Tesseract, because no pure-Rust OCR engine belongs on a
    /// request path.
    Ocr,
    CaptionAndOcr,
}

impl ImageEnrichment {
    #[must_use]
    pub fn is_on(self) -> bool {
        !matches!(self, ImageEnrichment::Off)
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ImageEnrichment::Off => "off",
            ImageEnrichment::Caption => "caption",
            ImageEnrichment::Ocr => "ocr",
            ImageEnrichment::CaptionAndOcr => "caption+ocr",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioEnrichment {
    #[default]
    Off,
    Transcribe,
}

impl AudioEnrichment {
    #[must_use]
    pub fn is_on(self) -> bool {
        matches!(self, AudioEnrichment::Transcribe)
    }
}

/// A profile after validation: the compiled engine plus the host-facing bits.
pub struct Profile {
    pub name: String,
    pub engine: Engine,
    pub config: ProfileConfig,
}

impl std::fmt::Debug for Profile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Profile")
            .field("name", &self.name)
            .field("engine", &self.engine)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_accept_inline_and_resource_but_not_url() {
        let p: ProfileConfig = serde_json::from_str("{}").unwrap();
        assert!(p.sources.inline);
        assert!(p.sources.resource);
        assert!(!p.sources.url, "url must be opt-in");
    }

    #[test]
    fn there_is_no_filesystem_source_to_enable() {
        // A profile naming one must fail loudly rather than be silently
        // ignored: an operator who wrote it believes it works.
        let err =
            serde_json::from_str::<ProfileConfig>(r#"{"sources":{"path":true}}"#).unwrap_err();
        assert!(format!("{err}").contains("unknown field"), "{err}");
        let err = serde_json::from_str::<ProfileConfig>(r#"{"sources":{"filesystem":true}}"#)
            .unwrap_err();
        assert!(format!("{err}").contains("unknown field"), "{err}");
    }

    #[test]
    fn engine_options_flatten_into_the_profile() {
        let p: ProfileConfig =
            serde_json::from_str(r#"{"limits":{"max_depth":5},"output":{"front_matter":"yaml"}}"#)
                .unwrap();
        assert_eq!(p.convert.limits.max_depth, 5);
        assert_eq!(
            p.convert.output.front_matter,
            mcpg_markdown_convert::FrontMatter::Yaml
        );
    }

    #[test]
    fn a_typo_anywhere_in_the_profile_is_rejected() {
        for bad in [
            r#"{"sourcs":{}}"#,
            r#"{"llm":{"bindingg":"x"}}"#,
            r#"{"url":{"timeout":1}}"#,
        ] {
            let err = serde_json::from_str::<ProfileConfig>(bad).unwrap_err();
            assert!(format!("{err}").contains("unknown field"), "{bad}: {err}");
        }
    }

    #[test]
    fn private_addresses_are_refused_by_default() {
        let u = UrlOptions::default();
        assert!(!u.allow_private_addresses);
        assert_eq!(u.max_redirects, 3);
    }

    #[test]
    fn enrichment_is_off_unless_asked_for() {
        let l = LlmOptions::default();
        assert!(l.binding.is_none());
        assert!(!l.enrich.images.is_on());
        assert!(!l.enrich.audio.is_on());
        assert!(l.cache);
    }

    #[test]
    fn the_profile_round_trips_through_json() {
        let p = ProfileConfig::default();
        let v = serde_json::to_value(&p).unwrap();
        let back: ProfileConfig = serde_json::from_value(v).unwrap();
        assert_eq!(p, back);
    }
}
