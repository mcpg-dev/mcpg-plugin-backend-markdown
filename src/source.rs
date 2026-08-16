//! Input acquisition — where the bytes come from.
//!
//! Three modes, and deliberately no fourth. There is **no local-filesystem
//! mode**: a gateway process holding a `filesystem_read` grant becomes a file
//! exfiltration primitive the moment a tool argument reaches it, because the
//! model chooses the path. An operator who needs a local file converts it
//! through a source that already has authenticated, audited reach into that
//! filesystem — `backend.sftp`, `backend.smb` — and hands this plugin an
//! `mcpg-resource://` URI.

use base64::Engine as _;
use mcpg_markdown_convert::StreamInfo;
use mcpg_plugin_protocol::BackendError;
use mcpg_plugin_sdk::HostHandle;
use serde::Deserialize;

use crate::config::ProfileConfig;

/// The one host capability acquisition needs.
///
/// A trait rather than a bare [`HostHandle`] so every acquisition path is
/// unit-testable: a `HostHandle` can only be built by a real gateway, and
/// "does `file://` get refused?" should not need one.
pub trait ContentSource {
    /// Read a `mcpg-resource://` URI. `Ok(None)` means it did not resolve.
    fn fetch_resource(&self, uri: &str) -> Result<Option<Vec<u8>>, String>;
}

impl ContentSource for HostHandle {
    fn fetch_resource(&self, uri: &str) -> Result<Option<Vec<u8>>, String> {
        self.fetch_content(uri)
            .map(|o| o.map(|b| b.to_vec()))
            .map_err(|e| e.to_string())
    }
}

/// The tool's arguments. Exactly one of the source fields may be set.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConvertArgs {
    /// Base64-encoded bytes.
    #[serde(default)]
    pub content: Option<String>,
    /// Text that is already text — skips a base64 round trip for callers that
    /// have a string in hand.
    #[serde(default)]
    pub text: Option<String>,
    /// `mcpg-resource://…`, or an `https://…` URL when the profile allows it.
    #[serde(default)]
    pub uri: Option<String>,
    /// Hint: the original filename. Drives extension-based detection.
    #[serde(default)]
    pub filename: Option<String>,
    /// Hint: the declared content type.
    #[serde(default)]
    pub mimetype: Option<String>,
    /// Hint: the character set for text formats.
    #[serde(default)]
    pub charset: Option<String>,
}

/// Acquired bytes plus what we know about them.
#[derive(Debug)]
pub struct Acquired {
    pub bytes: Vec<u8>,
    pub info: StreamInfo,
    /// Which mode produced the bytes. A metric label.
    pub mode: &'static str,
}

/// Resolve arguments into bytes.
pub fn acquire(
    args: &ConvertArgs,
    cfg: &ProfileConfig,
    store: &dyn ContentSource,
) -> Result<Acquired, BackendError> {
    let named = [
        args.content.is_some(),
        args.text.is_some(),
        args.uri.is_some(),
    ]
    .iter()
    .filter(|b| **b)
    .count();
    if named == 0 {
        return Err(invalid(
            "one of `content`, `text` or `uri` is required".to_owned(),
        ));
    }
    if named > 1 {
        return Err(invalid(
            "give exactly one of `content`, `text` or `uri`".to_owned(),
        ));
    }

    let mut info = StreamInfo::new();
    if let Some(f) = &args.filename {
        info = info.with_filename(f.clone());
    }
    if let Some(m) = &args.mimetype {
        info = info.with_mimetype(m.clone());
    }
    if let Some(c) = &args.charset {
        info = info.with_charset(c.clone());
    }

    if let Some(text) = &args.text {
        require(cfg.sources.inline, "inline")?;
        return Ok(Acquired {
            bytes: text.as_bytes().to_vec(),
            // A caller who handed us a string has told us it is text; without
            // this the detector would have to guess at a charset it knows.
            info: if info.charset.is_none() {
                info.with_charset("utf-8")
            } else {
                info
            },
            mode: "inline",
        });
    }

    if let Some(b64) = &args.content {
        require(cfg.sources.inline, "inline")?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64.trim())
            .map_err(|e| invalid(format!("`content` is not valid base64: {e}")))?;
        return Ok(Acquired {
            bytes,
            info,
            mode: "inline",
        });
    }

    let uri = args.uri.as_deref().unwrap_or_default().trim();
    if uri.starts_with("mcpg-resource://") {
        require(cfg.sources.resource, "resource")?;
        let bytes = store
            .fetch_resource(uri)
            .map_err(|message| BackendError::Transport {
                message: format!("content store: {message}"),
            })?
            .ok_or_else(|| {
                invalid(format!(
                    "{uri} did not resolve — it may have expired or belong to another session"
                ))
            })?;
        return Ok(Acquired {
            bytes,
            info: info.with_url(uri.to_owned()),
            mode: "resource",
        });
    }

    if let Some(rest) = uri.strip_prefix("data:") {
        require(cfg.sources.inline, "inline")?;
        return decode_data_uri(rest, info);
    }

    if uri.starts_with("http://") || uri.starts_with("https://") {
        require(cfg.sources.url, "url")?;
        return crate::fetch::get(uri, cfg, info);
    }

    // Naming the refused schemes explicitly beats a generic parse error: an
    // operator reaching for `file://` should learn that it is a deliberate
    // omission, not a gap.
    if uri.starts_with("file://") || uri.starts_with('/') || uri.starts_with("./") {
        return Err(invalid(
            "local filesystem paths are not supported and will not be: fetch the file \
             through a backend that has audited access to it (sftp, smb, s3) and pass \
             the resulting mcpg-resource:// URI"
                .to_owned(),
        ));
    }

    Err(invalid(format!(
        "unsupported uri scheme in {uri:?} — expected mcpg-resource://, data:, \
         or https:// (when the profile enables the url source)"
    )))
}

fn require(enabled: bool, mode: &str) -> Result<(), BackendError> {
    if enabled {
        Ok(())
    } else {
        Err(invalid(format!(
            "the {mode:?} source is not enabled for this profile"
        )))
    }
}

fn decode_data_uri(rest: &str, info: StreamInfo) -> Result<Acquired, BackendError> {
    let (meta, payload) = rest
        .split_once(',')
        .ok_or_else(|| invalid("malformed data: URI (no comma)".to_owned()))?;
    let mut info = info;
    let mime = meta.split(';').next().unwrap_or("").trim();
    if !mime.is_empty() && info.mimetype.is_none() {
        info = info.with_mimetype(mime.to_owned());
    }
    let bytes = if meta.contains("base64") {
        base64::engine::general_purpose::STANDARD
            .decode(payload)
            .map_err(|e| invalid(format!("data: URI payload is not valid base64: {e}")))?
    } else {
        payload.as_bytes().to_vec()
    };
    Ok(Acquired {
        bytes,
        info,
        mode: "inline",
    })
}

/// A caller-supplied argument was wrong.
///
/// `BackendError` has no `InvalidRequest` variant, so this rides in
/// `Transport` — the convention the other backends use for every runtime
/// failure. The message is written to be actionable, since the taxonomy will
/// not carry that information for us.
pub(crate) fn invalid(message: String) -> BackendError {
    BackendError::Transport { message }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Sources;

    fn args() -> ConvertArgs {
        ConvertArgs::default()
    }

    fn cfg_with(sources: Sources) -> ProfileConfig {
        ProfileConfig {
            sources,
            ..ProfileConfig::default()
        }
    }

    /// A content store that answers from a fixed map.
    struct StubStore(Vec<(String, Vec<u8>)>);

    impl ContentSource for StubStore {
        fn fetch_resource(&self, uri: &str) -> Result<Option<Vec<u8>>, String> {
            Ok(self
                .0
                .iter()
                .find(|(u, _)| u == uri)
                .map(|(_, b)| b.clone()))
        }
    }

    struct FailingStore;

    impl ContentSource for FailingStore {
        fn fetch_resource(&self, _uri: &str) -> Result<Option<Vec<u8>>, String> {
            Err("store unavailable".to_owned())
        }
    }

    fn acquire_hostless(a: &ConvertArgs, c: &ProfileConfig) -> Result<Acquired, BackendError> {
        acquire(a, c, &StubStore(Vec::new()))
    }

    #[test]
    fn text_arrives_as_utf8_bytes() {
        let mut a = args();
        a.text = Some("hello".into());
        let got = acquire_hostless(&a, &ProfileConfig::default()).unwrap();
        assert_eq!(got.bytes, b"hello");
        assert_eq!(got.mode, "inline");
        assert_eq!(got.info.charset.as_deref(), Some("utf-8"));
    }

    #[test]
    fn base64_content_decodes() {
        let mut a = args();
        a.content = Some("aGVsbG8=".into());
        assert_eq!(
            acquire_hostless(&a, &ProfileConfig::default())
                .unwrap()
                .bytes,
            b"hello"
        );
    }

    #[test]
    fn bad_base64_is_an_error_not_a_panic() {
        let mut a = args();
        a.content = Some("!!!not base64!!!".into());
        let e = acquire_hostless(&a, &ProfileConfig::default()).unwrap_err();
        assert!(format!("{e:?}").contains("base64"), "{e:?}");
    }

    #[test]
    fn exactly_one_source_field_is_required() {
        let e = acquire_hostless(&args(), &ProfileConfig::default()).unwrap_err();
        assert!(format!("{e:?}").contains("required"), "{e:?}");

        let mut a = args();
        a.text = Some("x".into());
        a.content = Some("eA==".into());
        let e = acquire_hostless(&a, &ProfileConfig::default()).unwrap_err();
        assert!(format!("{e:?}").contains("exactly one"), "{e:?}");
    }

    #[test]
    fn file_uris_are_refused_with_an_explanation() {
        for uri in ["file:///etc/passwd", "/etc/passwd", "./secret.txt"] {
            let mut a = args();
            a.uri = Some(uri.into());
            let e = acquire_hostless(&a, &ProfileConfig::default()).unwrap_err();
            let text = format!("{e:?}");
            assert!(text.contains("not supported"), "{uri}: {text}");
            assert!(text.contains("mcpg-resource"), "{uri}: {text}");
        }
    }

    #[test]
    fn http_is_refused_unless_the_profile_enables_it() {
        let mut a = args();
        a.uri = Some("https://example.invalid/x.pdf".into());
        let e = acquire_hostless(&a, &ProfileConfig::default()).unwrap_err();
        let text = format!("{e:?}");
        assert!(text.contains("url"), "{text}");
        assert!(text.contains("not enabled"), "{text}");
    }

    #[test]
    fn inline_can_be_switched_off() {
        let mut a = args();
        a.text = Some("x".into());
        let cfg = cfg_with(Sources {
            inline: false,
            ..Sources::default()
        });
        assert!(acquire_hostless(&a, &cfg).is_err());
    }

    #[test]
    fn data_uris_decode_and_carry_their_mime() {
        let mut a = args();
        a.uri = Some("data:text/csv;base64,YSxiCjEsMgo=".into());
        let got = acquire_hostless(&a, &ProfileConfig::default()).unwrap();
        assert_eq!(got.bytes, b"a,b\n1,2\n");
        assert_eq!(got.info.mimetype.as_deref(), Some("text/csv"));
    }

    #[test]
    fn a_plain_data_uri_needs_no_base64() {
        let mut a = args();
        a.uri = Some("data:text/plain,hello".into());
        assert_eq!(
            acquire_hostless(&a, &ProfileConfig::default())
                .unwrap()
                .bytes,
            b"hello"
        );
    }

    #[test]
    fn an_unknown_scheme_names_what_is_accepted() {
        let mut a = args();
        a.uri = Some("ftp://example.invalid/x".into());
        let e = acquire_hostless(&a, &ProfileConfig::default()).unwrap_err();
        assert!(format!("{e:?}").contains("mcpg-resource"), "{e:?}");
    }

    #[test]
    fn hints_reach_the_stream_info() {
        let mut a = args();
        a.text = Some("x".into());
        a.filename = Some("report.docx".into());
        a.mimetype = Some("text/plain; charset=utf-8".into());
        let got = acquire_hostless(&a, &ProfileConfig::default()).unwrap();
        assert_eq!(got.info.filename.as_deref(), Some("report.docx"));
        assert_eq!(got.info.extension.as_deref(), Some("docx"));
        assert_eq!(got.info.mimetype.as_deref(), Some("text/plain"));
    }

    #[test]
    fn unknown_argument_fields_are_rejected() {
        let e = serde_json::from_str::<ConvertArgs>(r#"{"path":"/etc/passwd"}"#).unwrap_err();
        assert!(format!("{e}").contains("unknown field"), "{e}");
    }

    #[test]
    fn a_resource_uri_reads_from_the_content_store() {
        let store = StubStore(vec![(
            "mcpg-resource://hash:abc".to_owned(),
            b"a,b\n1,2\n".to_vec(),
        )]);
        let mut a = args();
        a.uri = Some("mcpg-resource://hash:abc".into());
        let got = acquire(&a, &ProfileConfig::default(), &store).unwrap();
        assert_eq!(got.bytes, b"a,b\n1,2\n");
        assert_eq!(got.mode, "resource");
        // The URI is threaded through so image enrichment can fetch it back.
        assert_eq!(got.info.url.as_deref(), Some("mcpg-resource://hash:abc"));
    }

    #[test]
    fn an_unresolvable_resource_explains_why_it_might_be_gone() {
        let mut a = args();
        a.uri = Some("mcpg-resource://hash:gone".into());
        let e = acquire(&a, &ProfileConfig::default(), &StubStore(Vec::new())).unwrap_err();
        assert!(format!("{e:?}").contains("expired"), "{e:?}");
    }

    #[test]
    fn a_store_outage_reads_differently_from_a_missing_resource() {
        // `BackendError` has no request-versus-upstream split, so the
        // distinction has to live in the message — a caller retrying on the
        // wrong one wastes the retry.
        let mut a = args();
        a.uri = Some("mcpg-resource://hash:abc".into());
        let e = acquire(&a, &ProfileConfig::default(), &FailingStore).unwrap_err();
        let text = format!("{e:?}");
        assert!(text.contains("content store"), "{text}");
        assert!(!text.contains("expired"), "{text}");
    }

    #[test]
    fn the_resource_source_can_be_switched_off() {
        let mut a = args();
        a.uri = Some("mcpg-resource://hash:abc".into());
        let cfg = cfg_with(Sources {
            resource: false,
            ..Sources::default()
        });
        assert!(acquire(&a, &cfg, &StubStore(Vec::new())).is_err());
    }
}
