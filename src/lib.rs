//! # `dev.mcpg.backend.markdown`
//!
//! Any file → Markdown, for MCPG.
//!
//! One cdylib carrying **two entities** over one conversion engine:
//!
//! | Entity | Invoked by | Input | I/O | Host handle |
//! |---|---|---|---|---|
//! | `backend` | the **model**, as a tool | acquires bytes | yes | yes |
//! | `transform` | the **gateway**, on a payload in flight | what is already there | never | no |
//!
//! Neither covers the other. Backend-only would force a pipeline holding a
//! `.docx` to round-trip it out through the model's context to convert it;
//! transform-only would mean the model can never *ask* for a conversion. They
//! share one profile registry so both render the same document the same way —
//! which is why they are one plugin rather than two.
//!
//! ## What it will not do
//!
//! **No filesystem access, ever.** The plugin declares no `filesystem_read`
//! capability, offers no path argument, and rejects `file:` URIs with an
//! explanation. A gateway process that can read a caller-named path is a file
//! exfiltration primitive, because the model chooses the path. Local files
//! reach the converter through a backend that already has audited access to
//! them — `sftp`, `smb`, `s3` — as an `mcpg-resource://` URI.
//!
//! **No fetching of URLs found inside documents.** An `<img src>` in
//! converted HTML renders as a link and is never requested. The opt-in `url`
//! source mode fetches what the *caller* named, with every redirect hop
//! re-resolved and address-checked; a document asking on the caller's behalf
//! is a different thing and is refused.
//!
//! **No provider credentials.** Optional enrichment — image captioning, OCR,
//! audio transcription — is dispatched through the host to an LLM binding the
//! operator already configured. Budgets, retries, caching and audit come from
//! there. Enrichment is fail-soft: it degrades the document with a warning,
//! never the call.

#![forbid(unsafe_code)]

pub mod backend_entity;
pub mod cdylib;
pub mod config;
pub mod convert;
pub mod enrich;
pub mod fetch;
pub mod obs;
pub mod source;
pub mod state;
pub mod transform_entity;

pub use backend_entity::MarkdownBackend;
pub use config::{ProfileConfig, Sources};
pub use convert::Converted;
pub use transform_entity::MarkdownTransform;

#[cfg(test)]
mod tests;
