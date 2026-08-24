//! A no-op placeholder filter.
//!
//! This exists solely to prove that praxis-ai's build-time filter discovery and
//! the [`praxis_filter::export_filters!`] registration path work end to end for
//! this crate. It performs no request or response processing and will be
//! replaced by the real experimental filters (praxis-proxy/ai#758).

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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[expect(clippy::allow_attributes, reason = "blanket test-module suppressions")]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "unwrap/expect/panic are acceptable in tests"
)]
mod tests {
    use praxis_filter::FilterRegistry;

    use super::PlaceholderFilter;

    #[test]
    fn name_matches_registration() {
        let filter = PlaceholderFilter;
        assert_eq!(
            praxis_filter::HttpFilter::name(&filter),
            "experimental_placeholder",
            "advertised name must match the export_filters! registration"
        );
    }

    #[test]
    fn from_config_ignores_configuration() {
        let config = serde_yaml::from_str("anything: ignored").expect("test YAML should parse");
        let filter = PlaceholderFilter::from_config(&config).expect("placeholder config never fails");
        assert_eq!(
            filter.name(),
            "experimental_placeholder",
            "filter built from config must advertise the registered name"
        );
    }

    #[test]
    fn from_config_accepts_empty_mapping() {
        let config = serde_yaml::from_str("{}").expect("empty mapping should parse");
        assert!(
            PlaceholderFilter::from_config(&config).is_ok(),
            "an empty config mapping must be accepted"
        );
    }

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
