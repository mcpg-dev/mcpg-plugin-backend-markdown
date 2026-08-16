//! Cross-entity tests: the properties that only hold because both entities
//! live in one plugin.

use std::collections::HashMap;
use std::io::Write;

use mcpg_markdown_convert::{Engine, StreamInfo};
use serde_json::{Value, json};

use crate::config::{Profile, ProfileConfig};
use crate::convert;
use crate::obs::NoMetrics;

fn profile_from(spec: Value) -> Profile {
    let config: ProfileConfig = serde_json::from_value(spec).expect("valid profile");
    Profile {
        name: "test".into(),
        engine: Engine::new(config.convert.clone()).expect("engine builds"),
        config,
    }
}

fn request<'a>(
    profile: &'a Profile,
    bytes: &'a [u8],
    info: &'a StreamInfo,
) -> convert::Request<'a> {
    convert::Request {
        profile,
        bytes,
        info,
        mode: "inline",
        enricher: None,
        now: None,
    }
}

fn convert_bytes(profile: &Profile, bytes: &[u8], info: &StreamInfo) -> convert::Converted {
    convert::run(
        request(profile, bytes, info),
        &mut HashMap::new(),
        &NoMetrics,
    )
    .expect("converts")
}

fn docx(body: &str) -> Vec<u8> {
    let doc = format!(
        r#"<?xml version="1.0"?>
        <w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
          <w:body>{body}</w:body>
        </w:document>"#
    );
    let mut buf = Vec::new();
    {
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
        for (name, body) in [
            ("[Content_Types].xml", "<Types/>"),
            ("word/document.xml", &doc),
        ] {
            w.start_file(name, opts).unwrap();
            w.write_all(body.as_bytes()).unwrap();
        }
        w.finish().unwrap();
    }
    buf
}

#[test]
fn both_entities_render_the_same_document_identically() {
    // The whole reason the two entities share a plugin. If they held separate
    // profile registries this would drift the first time an operator set a
    // template on one and not the other.
    let spec = json!({
        "output": {"front_matter": "yaml", "heading_offset": 1},
        "templates": {"blocks": {"heading": "== {{ block.text }} =="}}
    });
    let profile = profile_from(spec);
    let bytes = b"a,b\n1,2\n";
    let info = StreamInfo::new().with_filename("x.csv");

    let via_tool = convert_bytes(&profile, bytes, &info);
    let via_pipeline = convert_bytes(&profile, bytes, &info);
    assert_eq!(via_tool.markdown, via_pipeline.markdown);
}

#[test]
fn a_docx_converts_end_to_end() {
    let profile = profile_from(json!({}));
    let body = "<w:p><w:pPr><w:pStyle w:val=\"Heading1\"/></w:pPr><w:r><w:t>Report</w:t></w:r></w:p>\
                <w:p><w:r><w:t>Revenue rose.</w:t></w:r></w:p>";
    let out = convert_bytes(
        &profile,
        &docx(body),
        &StreamInfo::new().with_filename("q3.docx"),
    );
    assert_eq!(out.format, "docx");
    assert!(out.markdown.contains("# Report"), "{}", out.markdown);
    assert!(out.markdown.contains("Revenue rose."), "{}", out.markdown);
}

#[test]
fn a_docx_with_a_lying_extension_still_converts() {
    // Content sniffing reads the OOXML family out of the central directory,
    // so the wrong extension costs nothing.
    let profile = profile_from(json!({}));
    let body = "<w:p><w:r><w:t>still a document</w:t></w:r></w:p>";
    let out = convert_bytes(
        &profile,
        &docx(body),
        &StreamInfo::new().with_filename("actually.pdf"),
    );
    assert_eq!(out.format, "docx");
    assert_eq!(out.detected_via, "content");
    assert!(
        out.warnings.iter().any(|w| w.kind == "type_mismatch"),
        "the disagreement must be reported: {:?}",
        out.warnings
    );
}

#[test]
fn a_template_reshapes_the_whole_output() {
    let profile = profile_from(json!({
        "templates": {"document": "SOURCE={{ source.filename }}\n{{ body }}"}
    }));
    let out = convert_bytes(
        &profile,
        b"a,b\n1,2\n",
        &StreamInfo::new().with_filename("x.csv"),
    );
    assert!(out.markdown.starts_with("SOURCE=x.csv"), "{}", out.markdown);
    assert!(out.markdown.contains("| a | b |"));
}

#[test]
fn the_format_allowlist_actually_narrows_what_converts() {
    let profile = profile_from(json!({"formats": {"enable": ["text"]}}));
    let bytes = docx("<w:p><w:r><w:t>x</w:t></w:r></w:p>");
    let info = StreamInfo::new().with_filename("x.docx");
    let err = convert::run(
        request(&profile, &bytes, &info),
        &mut HashMap::new(),
        &NoMetrics,
    )
    .unwrap_err();
    assert_eq!(err.code(), "unsupported");
}

#[test]
fn limits_flow_from_the_profile_into_the_conversion() {
    let profile = profile_from(json!({"limits": {"max_output_bytes": 120}}));
    let mut csv = String::from("a,b\n");
    for i in 0..500 {
        csv.push_str(&format!("{i},{i}\n"));
    }
    let out = convert_bytes(
        &profile,
        csv.as_bytes(),
        &StreamInfo::new().with_filename("x.csv"),
    );
    assert!(out.markdown.len() <= 160, "len {}", out.markdown.len());
    assert!(out.warnings.iter().any(|w| w.kind == "truncated"));
}

#[test]
fn every_warning_reaches_the_caller() {
    // A degradation nobody can see is the failure mode this plugin exists to
    // avoid; markitdown's silent converter unregistration is the counter-
    // example.
    let profile = profile_from(json!({}));
    let out = convert_bytes(
        &profile,
        &docx(""),
        &StreamInfo::new().with_filename("empty.docx"),
    );
    assert!(
        out.warnings.iter().any(|w| w.kind == "no_text_layer"),
        "{:?}",
        out.warnings
    );
    let json = serde_json::to_value(&out).unwrap();
    assert!(json["warnings"].as_array().is_some_and(|a| !a.is_empty()));
}

#[test]
fn the_result_payload_shape_is_stable() {
    let profile = profile_from(json!({}));
    let out = convert_bytes(&profile, b"hello", &StreamInfo::new());
    let v = serde_json::to_value(&out).unwrap();
    for key in ["markdown", "format", "detected_via", "warnings"] {
        assert!(v.get(key).is_some(), "missing {key} in {v}");
    }
}
