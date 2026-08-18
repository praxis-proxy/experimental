//! A no-op placeholder filter.
//!
//! This exists solely to prove that praxis-ai's build-time filter discovery and
//! the [`praxis_filter::export_filters!`] registration path work end to end for
//! this crate. It performs no request or response processing and will be
//! replaced by the real `switchyard_route` filter (praxis-proxy/experimental#2).

use async_trait::async_trait;
use praxis_filter::{FilterAction, FilterError, HttpFilter, HttpFilterContext};

/// A filter that does nothing and lets every request continue unchanged.
#[derive(Debug)]
pub(crate) struct PlaceholderFilter;

impl PlaceholderFilter {
    /// Builds a [`PlaceholderFilter`] from filter configuration.
    ///
    /// The configuration is ignored; this filter has no options.
    ///
    /// # Errors
    ///
    /// Never returns an error. The `Result` return type is required by the
    /// filter factory signature that [`praxis_filter::export_filters!`] expects.
    #[expect(
        clippy::unnecessary_wraps,
        reason = "the export_filters! factory signature requires Result even though this never fails"
    )]
    pub(crate) fn from_config(_config: &serde_yaml::Value) -> Result<Box<dyn HttpFilter>, FilterError> {
        Ok(Box::new(Self))
    }
}

#[async_trait]
impl HttpFilter for PlaceholderFilter {
    fn name(&self) -> &'static str {
        "experimental_placeholder"
    }

    async fn on_request(&self, _ctx: &mut HttpFilterContext<'_>) -> Result<FilterAction, FilterError> {
        Ok(FilterAction::Continue)
    }
}

#[cfg(test)]
#[expect(clippy::allow_attributes, reason = "blanket test-module suppressions")]
#[allow(clippy::unwrap_used, clippy::panic, reason = "unwrap/panic are acceptable in tests")]
mod tests {
    use praxis_filter::FilterRegistry;

    use super::PlaceholderFilter;

    /// The filter's advertised name matches the name it is registered under.
    #[test]
    fn name_matches_registration() {
        let filter = PlaceholderFilter;
        assert_eq!(
            praxis_filter::HttpFilter::name(&filter),
            "experimental_placeholder",
            "advertised name must match the export_filters! registration"
        );
    }

    /// The macro-generated `register_filters` makes the placeholder discoverable
    /// in a `FilterRegistry`, mirroring what praxis-ai's discovery does.
    #[test]
    fn placeholder_is_registered() {
        let mut registry = FilterRegistry::with_builtins();
        crate::register_filters(&mut registry);
        let names = registry.available_filters();
        assert!(
            names.contains(&"experimental_placeholder"),
            "expected experimental_placeholder to be registered, got: {names:?}"
        );
    }
}
