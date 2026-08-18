//! Assembly of the Switchyard Capability-mode classifier.

use std::sync::Arc;

use praxis_filter::FilterError;
use switchyard_libsy::{
    Algorithm, ClassifierContractConfig, LlmClassifierConfig, LlmTarget, LlmTaskClassifier, TaskClassifierConfig,
};

use super::config::{RouteConfig, Tier};

/// Switchyard `semantic_name` for the judge target.
pub(crate) const JUDGE_TAG: &str = "judge";

/// Builds the Capability-mode classifier from validated filter config.
///
/// Built **once** at `from_config` time: the algorithm owns per-session state
/// machinery (with an internal hourly sweeper task spawned on first use) that
/// must not be re-created per request — and none of it affects routing while
/// `session_affinity` stays `false`, which it must (Switchyard's affinity is a
/// first-decision-wins latch, not a ratchet; the session floor in `floor.rs`
/// owns the no-downgrade guarantee instead).
///
/// # Errors
///
/// Returns a [`FilterError`] when Switchyard rejects the classifier config.
pub(crate) fn build_algorithm(config: &RouteConfig) -> Result<Arc<dyn Algorithm>, FilterError> {
    let classifier_config = LlmClassifierConfig::Capability {
        judge_target: target(JUDGE_TAG),
        efficient_target: target(Tier::Weak.tag()),
        capable_target: target(Tier::Strong.tag()),
        config: TaskClassifierConfig {
            base_threshold: config.threshold,
            session_affinity: false,
            contract: ClassifierContractConfig::default(),
            ..TaskClassifierConfig::default()
        },
    };
    let classifier = LlmTaskClassifier::new(classifier_config)
        .map_err(|err| -> FilterError { format!("switchyard_route: classifier config rejected: {err}").into() })?;
    Ok(Arc::new(classifier))
}

/// Builds a client-less [`LlmTarget`] for `tag`.
///
/// No target carries an `llm_client`: in `run_stream` mode the driver always
/// offloads every model call to the step stream, so the filter serves the
/// judge itself (and never serves the answer call).
fn target(tag: &str) -> LlmTarget {
    LlmTarget {
        semantic_name: tag.to_owned(),
        llm_client: None,
    }
}
