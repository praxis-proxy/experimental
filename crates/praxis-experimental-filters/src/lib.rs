//! Experimental Praxis AI filters.
//!
//! This crate is discovered at build time by praxis-ai's
//! `[package.metadata.praxis-filters]` auto-discovery (see praxis-ai
//! `server/build.rs` and `praxis-ai-build-support`). The generated registration
//! code calls [`register_filters`], which is emitted by the
//! [`praxis_filter::export_filters!`] macro invoked below.
//!
//! For now this crate ships a single no-op placeholder filter
//! (`experimental_placeholder`), so that end-to-end discovery and registration
//! are provable before the real filters land (`switchyard_route` in
//! praxis-proxy/experimental#2; `api_key_auth` and `token_ceiling` under the
//! Standalone AI Gateway MVP epic, praxis-proxy/ai#758).

mod placeholder;

praxis_filter::export_filters! {
    http "experimental_placeholder" => placeholder::PlaceholderFilter::from_config,
}
