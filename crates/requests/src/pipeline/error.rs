//! Pipeline error type. Stages are responsible for constructing this with the
//! correct [`Stage`] tag and `recoverable` flag; the runner emits a matching
//! [`StageEvent::Error`] verbatim from these fields.
//!
//! The `stop` flag distinguishes a *requested* short-circuit (a stage chose
//! to halt the pipeline without producing a response — e.g. a dry-run Send
//! stub) from a *real* failure. The runner treats both identically (emits
//! `Error` + `Completed { success: false }` and short-circuits), but
//! callers can branch on `err.stop` to render a successful dry-run report
//! instead of a failure report. State accumulated up to the stop point is
//! available to subscribers on the [`EventBus`] — each per-stage event
//! carries that stage's own output.
//!
//! [`StageEvent::Error`]: crate::event::StageEvent::Error
//! [`EventBus`]: crate::event::EventBus

use crate::event::Stage;
use crate::utils::codec::CodecError;
use llm_convert::error::ConvertError;
use llm_core::provider::Endpoint;
use smol_str::SmolStr;
use snafu::Snafu;

type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

#[derive(Debug, Snafu)]
#[snafu(visibility(pub))]
pub enum RequestsError {
  #[snafu(display("session expired: {session_id}"))]
  SessionExpired { session_id: SmolStr },

  #[snafu(display("no account supports endpoint {endpoint} for model {model}"))]
  NoAccount { endpoint: Endpoint, model: SmolStr },

  #[snafu(display("request conversion failed: {source}"))]
  RequestConversion { source: ConvertError },

  #[snafu(display("provider input_transformer failed: {source}"))]
  ProviderInputTransformer { source: llm_core::provider::Error },

  #[snafu(display("serialize upstream body: {source}"))]
  SerializeUpstreamBody { source: serde_json::Error },

  #[snafu(display("re-encode outbound body: {source}"))]
  ReencodeOutboundBody { source: CodecError },

  #[snafu(display("upstream body not valid JSON: {source}"))]
  UpstreamBodyNotJson { source: serde_json::Error },

  #[snafu(display("response conversion failed: {source}"))]
  ResponseConversion { source: ConvertError },

  #[snafu(display("serializing translated response failed: {source}"))]
  SerializeTranslatedResponse { source: serde_json::Error },

  #[snafu(display("upstream {status}: failed to read body: {source}"))]
  UpstreamReadFailed { status: u16, source: reqwest::Error },

  #[snafu(display("upstream {status}: {body}"))]
  UpstreamStatus { status: u16, body: String },

  #[snafu(display("reading upstream body: {source}"))]
  ReadingUpstreamBody { source: reqwest::Error },

  #[snafu(display("dry-run profile stopped before contacting upstream"))]
  Stop,
}

#[derive(Debug)]
pub struct PipelineError {
  pub stage: Stage,
  source: BoxError,
  /// `true` iff a retry-style decorator may sensibly re-invoke the stage.
  /// Permanent failures (bad request, 4xx, unknown model) set this to
  /// `false`. Always `false` when `stop == true`.
  pub recoverable: bool,
  /// `true` iff a stage deliberately halted the pipeline rather than
  /// failing. Used by dry-run Send stubs and other "stop here" stages.
  /// Callers should treat this as a successful early termination.
  pub stop: bool,
}

impl PipelineError {
  pub fn permanent<E>(stage: Stage, source: E) -> Self
  where
    E: std::error::Error + Send + Sync + 'static,
  {
    Self {
      stage,
      source: Box::new(source),
      recoverable: false,
      stop: false,
    }
  }

  pub fn recoverable<E>(stage: Stage, source: E) -> Self
  where
    E: std::error::Error + Send + Sync + 'static,
  {
    Self {
      stage,
      source: Box::new(source),
      recoverable: true,
      stop: false,
    }
  }

  /// A deliberate short-circuit: the stage chose to halt the pipeline
  /// without producing a response. Not a failure; callers should render
  /// whatever partial state they captured (typically via the event bus).
  pub fn stop(stage: Stage) -> Self {
    Self {
      stage,
      source: Box::new(RequestsError::Stop),
      recoverable: false,
      stop: true,
    }
  }

  pub fn source_ref(&self) -> &(dyn std::error::Error + 'static) {
    self.source.as_ref()
  }

  pub fn message(&self) -> std::borrow::Cow<'_, str> {
    let s = self.source.to_string();
    std::borrow::Cow::Owned(s)
  }
}

impl std::fmt::Display for PipelineError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "[{}] {}", self.stage, self.source)
  }
}

impl std::error::Error for PipelineError {
  fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
    Some(self.source_ref())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[derive(Debug, Snafu)]
  #[snafu(display("boom"))]
  struct Boom;

  #[test]
  fn preserves_inner_error_as_source() {
    let err = PipelineError::permanent(Stage::Resolve, Boom);
    let source = err.source_ref();
    assert!(source.downcast_ref::<Boom>().is_some());
    assert_eq!(err.to_string(), "[resolve] boom");
  }
}
