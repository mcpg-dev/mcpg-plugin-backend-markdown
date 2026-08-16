# Markdown Conversion — `dev.mcpg.backend.markdown`

> class `backend` (+ a `transform` entity) · `native` · package `mcpg-plugin-backend-markdown` · artifact `libmcpg_plugin_backend_markdown.so` · Apache-2.0

Turns documents into LLM-friendly Markdown: Office files (DOCX, PPTX, XLSX,
legacy XLS), PDF, HTML, EPUB, Outlook `.msg`, ZIP archives, images, audio,
CSV/TSV, JSON/NDJSON, XML, RSS/Atom and Jupyter notebooks. Structure survives —
headings stay headings, tables stay tables — at a fraction of the tokens the
original encoding costs.

Written entirely in Rust with no C dependency, so it cross-compiles to the same
matrix as the rest of the gateway (glibc/musl × x86_64/aarch64, darwin,
windows-gnu). The conversion engine lives in the `mcpg-markdown-convert` crate;
this one is the plugin shell around it.

## Two entities, one plugin

| Entity | Invoked by | Input | Does I/O | Host handle |
|---|---|---|---|---|
| `backend` | the **model**, as a tool call | acquires the bytes | yes | yes |
| `transform` (`markdown`) | the **gateway**, on a payload already in flight | what is already in the JSON | never | no |

Neither substitutes for the other. Backend-only would force a pipeline that
already holds a `.docx` to round-trip it out through the model's context to
convert it; transform-only would mean the model can never *ask* for a
conversion. They share one profile registry, so a document converted through
the tool and the same document converted in a pipeline render identically —
which is why they are one `.so` rather than two plugins.

## What it does
- Picks a converter from a prioritised guess ladder — magic bytes, then
  extension, then declared MIME — and tries each guess against each converter,
  so a mislabelled file still converts and the disagreement is reported.
- Parses into a small document IR, then renders CommonMark + GFM tables, or
  runs the IR through operator MiniJinja templates.
- Reports every degradation. Truncation, a skipped archive member, a PDF page
  with no text layer, a low-confidence structural guess — each lands in
  `warnings` on the result and in `mcpg_markdown_warnings_total`.
- Accepts bytes inline (base64 or text), from the gateway content store
  (`mcpg-resource://`), or over HTTPS when the operator opts in.
- Optionally enriches with a model: image captions, OCR, audio transcription,
  dispatched through the host to an LLM binding the operator already
  configured.
- Bounds every unbounded loop: input size, expansion (the zip-bomb ceiling),
  nesting depth, member count, table rows, output size and wall clock.
- Never resolves XML external entities, so XXE and billion-laughs are closed
  across every XML-derived format (OOXML, EPUB, feeds).

## What it will not do
- **Read the local filesystem.** No `filesystem_read` capability, no path
  argument, and `file:` URIs are refused with an explanation. A gateway that
  can read a caller-named path is a file-exfiltration primitive, because the
  model chooses the path. Local files reach the converter through a backend
  that already has audited access to them (`sftp`, `smb`, `s3`) as an
  `mcpg-resource://` URI.
- **Fetch a URL found inside a document.** An `<img src>` in converted HTML
  renders as a link and is never requested. The opt-in `url` source fetches
  what the *caller* named; a document asking on the caller's behalf is a
  different thing.
- **Hold a provider credential.** Enrichment goes through the host to a
  configured LLM binding. The key stays there, along with budgets, retries,
  caching and cost accounting.

## Configuration

```yaml
plugins:
  - id: dev.mcpg.backend.markdown
    source: { oci: ghcr.io/mcpg-dev/source-code/plugins/backend-markdown:protocol-1 }

mcp:
  capabilities:
    tools:
      - name: convert_to_markdown
        backend:
          kind: markdown
          # Everything below is the profile. A binding may instead say
          # `profile: reports` to share one defined on another binding.
          limits:
            max_input_bytes: 20Mi
            max_output_bytes: 4Mi
            max_expanded_bytes: 200Mi   # zip-bomb ceiling
            max_depth: 3                # nested archives / attachments
            max_embedded_documents: 64
            max_table_rows: 5000
            timeout_ms: 30000
          output:
            front_matter: yaml          # none | yaml | toml
            tables: gfm                 # gfm | html | csv
            heading_offset: 0
            preserve_unsupported_html: false
          formats:
            # Explicit allowlist. Omit the key for every converter in the
            # build; naming them means a format added by a future version
            # arrives as your decision rather than as a surprise.
            enable: [text, csv, json, ipynb, xml, feed, html,
                     docx, pptx, spreadsheet, epub, zip, pdf,
                     image, audio, msg]
```

### Sources

```yaml
          sources:
            inline: true      # `content` (base64) or `text` argument
            resource: true    # mcpg-resource:// through the content store
            url: false        # https:// — opt-in, needs network_outbound
          url:
            allow_private_addresses: false
            allow_hosts: [docs.example.com]
            max_redirects: 3
            timeout_ms: 20000
```

`resource` is the intended production path: a `backend.sftp` tool stores the
file and hands back an `mcpg-resource://` URI, so the bytes never travel
through the model's context as base64.

With `url` enabled, every redirect hop is re-resolved and address-checked, so a
redirect to `169.254.169.254` is refused even when the first hop was public.

### Templates

```yaml
          templates:
            document: |
              ---
              title: {{ doc.title | default(source.filename) }}
              source: {{ source.uri }}
              ---
              {% for w in doc.warnings %}> ⚠ {{ w.message }}
              {% endfor %}
              {{ body }}
            blocks:
              table: |
                {% if block.caption %}**{{ block.caption }}**{% endif %}
                {{ gfm_table(block) }}
```

Templates see a simplified projection: `block.text` is a string,
`block.rows` a list of lists of strings. `body` holds the default rendering, so
a template that only wants to add a header need not reimplement the renderer,
and `gfm_table(t)` renders one table the built-in way. Block overrides are
applied *before* `body` is built, so setting both works. Templates compile at
boot; a template that will not parse is a startup error.

### LLM enrichment

```yaml
          llm:
            binding: vision             # an existing LLM-backed tool
            enrich:
              images: caption           # off | caption | ocr | caption_and_ocr
              audio: transcribe         # off | transcribe
              pdf: ocr                  # off | ocr — scanned pages
            max_calls_per_document: 8
            cache: true
            image_prompt: "…"           # optional overrides
            audio_prompt: "…"
            pdf_prompt: "…"
```

Fail-soft by construction: a failure, a missing binding or an exhausted call
budget leaves the document as the converter produced it plus a warning. A
conversion never fails because a model was unavailable. Captions are cached by
content hash, so the same image described twice costs one call.

`ocr` is a vision-model call, not Tesseract — no pure-Rust OCR engine belongs
on a request path.

### Scanned PDFs

`enrich.pdf: ocr` handles the case a text-only extractor cannot: a PDF whose
pages are images. The converter records which pages carried no text layer;
when any did, the document is sent to the vision model and what it reads back
is appended under a heading that says it was transcribed. That labelling is
deliberate — a reader has to be able to tell a model's reading of a page from
text the document actually carried.

The **whole document** goes to the model, not the scanned pages alone.
Rasterising a single page needs a PDF renderer this plugin does not carry, and
the providers accept a PDF as a document part, so the model does that work on
its side. The cost is that a 200-page report with one scanned insert is sent
whole. The page list is recorded in the document metadata, so a future
page-level path has what it needs; today it is one call per document, bounded
by `max_calls_per_document` like every other enrichment.

A document that arrived inline is staged in the content store for five
minutes so the model can read it, then left to expire. Nothing is retained.

### As a pipeline step

```yaml
mcp:
  pipelines:
    - name: ingest
      steps:
        - backend: { name: fetch_report }        # returns base64 in /result/file
        - plugin_transform:
            plugin: dev.mcpg.backend.markdown:markdown
            config:
              profile: default
              pointer: /result/file
              filename: report.docx
```

The transform entity accepts `profile`, `pointer`, `phase`
(`arguments`/`result`/`both`), `encoding` (`auto`/`base64`/`text`),
`filename`, `mimetype` and `verbose`. It reads the same profiles the backend
registers; `profile` may be omitted when exactly one exists.

## Result shape

```json
{
  "markdown": "# Q3 report\n\n| region | eur |\n| --- | --- |\n…",
  "title": "Q3 report",
  "format": "docx",
  "detected_via": "content",
  "warnings": [
    { "kind": "truncated", "message": "table truncated to 5000 of 82190 rows" }
  ]
}
```

`warnings` is always present. An empty array means the conversion lost
nothing; that is a different statement from the field being absent.

## Observability

```
mcpg_markdown_conversions_total{format,source,detected_via,outcome[,error]}
mcpg_markdown_duration_seconds{format}
mcpg_markdown_input_bytes{format}
mcpg_markdown_output_bytes{format}
mcpg_markdown_warnings_total{format,kind}
mcpg_markdown_enrichment_calls_total{outcome}
mcpg_markdown_parser_panics_total{format}
```

Every label value comes from a closed set, so no filename or URL can inflate
cardinality. `mcpg_markdown_parser_panics_total` should be flat at zero —
alert on any non-zero value rather than on a rate change.

## Capabilities

Declares `network_outbound`, and nothing else. It is used only by the opt-in
`url` source, which is off by default, but the plugin *can* open a socket and
under-declaring would let it pass as one that cannot.

No `filesystem_read`, no `filesystem_write`, no `secrets_read`.

## Known limits

- **PDF is text-only.** `pdf-extract` gives content-stream order: no
  multi-column reading order, no table reconstruction, no OCR. Headings are
  inferred from line shape and every such inference raises a
  `heuristic_applied` warning. pdfminer.six is better at this; no pure-Rust
  stack matches it today.
- **Images and audio convert to metadata** unless enrichment is on. Both say
  so with a `degraded` warning rather than returning a thin document silently.
- **Scanned PDFs need `enrich.pdf: ocr`.** Without it the pages are detected
  and warned about, not read.
- **Outlook HTML/RTF bodies are not converted** — only the plain-text
  alternative, with a warning when it is absent.
- **Attachments inside `.msg` are named, not converted.**

## Testing

```sh
./dev test mcpg-plugin-backend-markdown
./dev test mcpg-markdown-convert
```

The engine's suite covers each converter, the escaping invariants, and a
hostile corpus — zip bombs, XXE, billion laughs, deep nesting, lying size
headers, truncated containers — asserting a clean error and a bounded process
in every case. The plugin's suite covers acquisition, the address guard,
enrichment fail-soft behaviour, and that both entities render identically.
