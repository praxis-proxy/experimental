//! The decision-only step loop.
//!
//! Switchyard's `run_stream` yields *steps*; the host owns all I/O. For
//! Capability mode the order is: (1) the judge `CallLlm`
//! (`is_routed_call() == false`) — serve it; (2) `Step::Decision` with
//! `is_routed_call() == true` — the routing decision, emitted **before** any
//! answer call: read the tier tag and stop; (3) the answer `CallLlm` and
//! (4) `ReturnToAgent` — never reached. Dropping the [`StepStream`] is the
//! intended abandon mechanism: the algorithm task is aborted cleanly and an
//! unanswered answer call resolves to a benign, unobserved internal error.

use futures::StreamExt as _;
use switchyard_libsy::{Step, StepStream};

use super::{error::RouteError, judge::JudgeTransport};

/// Drives the step stream to the routed decision and returns its tier tag.
///
/// The judge call is served through `transport`; the answer call is never
/// served. Returning (on success or failure) drops the stream, which aborts
/// the algorithm task.
///
/// # Errors
///
/// Returns a [`RouteError`] when the judge callout fails, the run surfaces an
/// error step, or the stream ends without a routed decision.
pub(crate) async fn decide(
    mut stream: StepStream,
    judge_model: &str,
    transport: &dyn JudgeTransport,
) -> Result<String, RouteError> {
    while let Some(item) = stream.next().await {
        let step = item.map_err(|err| RouteError::Run(err.to_string()))?;
        match step {
            Step::Decision(decision) if decision.is_routed_call() => {
                return Ok(decision.selected_model().to_owned());
            },
            Step::Decision(_) => {},
            Step::CallLlm(call) => {
                if call.get_decision().is_routed_call() {
                    // The answer call. Capability emits Step::Decision first,
                    // so this is defensive: learn the tier and stop without
                    // serving the answer.
                    return Ok(call.get_decision().selected_model().to_owned());
                }
                super::judge::serve_judge(call, judge_model, transport).await?;
            },
            Step::ReturnToAgent(_) => return Err(RouteError::NoDecision),
        }
    }
    Err(RouteError::NoDecision)
}
