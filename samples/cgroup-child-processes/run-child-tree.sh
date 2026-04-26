#!/usr/bin/env bash

set -euo pipefail

CHILD_COUNT="${1:-4}"
CHUNK_MB="${2:-16}"
RUN_SECONDS="${3:-8}"

if ! [[ "$CHILD_COUNT" =~ ^[0-9]+$ ]] || ! [[ "$CHUNK_MB" =~ ^[0-9]+$ ]] || ! [[ "$RUN_SECONDS" =~ ^[0-9]+$ ]]; then
  echo "usage: $0 [CHILD_COUNT] [CHUNK_MB_PER_TICK] [RUN_SECONDS]" >&2
  exit 2
fi

if [[ "$CHILD_COUNT" -lt 1 ]] || [[ "$CHUNK_MB" -lt 1 ]] || [[ "$RUN_SECONDS" -lt 1 ]]; then
  echo "all arguments must be positive integers" >&2
  exit 2
fi

echo "spawning ${CHILD_COUNT} child workers for ${RUN_SECONDS}s"

pids=()
for idx in $(seq 1 "$CHILD_COUNT"); do
  CHUNK_MB="$CHUNK_MB" \
  RUN_SECONDS="$RUN_SECONDS" \
  WORKER_ID="$idx" \
  python3 - "$CHILD_COUNT" <<'PY' &
import os
import sys
import time

chunk_mb = int(os.environ["CHUNK_MB"])
run_seconds = int(os.environ["RUN_SECONDS"])
worker_id = os.environ["WORKER_ID"]

chunk_size = chunk_mb * 1024 * 1024
chunk = bytes(chunk_size)
blocks = []
deadline = time.time() + run_seconds

while time.time() < deadline:
    blocks.append(bytearray(chunk))
    blocks[-1][0] = 1
    time.sleep(0.05)

print(f"child {worker_id} complete, blocks={len(blocks)}")
PY
  pids+=("$!")
done

failed=0
for pid in "${pids[@]}"; do
  if ! wait "$pid"; then
    failed=1
  fi
done

if [[ "$failed" -ne 0 ]]; then
  echo "one or more workers failed"
  exit 1
fi

echo "all children complete"
