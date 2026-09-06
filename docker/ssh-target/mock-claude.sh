#!/bin/sh
set -eu

if [ "${1:-}" = "--version" ]; then
    echo "ralphus mock claude 1.0"
    exit 0
fi

session_id="mock-session-default"
previous=""
for argument in "$@"; do
    if [ "$previous" = "--session-id" ] || [ "$previous" = "--resume" ]; then
        session_id="$argument"
    fi
    previous="$argument"
done

if [ -n "${RALPHUS_MOCK_CLAUDE_DELAY_SECS:-}" ]; then
    sleep "$RALPHUS_MOCK_CLAUDE_DELAY_SECS"
fi

printf '%s\n' "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"$session_id\"}"
printf '%s\n' '{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"remote mock response"}}}'
printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"text","text":"remote mock response"}]}}'
printf '%s\n' '{"type":"result","usage":{"input_tokens":11,"output_tokens":7},"total_cost_usd":0.0123,"result":"remote mock completed"}'
