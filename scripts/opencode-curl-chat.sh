#!/bin/sh
set -eu

base_url="${1:?usage: opencode-curl-chat.sh <base-url>}"

curl --silent --show-error --fail \
  "$base_url/v1/chat/completions" \
  -H 'content-type: application/json' \
  -H 'user-agent: opencode/1.14.28 ai-sdk/provider-utils/4.0.23 runtime/bun/1.3.13' \
  -H 'x-behave-as: opencode' \
  -H 'x-session-id: sess-opencode-e2e' \
  -H 'x-opencode-project: /tmp/opencode-e2e' \
  -d '{
    "model": "openai/gpt-4o-mini",
    "messages": [{"role": "user", "content": "hi"}],
    "stream": false
  }'
