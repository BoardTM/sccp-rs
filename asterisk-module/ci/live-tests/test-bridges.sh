#!/bin/sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)

SCCP_LIVE_BRIDGES=1 \
SCCP_LIFECYCLE_WARMUP_CYCLES=${SCCP_LIVE_WARMUP_CYCLES:-2} \
SCCP_LIFECYCLE_BATCH_CYCLES=${SCCP_LIVE_BATCH_CYCLES:-2} \
	exec "$script_dir/../test-native-lifecycle.sh" "$@"
