//! LLM enrichment — captions, OCR and transcripts.
//!
//! markitdown takes an `llm_client` and calls a provider directly. This
//! plugin does not, and the difference is the point: enrichment is dispatched
//! **through the host** to an LLM binding the operator already configured.
//!
//! - The converter never holds a provider credential. The key stays in the
//!   LLM binding's own config and resolves through the gateway's secret
//!   machinery.
//! - Budgets, retries, guardrails, response caching and cost accounting come
//!   from that binding's engine rather than being reimplemented here.
//! - Any provider works — including a local OpenAI-compatible endpoint — with
//!   no code path of its own.
//! - Every call appears in the audit log as an ordinary child dispatch.
//!
//! Enrichment is **strictly additive and strictly fail-soft**. A failure, a
//! missing binding or an exhausted call budget leaves the document exactly as
//! the converter produced it, plus a warning. A conversion never fails
//! because a model was unavailable.

use std::collections::HashMap;

use mcpg_markdown_convert::{Block, Document, ImageRef, Inline, Warning, WarningKind};
use serde_json::{Value, json};

use crate::config::{AudioEnrichment, ImageEnrichment, LlmOptions};

/// What the plugin needs from the host to enrich. A trait so the whole pass
/// is testable without a gateway.
pub trait Enricher {
    /// Dispatch a prompt, optionally about a stored resource. Returns the
    /// model's text.
    fn describe(&self, prompt: &str, resource_uri: Option<&str>) -> Result<String, String>;

    /// Put bytes in the gateway content store and return their
    /// `mcpg-resource://` URI.
    ///
    /// Needed because a document that arrived inline has no URI, and the LLM
    /// bindings resolve a resource rather than accepting raw bytes from us —
    /// which is the right split, since it keeps one copy of the blob in one
    /// place instead of threading it through every dispatch.
    fn store(&self, bytes: &[u8], mime: &str) -> Result<String, String>;
}

const DEFAULT_IMAGE_PROMPT: &str = "Describe this image for a reader who cannot see it. Be specific and factual. \
     Two sentences at most. Do not speculate about anything not visible.";

const DEFAULT_OCR_PROMPT: &str = "Transcribe all text visible in this image, preserving reading order. \
     Output only the transcribed text. If there is no text, output nothing.";

const DEFAULT_AUDIO_PROMPT: &str = "Transcribe this audio. Output only the transcript.";

const DEFAULT_PDF_OCR_PROMPT: &str = "This PDF has pages with no text layer — they are scanned images. \
     Transcribe their text as Markdown, preserving headings, lists and tables. \
     Output only the transcription, with no commentary. If a page is \
     illegible, write `[illegible]` for it rather than guessing.";

/// Outcome of one enrichment pass.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct EnrichReport {
    pub attempted: u32,
    pub succeeded: u32,
    pub failed: u32,
    pub cached: u32,
    pub skipped_over_budget: u32,
}

impl EnrichReport {
    #[must_use]
    pub fn ran(&self) -> bool {
        self.attempted > 0
    }
}

/// The source document, for passes that need the original bytes rather than
/// something already in the content store.
#[derive(Clone, Copy)]
pub struct Source<'a> {
    pub bytes: &'a [u8],
    pub mime: &'a str,
    /// Set when the bytes already live in the content store, so a second
    /// copy is not stored.
    pub uri: Option<&'a str>,
}

/// Enrich `doc` in place. Returns what happened, for metrics.
pub fn enrich(
    doc: &mut Document,
    opts: &LlmOptions,
    enricher: &dyn Enricher,
    cache: &mut HashMap<String, String>,
    source: Option<Source<'_>>,
) -> EnrichReport {
    let mut report = EnrichReport::default();
    if opts.binding.is_none() {
        return report;
    }
    let images_on = opts.enrich.images.is_on();
    let audio_on = opts.enrich.audio.is_on();
    let pdf_on = opts.enrich.pdf.is_on();
    if !images_on && !audio_on && !pdf_on {
        return report;
    }

    let mut warnings: Vec<Warning> = Vec::new();
    if images_on {
        enrich_images(
            &mut doc.blocks,
            opts,
            enricher,
            cache,
            &mut report,
            &mut warnings,
        );
    }
    if audio_on {
        enrich_audio(doc, opts, enricher, cache, &mut report, &mut warnings);
    }
    if pdf_on && let Some(src) = source {
        enrich_pdf(doc, opts, enricher, cache, &mut report, &mut warnings, src);
    }

    for w in warnings {
        doc.warnings.push(w);
    }
    report
}

/// Read a scanned PDF with the vision model.
///
/// The whole document goes to the model rather than page images, because
/// rasterising a page needs a renderer this crate does not have — and the
/// providers already accept a PDF as a document part, so the model does the
/// rasterisation on its side. The cost is that a document with one scanned
/// insert is sent whole; the page list in the metadata is what a future
/// page-level path would use to do better.
fn enrich_pdf(
    doc: &mut Document,
    opts: &LlmOptions,
    enricher: &dyn Enricher,
    cache: &mut HashMap<String, String>,
    report: &mut EnrichReport,
    warnings: &mut Vec<Warning>,
    source: Source<'_>,
) {
    use mcpg_markdown_convert::converters::pdf::{PAGE_COUNT_KEY, SCANNED_PAGES_KEY};

    // The converter records which pages had no text. No key means either a
    // text PDF or a different format, and either way there is nothing to OCR.
    let Some(scanned) = doc.metadata.get(SCANNED_PAGES_KEY).map(str::to_owned) else {
        return;
    };
    if scanned.is_empty() {
        return;
    }
    if report.attempted >= opts.max_calls_per_document {
        report.skipped_over_budget += 1;
        return;
    }

    // Resolve to something the model can read. An inline upload has no URI
    // yet, so it is stored once here.
    let uri = match source.uri {
        Some(u) => u.to_owned(),
        None => match enricher.store(source.bytes, source.mime) {
            Ok(u) => u,
            Err(e) => {
                report.failed += 1;
                warnings.push(Warning::new(
                    WarningKind::EnrichmentFailed,
                    format!("could not store the PDF for OCR: {e}"),
                ));
                return;
            }
        },
    };

    let prompt = opts.pdf_prompt.as_deref().unwrap_or(DEFAULT_PDF_OCR_PROMPT);
    let Some(text) = call(prompt, Some(&uri), enricher, cache, opts, report, warnings) else {
        return;
    };
    let text = text.trim();
    if text.is_empty() {
        warnings.push(Warning::new(
            WarningKind::EnrichmentFailed,
            "OCR returned nothing for the scanned pages",
        ));
        return;
    }

    let pages = doc.metadata.get(PAGE_COUNT_KEY).unwrap_or("?").to_owned();
    // A heading rather than a silent splice: a reader must be able to tell
    // model-transcribed text from text the document actually carried.
    doc.push(Block::Heading {
        level: 2,
        text: Inline::text(format!("Scanned pages {scanned} of {pages} (transcribed)")),
    });
    doc.push(Block::Raw {
        markdown: text.to_owned(),
    });
    doc.warnings.push(Warning::new(
        WarningKind::Degraded,
        format!(
            "pages {scanned} carried no text layer and were transcribed by a model; \
             treat that section as a reading, not as the document's own text"
        ),
    ));
}

fn enrich_images(
    blocks: &mut [Block],
    opts: &LlmOptions,
    enricher: &dyn Enricher,
    cache: &mut HashMap<String, String>,
    report: &mut EnrichReport,
    warnings: &mut Vec<Warning>,
) {
    for block in blocks.iter_mut() {
        match block {
            Block::Image(img) => {
                // Only a stored resource can be read by a model. A URL found
                // inside a document is never fetched — that would make the
                // converter a request-forgery primitive on the document's
                // behalf rather than the caller's.
                let ImageRef::Resource(uri) = &img.source else {
                    continue;
                };
                if img.caption.is_some() {
                    continue;
                }
                if report.attempted >= opts.max_calls_per_document {
                    report.skipped_over_budget += 1;
                    continue;
                }
                let uri = uri.clone();
                let mut parts: Vec<String> = Vec::new();
                if matches!(
                    opts.enrich.images,
                    ImageEnrichment::Caption | ImageEnrichment::CaptionAndOcr
                ) {
                    let prompt = opts.image_prompt.as_deref().unwrap_or(DEFAULT_IMAGE_PROMPT);
                    if let Some(t) =
                        call(prompt, Some(&uri), enricher, cache, opts, report, warnings)
                    {
                        parts.push(t);
                    }
                }
                if matches!(
                    opts.enrich.images,
                    ImageEnrichment::Ocr | ImageEnrichment::CaptionAndOcr
                ) && report.attempted < opts.max_calls_per_document
                    && let Some(t) = call(
                        DEFAULT_OCR_PROMPT,
                        Some(&uri),
                        enricher,
                        cache,
                        opts,
                        report,
                        warnings,
                    )
                    && !t.trim().is_empty()
                {
                    parts.push(format!("Text in image: {t}"));
                }
                if !parts.is_empty() {
                    img.caption = Some(parts.join("\n\n"));
                }
            }
            // Descend so an image inside an archive member or a quote is
            // reached too.
            Block::Quote(inner) => {
                enrich_images(inner, opts, enricher, cache, report, warnings);
            }
            Block::List { items, .. } => {
                for item in items.iter_mut() {
                    enrich_images(item, opts, enricher, cache, report, warnings);
                }
            }
            Block::Embedded { doc, .. } => {
                enrich_images(&mut doc.blocks, opts, enricher, cache, report, warnings);
            }
            _ => {}
        }
    }
}

/// Replace the audio converter's placeholder paragraph with a transcript.
fn enrich_audio(
    doc: &mut Document,
    opts: &LlmOptions,
    enricher: &dyn Enricher,
    cache: &mut HashMap<String, String>,
    report: &mut EnrichReport,
    warnings: &mut Vec<Warning>,
) {
    // The converter has no host, so it leaves a marker rather than a
    // transcript. Nothing else in the pipeline produces this string.
    const PLACEHOLDER: &str = "[audio: no transcript]";

    let Some(uri) = doc
        .metadata
        .get("source_uri")
        .map(str::to_owned)
        .or_else(|| doc.metadata.get("resource_uri").map(str::to_owned))
    else {
        return;
    };

    let has_placeholder = doc.blocks.iter().any(|b| match b {
        Block::Paragraph(i) => i.to_plain() == PLACEHOLDER,
        _ => false,
    });
    if !has_placeholder {
        return;
    }
    if report.attempted >= opts.max_calls_per_document {
        report.skipped_over_budget += 1;
        return;
    }
    if opts.enrich.audio != AudioEnrichment::Transcribe {
        return;
    }

    let prompt = opts.audio_prompt.as_deref().unwrap_or(DEFAULT_AUDIO_PROMPT);
    let Some(text) = call(prompt, Some(&uri), enricher, cache, opts, report, warnings) else {
        return;
    };
    for b in doc.blocks.iter_mut() {
        if let Block::Paragraph(i) = b
            && i.to_plain() == PLACEHOLDER
        {
            *b = Block::Paragraph(Inline::text(text.trim()));
            break;
        }
    }
}

/// One dispatch, with the cache in front of it. `None` on any failure, with a
/// warning recorded — never an error, because enrichment must not be able to
/// fail a conversion.
fn call(
    prompt: &str,
    resource: Option<&str>,
    enricher: &dyn Enricher,
    cache: &mut HashMap<String, String>,
    opts: &LlmOptions,
    report: &mut EnrichReport,
    warnings: &mut Vec<Warning>,
) -> Option<String> {
    let key = cache_key(prompt, resource);
    if opts.cache
        && let Some(hit) = cache.get(&key)
    {
        report.cached += 1;
        return Some(hit.clone());
    }

    report.attempted += 1;
    match enricher.describe(prompt, resource) {
        Ok(text) => {
            report.succeeded += 1;
            if opts.cache {
                cache.insert(key, text.clone());
            }
            Some(text)
        }
        Err(e) => {
            report.failed += 1;
            warnings.push(Warning::new(
                WarningKind::EnrichmentFailed,
                format!(
                    "enrichment for {} failed: {e}",
                    resource.unwrap_or("the document")
                ),
            ));
            None
        }
    }
}

/// Content-addressed: the same bytes and the same prompt yield the same
/// caption, which is what makes a cache correct here rather than merely
/// convenient. The resource URI already carries a BLAKE3 of the content.
fn cache_key(prompt: &str, resource: Option<&str>) -> String {
    let mut h = blake3::Hasher::new();
    h.update(prompt.as_bytes());
    h.update(b"\0");
    h.update(resource.unwrap_or_default().as_bytes());
    h.finalize().to_hex().to_string()
}

/// Build the arguments for a child tool dispatch to an LLM binding.
///
/// A resource URI is passed through rather than the bytes: the LLM binding's
/// multimodal resolver already knows how to inline an `mcpg-resource://` from
/// the content store, and routing the bytes through here would double the
/// memory for no gain.
#[must_use]
pub fn dispatch_arguments(prompt: &str, resource: Option<&str>) -> Value {
    match resource {
        Some(uri) => json!({
            "messages": [{
                "role": "user",
                "content": [
                    { "type": "resource", "uri": uri },
                    { "type": "text", "text": prompt },
                ],
            }],
        }),
        None => json!({
            "messages": [{ "role": "user", "content": prompt }],
        }),
    }
}

/// Pull the text out of whatever shape the LLM binding returned.
#[must_use]
pub fn extract_text(value: &Value) -> Option<String> {
    for pointer in [
        "/content/0/text",
        "/message/content",
        "/choices/0/message/content",
        "/text",
        "/output",
    ] {
        if let Some(s) = value.pointer(pointer).and_then(Value::as_str)
            && !s.trim().is_empty()
        {
            return Some(s.to_owned());
        }
    }
    if let Some(s) = value.as_str()
        && !s.trim().is_empty()
    {
        return Some(s.to_owned());
    }
    None
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::config::{EnrichOptions, PdfEnrichment};
    use mcpg_markdown_convert::{Image, Metadata};

    struct Recording {
        replies: RefCell<Vec<Result<String, String>>>,
        calls: RefCell<Vec<(String, Option<String>)>>,
        /// (byte length, mime) per `store` call.
        stored: RefCell<Vec<(usize, String)>>,
    }

    impl Recording {
        fn ok(n: usize, text: &str) -> Self {
            Self {
                replies: RefCell::new((0..n).map(|_| Ok(text.to_owned())).collect()),
                calls: RefCell::new(Vec::new()),
                stored: RefCell::new(Vec::new()),
            }
        }
        fn failing() -> Self {
            Self {
                replies: RefCell::new(Vec::new()),
                calls: RefCell::new(Vec::new()),
                stored: RefCell::new(Vec::new()),
            }
        }
    }

    impl Enricher for Recording {
        fn describe(&self, prompt: &str, resource: Option<&str>) -> Result<String, String> {
            self.calls
                .borrow_mut()
                .push((prompt.to_owned(), resource.map(str::to_owned)));
            self.replies
                .borrow_mut()
                .pop()
                .unwrap_or_else(|| Err("no model available".to_owned()))
        }

        fn store(&self, bytes: &[u8], mime: &str) -> Result<String, String> {
            self.stored
                .borrow_mut()
                .push((bytes.len(), mime.to_owned()));
            Ok("mcpg-resource://hash:stored".to_owned())
        }
    }

    fn opts(images: ImageEnrichment) -> LlmOptions {
        LlmOptions {
            binding: Some("vision".into()),
            enrich: EnrichOptions {
                images,
                audio: AudioEnrichment::Off,
                pdf: PdfEnrichment::Off,
            },
            max_calls_per_document: 8,
            cache: true,
            image_prompt: None,
            audio_prompt: None,
            pdf_prompt: None,
        }
    }

    fn doc_with_image(source: ImageRef) -> Document {
        Document {
            blocks: vec![Block::Image(Image {
                alt: Some("chart".into()),
                caption: None,
                source,
            })],
            ..Document::default()
        }
    }

    fn caption(doc: &Document) -> Option<String> {
        doc.blocks.iter().find_map(|b| match b {
            Block::Image(i) => i.caption.clone(),
            _ => None,
        })
    }

    #[test]
    fn a_stored_resource_gets_a_caption() {
        let mut doc = doc_with_image(ImageRef::Resource("mcpg-resource://hash:a".into()));
        let e = Recording::ok(1, "A bar chart.");
        let r = enrich(
            &mut doc,
            &opts(ImageEnrichment::Caption),
            &e,
            &mut HashMap::new(),
            None,
        );
        assert_eq!(caption(&doc).as_deref(), Some("A bar chart."));
        assert_eq!(r.succeeded, 1);
    }

    #[test]
    fn a_url_inside_a_document_is_never_sent_anywhere() {
        // Following a document-supplied URL would make the converter fetch on
        // the document's behalf. It must not, enrichment or not.
        let mut doc = doc_with_image(ImageRef::Url("http://169.254.169.254/x.png".into()));
        let e = Recording::ok(1, "should not happen");
        let r = enrich(
            &mut doc,
            &opts(ImageEnrichment::Caption),
            &e,
            &mut HashMap::new(),
            None,
        );
        assert_eq!(r.attempted, 0);
        assert!(e.calls.borrow().is_empty());
        assert!(caption(&doc).is_none());
    }

    #[test]
    fn enrichment_is_off_without_a_binding() {
        let mut doc = doc_with_image(ImageRef::Resource("mcpg-resource://hash:a".into()));
        let mut o = opts(ImageEnrichment::Caption);
        o.binding = None;
        let e = Recording::ok(1, "x");
        assert_eq!(
            enrich(&mut doc, &o, &e, &mut HashMap::new(), None).attempted,
            0
        );
    }

    #[test]
    fn a_failure_degrades_the_document_rather_than_the_call() {
        let mut doc = doc_with_image(ImageRef::Resource("mcpg-resource://hash:a".into()));
        let e = Recording::failing();
        let r = enrich(
            &mut doc,
            &opts(ImageEnrichment::Caption),
            &e,
            &mut HashMap::new(),
            None,
        );
        assert_eq!(r.failed, 1);
        assert!(caption(&doc).is_none());
        assert!(
            doc.warnings
                .iter()
                .any(|w| w.kind == WarningKind::EnrichmentFailed),
            "{:?}",
            doc.warnings
        );
    }

    #[test]
    fn the_call_budget_is_enforced() {
        let mut doc = Document {
            blocks: (0..5)
                .map(|i| {
                    Block::Image(Image {
                        alt: None,
                        caption: None,
                        source: ImageRef::Resource(format!("mcpg-resource://hash:{i}")),
                    })
                })
                .collect(),
            ..Document::default()
        };
        let mut o = opts(ImageEnrichment::Caption);
        o.max_calls_per_document = 2;
        let e = Recording::ok(5, "cap");
        let r = enrich(&mut doc, &o, &e, &mut HashMap::new(), None);
        assert_eq!(r.attempted, 2);
        assert_eq!(r.skipped_over_budget, 3);
    }

    #[test]
    fn identical_images_hit_the_cache_rather_than_the_model() {
        let mut doc = Document {
            blocks: (0..3)
                .map(|_| {
                    Block::Image(Image {
                        alt: None,
                        caption: None,
                        source: ImageRef::Resource("mcpg-resource://hash:same".into()),
                    })
                })
                .collect(),
            ..Document::default()
        };
        let e = Recording::ok(3, "same picture");
        let r = enrich(
            &mut doc,
            &opts(ImageEnrichment::Caption),
            &e,
            &mut HashMap::new(),
            None,
        );
        assert_eq!(r.attempted, 1, "the same content was described twice");
        assert_eq!(r.cached, 2);
    }

    #[test]
    fn ocr_mode_asks_for_the_text_as_well() {
        let mut doc = doc_with_image(ImageRef::Resource("mcpg-resource://hash:a".into()));
        let e = Recording::ok(2, "INVOICE 42");
        enrich(
            &mut doc,
            &opts(ImageEnrichment::CaptionAndOcr),
            &e,
            &mut HashMap::new(),
            None,
        );
        assert!(caption(&doc).unwrap().contains("Text in image"));
    }

    #[test]
    fn images_nested_in_embedded_documents_are_reached() {
        let inner = Document {
            blocks: vec![Block::Image(Image {
                alt: None,
                caption: None,
                source: ImageRef::Resource("mcpg-resource://hash:deep".into()),
            })],
            ..Document::default()
        };
        let mut doc = Document {
            blocks: vec![Block::Embedded {
                name: "attachment".into(),
                doc: Box::new(inner),
            }],
            ..Document::default()
        };
        let e = Recording::ok(1, "found it");
        assert_eq!(
            enrich(
                &mut doc,
                &opts(ImageEnrichment::Caption),
                &e,
                &mut HashMap::new(),
                None
            )
            .succeeded,
            1
        );
    }

    #[test]
    fn an_existing_caption_is_not_overwritten() {
        let mut doc = Document {
            blocks: vec![Block::Image(Image {
                alt: None,
                caption: Some("already described".into()),
                source: ImageRef::Resource("mcpg-resource://hash:a".into()),
            })],
            ..Document::default()
        };
        let e = Recording::ok(1, "new");
        assert_eq!(
            enrich(
                &mut doc,
                &opts(ImageEnrichment::Caption),
                &e,
                &mut HashMap::new(),
                None
            )
            .attempted,
            0
        );
    }

    #[test]
    fn audio_placeholders_are_replaced_by_a_transcript() {
        let mut metadata = Metadata::default();
        metadata.set("source_uri", "mcpg-resource://hash:audio");
        let mut doc = Document {
            metadata,
            blocks: vec![Block::Paragraph(Inline::text("[audio: no transcript]"))],
            ..Document::default()
        };
        let mut o = opts(ImageEnrichment::Off);
        o.enrich.audio = AudioEnrichment::Transcribe;
        let e = Recording::ok(1, "hello world");
        enrich(&mut doc, &o, &e, &mut HashMap::new(), None);
        match &doc.blocks[0] {
            Block::Paragraph(i) => assert_eq!(i.to_plain(), "hello world"),
            other => panic!("{other:?}"),
        }
    }

    // --- scanned PDFs -----------------------------------------------------

    fn pdf_opts() -> LlmOptions {
        let mut o = opts(ImageEnrichment::Off);
        o.enrich.pdf = PdfEnrichment::Ocr;
        o
    }

    /// A document shaped the way the PDF converter leaves a scanned file.
    fn scanned_doc(scanned: &str, pages: &str) -> Document {
        let mut metadata = mcpg_markdown_convert::Metadata::default();
        metadata.set("pdf_scanned_pages", scanned);
        metadata.set("pdf_page_count", pages);
        Document {
            metadata,
            blocks: vec![],
            ..Document::default()
        }
    }

    fn source<'a>(bytes: &'a [u8], uri: Option<&'a str>) -> Source<'a> {
        Source {
            bytes,
            mime: "application/pdf",
            uri,
        }
    }

    #[test]
    fn a_scanned_pdf_is_read_and_the_result_is_labelled_as_transcribed() {
        let mut doc = scanned_doc("1,2", "2");
        let e = Recording::ok(1, "# Invoice\n\nTotal: 1200");
        let r = enrich(
            &mut doc,
            &pdf_opts(),
            &e,
            &mut HashMap::new(),
            Some(source(b"%PDF-1.4", Some("mcpg-resource://hash:pdf"))),
        );
        assert_eq!(r.succeeded, 1);

        let rendered = format!("{:?}", doc.blocks);
        assert!(rendered.contains("Invoice"), "{rendered}");
        // A reader must be able to tell a model's reading from the document's
        // own text; splicing it in unlabelled would be the worst outcome.
        assert!(rendered.contains("transcribed"), "{rendered}");
        assert!(
            doc.warnings
                .iter()
                .any(|w| w.message.contains("not as the document's own text")),
            "{:?}",
            doc.warnings
        );
    }

    #[test]
    fn an_inline_pdf_is_stored_once_so_the_model_can_read_it() {
        // A tool call that uploads bytes has nothing in the content store,
        // and the LLM bindings resolve a resource rather than taking bytes.
        let mut doc = scanned_doc("1", "1");
        let e = Recording::ok(1, "text");
        enrich(
            &mut doc,
            &pdf_opts(),
            &e,
            &mut HashMap::new(),
            Some(source(b"%PDF-1.4 inline", None)),
        );
        assert_eq!(e.stored.borrow().len(), 1);
        assert_eq!(e.stored.borrow()[0].1, "application/pdf");
        assert_eq!(
            e.calls.borrow()[0].1.as_deref(),
            Some("mcpg-resource://hash:stored")
        );
    }

    #[test]
    fn an_already_stored_pdf_is_not_stored_again() {
        let mut doc = scanned_doc("1", "1");
        let e = Recording::ok(1, "text");
        enrich(
            &mut doc,
            &pdf_opts(),
            &e,
            &mut HashMap::new(),
            Some(source(b"%PDF-1.4", Some("mcpg-resource://hash:already"))),
        );
        assert!(e.stored.borrow().is_empty(), "stored a second copy");
    }

    #[test]
    fn a_pdf_with_a_text_layer_is_not_sent_anywhere() {
        // No scanned-pages metadata means the converter read it fine. Paying
        // for a model call there would be pure waste.
        let mut doc = Document::default();
        let e = Recording::ok(1, "should not happen");
        let r = enrich(
            &mut doc,
            &pdf_opts(),
            &e,
            &mut HashMap::new(),
            Some(source(b"%PDF-1.4", Some("mcpg-resource://hash:pdf"))),
        );
        assert_eq!(r.attempted, 0);
        assert!(e.calls.borrow().is_empty());
    }

    #[test]
    fn ocr_stays_off_unless_asked_for() {
        let mut doc = scanned_doc("1", "1");
        let e = Recording::ok(1, "text");
        let r = enrich(
            &mut doc,
            &opts(ImageEnrichment::Caption),
            &e,
            &mut HashMap::new(),
            Some(source(b"%PDF-1.4", Some("mcpg-resource://hash:pdf"))),
        );
        assert_eq!(r.attempted, 0, "image captioning must not buy PDF OCR too");
    }

    #[test]
    fn a_failed_store_degrades_rather_than_failing_the_conversion() {
        struct NoStore;
        impl Enricher for NoStore {
            fn describe(&self, _p: &str, _r: Option<&str>) -> Result<String, String> {
                Ok("unused".to_owned())
            }
            fn store(&self, _b: &[u8], _m: &str) -> Result<String, String> {
                Err("content store unavailable".to_owned())
            }
        }
        let mut doc = scanned_doc("1", "1");
        let r = enrich(
            &mut doc,
            &pdf_opts(),
            &NoStore,
            &mut HashMap::new(),
            Some(source(b"%PDF-1.4", None)),
        );
        assert_eq!(r.failed, 1);
        assert!(
            doc.warnings
                .iter()
                .any(|w| w.kind == WarningKind::EnrichmentFailed),
            "{:?}",
            doc.warnings
        );
    }

    #[test]
    fn empty_ocr_output_warns_rather_than_appending_an_empty_section() {
        let mut doc = scanned_doc("1", "1");
        let e = Recording::ok(1, "   \n  ");
        enrich(
            &mut doc,
            &pdf_opts(),
            &e,
            &mut HashMap::new(),
            Some(source(b"%PDF-1.4", Some("mcpg-resource://hash:pdf"))),
        );
        assert!(doc.blocks.is_empty(), "{:?}", doc.blocks);
        assert!(
            doc.warnings
                .iter()
                .any(|w| w.kind == WarningKind::EnrichmentFailed)
        );
    }

    #[test]
    fn ocr_respects_the_per_document_call_budget() {
        let mut doc = scanned_doc("1", "1");
        let mut o = pdf_opts();
        o.max_calls_per_document = 0;
        let e = Recording::ok(1, "text");
        let r = enrich(
            &mut doc,
            &o,
            &e,
            &mut HashMap::new(),
            Some(source(b"%PDF-1.4", Some("mcpg-resource://hash:pdf"))),
        );
        assert_eq!(r.attempted, 0);
        assert_eq!(r.skipped_over_budget, 1);
    }

    #[test]
    fn dispatch_arguments_reference_the_resource_rather_than_inline_it() {
        let v = dispatch_arguments("describe", Some("mcpg-resource://hash:a"));
        let text = v.to_string();
        assert!(text.contains("mcpg-resource://hash:a"), "{text}");
        assert!(
            !text.contains("base64"),
            "bytes must not be inlined: {text}"
        );
    }

    #[test]
    fn text_is_extracted_from_the_common_response_shapes() {
        assert_eq!(
            extract_text(&json!({"content":[{"text":"a"}]})).as_deref(),
            Some("a")
        );
        assert_eq!(
            extract_text(&json!({"choices":[{"message":{"content":"b"}}]})).as_deref(),
            Some("b")
        );
        assert_eq!(extract_text(&json!("plain")).as_deref(), Some("plain"));
        assert_eq!(extract_text(&json!({"unrelated": 1})), None);
        assert_eq!(extract_text(&json!({"text": "   "})), None);
    }
}
