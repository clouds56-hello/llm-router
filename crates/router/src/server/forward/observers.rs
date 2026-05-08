use super::recording::CallRecordBuilder;
use crate::db::CallRecord;
use bytes::Bytes;

pub(super) fn build_stream_record(
  builder: CallRecordBuilder,
  usage: (Option<u64>, Option<u64>),
  captured: Bytes,
  resp_headers: &reqwest::header::HeaderMap,
) -> CallRecord {
  builder
    .with_response_body(captured.clone())
    .with_outbound_response(Some(resp_headers), Some(&captured))
    .with_usage(usage.0, usage.1)
    .build()
}
