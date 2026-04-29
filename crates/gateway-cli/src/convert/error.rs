use snafu::Snafu;

pub type Result<T, E = ConvertError> = std::result::Result<T, E>;

#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum ConvertError {
  #[snafu(display("missing required field `{field}`"))]
  MissingField { field: &'static str },

  #[snafu(display("bad `{field}` shape: {message}"))]
  BadShape { field: &'static str, message: String },

  #[snafu(display("unsupported conversion feature: {message}"))]
  UnsupportedFeature { message: String },

  #[snafu(display("json conversion failed"))]
  Json { source: serde_json::Error },

  #[snafu(display("sse conversion failed: {message}"))]
  Sse { message: String },
}

impl ConvertError {
  pub fn bad_shape(field: &'static str, message: impl Into<String>) -> Self {
    Self::BadShape {
      field,
      message: message.into(),
    }
  }

  pub fn sse(message: impl Into<String>) -> Self {
    Self::Sse {
      message: message.into(),
    }
  }
}

impl From<serde_json::Error> for ConvertError {
  fn from(source: serde_json::Error) -> Self {
    Self::Json { source }
  }
}
