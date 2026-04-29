pub mod chat;
pub mod error;
pub mod ir;
pub mod messages;
pub mod responses;
pub mod sse;

use crate::provider::Endpoint;
use serde_json::Value;

pub use error::Result;

pub fn convert_request(from: Endpoint, to: Endpoint, body: &Value) -> Result<Value> {
  if from == to {
    return Ok(body.clone());
  }
  let req = match from {
    Endpoint::ChatCompletions => chat::request_from_value(body)?,
    Endpoint::Responses => responses::request_from_value(body)?,
    Endpoint::Messages => messages::request_from_value(body)?,
  };
  match to {
    Endpoint::ChatCompletions => chat::request_to_value(&req),
    Endpoint::Responses => responses::request_to_value(&req),
    Endpoint::Messages => messages::request_to_value(&req),
  }
}

pub fn convert_response(from: Endpoint, to: Endpoint, body: &Value) -> Result<Value> {
  if from == to {
    return Ok(body.clone());
  }
  let resp = match from {
    Endpoint::ChatCompletions => chat::response_from_value(body)?,
    Endpoint::Responses => responses::response_from_value(body)?,
    Endpoint::Messages => messages::response_from_value(body)?,
  };
  match to {
    Endpoint::ChatCompletions => chat::response_to_value(&resp),
    Endpoint::Responses => responses::response_to_value(&resp),
    Endpoint::Messages => messages::response_to_value(&resp),
  }
}
