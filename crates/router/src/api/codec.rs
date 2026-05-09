use super::error::ApiError;
use axum::http::header::{ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_LENGTH, VARY};
use axum::http::{HeaderMap, HeaderValue};
use bytes::Bytes;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use serde_json::Value;
use std::io::{Read, Write};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ContentEncodingKind {
  Gzip,
  Zstd,
}

impl ContentEncodingKind {
  pub(crate) fn as_str(self) -> &'static str {
    match self {
      ContentEncodingKind::Gzip => "gzip",
      ContentEncodingKind::Zstd => "zstd",
    }
  }
}

#[derive(Clone, Debug)]
pub(crate) struct DecodedJsonRequest {
  pub raw_body: Bytes,
  pub value: Value,
  pub encoding: Option<ContentEncodingKind>,
}

pub(crate) fn decode_json_request(headers: &HeaderMap, raw_body: Bytes) -> Result<DecodedJsonRequest, ApiError> {
  let encoding = request_content_encoding(headers)?;
  let decoded = decode_body_bytes(raw_body.clone(), encoding)?;
  let value: Value =
    serde_json::from_slice(&decoded).map_err(|e| ApiError::bad_request(format!("invalid JSON request body: {e}")))?;
  Ok(DecodedJsonRequest {
    raw_body,
    value,
    encoding,
  })
}

pub(crate) fn encode_body_bytes(body: &[u8], encoding: Option<ContentEncodingKind>) -> Result<Bytes, String> {
  match encoding {
    None => Ok(Bytes::copy_from_slice(body)),
    Some(ContentEncodingKind::Gzip) => {
      let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
      encoder
        .write_all(body)
        .map_err(|e| format!("gzip encode failed: {e}"))?;
      encoder
        .finish()
        .map(Bytes::from)
        .map_err(|e| format!("gzip encode failed: {e}"))
    }
    Some(ContentEncodingKind::Zstd) => zstd::stream::encode_all(body, 0)
      .map(Bytes::from)
      .map_err(|e| format!("zstd encode failed: {e}")),
  }
}

pub(crate) fn request_content_encoding(headers: &HeaderMap) -> Result<Option<ContentEncodingKind>, ApiError> {
  let Some(value) = headers.get(CONTENT_ENCODING) else {
    return Ok(None);
  };
  let value = value
    .to_str()
    .map_err(|_| ApiError::unsupported_media_type("unsupported content-encoding header"))?;
  let mut encodings = value
    .split(',')
    .map(str::trim)
    .filter(|part| !part.is_empty() && !part.eq_ignore_ascii_case("identity"));
  let first = match encodings.next() {
    Some(first) => first,
    None => return Ok(None),
  };
  if encodings.next().is_some() {
    return Err(ApiError::unsupported_media_type(
      "multiple content-encodings are not supported",
    ));
  }
  match first.to_ascii_lowercase().as_str() {
    "gzip" => Ok(Some(ContentEncodingKind::Gzip)),
    "zstd" => Ok(Some(ContentEncodingKind::Zstd)),
    other => Err(ApiError::unsupported_media_type(format!(
      "unsupported content-encoding '{other}'"
    ))),
  }
}

pub(crate) fn negotiate_response_encoding(headers: &HeaderMap) -> Option<ContentEncodingKind> {
  let value = headers.get(ACCEPT_ENCODING)?.to_str().ok()?;
  let mut best: Option<(ContentEncodingKind, f32)> = None;
  for entry in value.split(',') {
    let mut parts = entry.split(';').map(str::trim);
    let token = parts.next()?.to_ascii_lowercase();
    let mut q = 1.0_f32;
    for param in parts {
      if let Some(rest) = param.strip_prefix("q=") {
        q = rest.parse::<f32>().ok().unwrap_or(0.0);
      }
    }
    if q <= 0.0 {
      continue;
    }
    let Some(encoding) = (match token.as_str() {
      "gzip" => Some(ContentEncodingKind::Gzip),
      "zstd" => Some(ContentEncodingKind::Zstd),
      _ => None,
    }) else {
      continue;
    };
    match best {
      None => best = Some((encoding, q)),
      Some((_, best_q)) if q > best_q => best = Some((encoding, q)),
      Some((ContentEncodingKind::Gzip, best_q)) if q == best_q && encoding == ContentEncodingKind::Zstd => {
        best = Some((encoding, q))
      }
      _ => {}
    }
  }
  best.map(|(encoding, _)| encoding)
}

pub(crate) fn maybe_compress_buffered_response(
  request_headers: &HeaderMap,
  response_headers: &mut HeaderMap,
  body: Bytes,
) -> Result<Bytes, String> {
  if body.is_empty() || response_headers.contains_key(CONTENT_ENCODING) {
    return Ok(body);
  }
  let Some(encoding) = negotiate_response_encoding(request_headers) else {
    return Ok(body);
  };
  let compressed = encode_body_bytes(body.as_ref(), Some(encoding))?;
  response_headers.insert(CONTENT_ENCODING, HeaderValue::from_static(encoding.as_str()));
  response_headers.remove(CONTENT_LENGTH);
  response_headers.insert(VARY, HeaderValue::from_static("accept-encoding"));
  Ok(compressed)
}

fn decode_body_bytes(body: Bytes, encoding: Option<ContentEncodingKind>) -> Result<Bytes, ApiError> {
  match encoding {
    None => Ok(body),
    Some(ContentEncodingKind::Gzip) => {
      let mut decoder = GzDecoder::new(body.as_ref());
      let mut out = Vec::new();
      decoder
        .read_to_end(&mut out)
        .map_err(|e| ApiError::bad_request(format!("gzip decode failed: {e}")))?;
      Ok(Bytes::from(out))
    }
    Some(ContentEncodingKind::Zstd) => zstd::stream::decode_all(body.as_ref())
      .map(Bytes::from)
      .map_err(|e| ApiError::bad_request(format!("zstd decode failed: {e}"))),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use axum::response::IntoResponse;

  #[test]
  fn negotiate_prefers_highest_q_then_zstd() {
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("gzip;q=0.8, zstd;q=0.8"));
    assert_eq!(negotiate_response_encoding(&headers), Some(ContentEncodingKind::Zstd));
  }

  #[test]
  fn gzip_round_trip() {
    let body = br#"{"model":"gpt-5","input":"hi"}"#;
    let encoded = encode_body_bytes(body, Some(ContentEncodingKind::Gzip)).unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_ENCODING, HeaderValue::from_static("gzip"));
    let decoded = decode_json_request(&headers, encoded).unwrap();
    assert_eq!(decoded.value["model"], "gpt-5");
  }

  #[test]
  fn zstd_round_trip() {
    let body = br#"{"model":"gpt-5","input":"hi"}"#;
    let encoded = encode_body_bytes(body, Some(ContentEncodingKind::Zstd)).unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_ENCODING, HeaderValue::from_static("zstd"));
    let decoded = decode_json_request(&headers, encoded).unwrap();
    assert_eq!(decoded.value["model"], "gpt-5");
  }

  #[test]
  fn rejects_unsupported_content_encoding() {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_ENCODING, HeaderValue::from_static("br"));
    let err = decode_json_request(&headers, Bytes::from_static(br#"{}"#)).unwrap_err();
    assert_eq!(
      err.into_response().status(),
      axum::http::StatusCode::UNSUPPORTED_MEDIA_TYPE
    );
  }

  #[test]
  fn compresses_buffered_response_for_supported_accept_encoding() {
    let mut request_headers = HeaderMap::new();
    request_headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("gzip"));
    let mut response_headers = HeaderMap::new();
    let body = Bytes::from_static(br#"{"ok":true}"#);
    let compressed = maybe_compress_buffered_response(&request_headers, &mut response_headers, body.clone()).unwrap();
    assert_ne!(compressed, body);
    assert_eq!(
      response_headers.get(CONTENT_ENCODING).and_then(|v| v.to_str().ok()),
      Some("gzip")
    );
  }
}
