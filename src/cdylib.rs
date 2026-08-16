//! cdylib export — ONE `.so` carrying TWO entities.
//!
//! - `backend` ([`MarkdownBackend`]) — `convert_to_markdown` as an MCP tool.
//!   Holds the host handle: acquires bytes and dispatches enrichment.
//! - `transform` ([`MarkdownTransform`]) — the global / pipeline transform.
//!   Handed no host, so it can neither reach the network nor call a model.
//!
//! Both wrap the same [`crate::state::MarkdownState`], so the converter
//! registry, the compiled templates and the enrichment cache exist once, and
//! a document converted through the tool renders identically to the same
//! document converted in a pipeline. Two separate plugins would have to
//! duplicate that configuration and would drift apart silently.
//!
//! Sharing the object does not share the grant: capability posture is per
//! entity, and the transform entity's factory ignores the handle it is passed.

#[cfg(any(feature = "cdylib-export", feature = "static-firstparty"))]
use crate::backend_entity::MarkdownBackend;
#[cfg(any(feature = "cdylib-export", feature = "static-firstparty"))]
use crate::transform_entity::MarkdownTransform;

// Gated so a plain workspace build emits only the rlib — an ungated export
// risks a duplicate `mcpg_plugin_register` symbol across plugin crates.
#[cfg(any(feature = "cdylib-export", feature = "static-firstparty"))]
mcpg_plugin_sdk::declare_plugin! {
    plugin_id: "dev.mcpg.backend.markdown",
    plugin_version: env!("CARGO_PKG_VERSION"),
    descriptor_yaml: include_str!("../plugin.yaml"),
    // Declared because the plugin CAN open an outbound socket (the opt-in
    // `url` source). Unused unless a profile enables it; under-declaring
    // would let a plugin that can reach the network pass as one that cannot.
    capabilities: &[::mcpg_plugin_protocol::capability::Capability::NetworkOutbound],
    // This kind may appear as a backend pipeline step, so it must declare
    // `pipeline_capable`. Health is Skip: conversion is pure compute plus an
    // optional fetch, with no upstream to probe.
    backend_profile: ::mcpg_plugin_protocol::manifest::BackendProfile {
        pipeline_capable: true,
        ..::core::default::Default::default()
    },
    entities: [
        backend as binding {
            inner_name: "",
            plugin_type: MarkdownBackend,
            factory: |_cfg: &str, host: ::mcpg_plugin_sdk::HostHandle| MarkdownBackend::new(host),
        },
        transform as xform {
            inner_name: "markdown",
            plugin_type: MarkdownTransform,
            // The handle is deliberately dropped: a transform must stay pure
            // compute, and holding a host would make that a convention rather
            // than a fact.
            factory: |_cfg: &str, _host: ::mcpg_plugin_sdk::HostHandle| MarkdownTransform::new(),
        },
    ],
}
