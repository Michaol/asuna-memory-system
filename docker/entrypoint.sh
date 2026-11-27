#!/bin/bash
set -euo pipefail

# AMS Gateway Entry Point

EFFECTIVE_PORT="${AMS_GATEWAY_PORT:-8765}"
HAS_PORT_ARG=0
expect_port_value=0
# W2: --config / --profile are global clap args; extract them (both
# "--flag VALUE" and "--flag=VALUE" forms) and forward them to the doctor
# startup check so it validates the same config/profile the gateway will use.
DOCTOR_ARGS=()
expect_config_value=0
expect_profile_value=0
for arg in "$@"; do
    if [[ "$expect_port_value" -eq 1 ]]; then
        EFFECTIVE_PORT="$arg"
        HAS_PORT_ARG=1
        expect_port_value=0
        continue
    fi
    if [[ "$expect_config_value" -eq 1 ]]; then
        DOCTOR_ARGS+=(--config "$arg")
        expect_config_value=0
        continue
    fi
    if [[ "$expect_profile_value" -eq 1 ]]; then
        DOCTOR_ARGS+=(--profile "$arg")
        expect_profile_value=0
        continue
    fi
    case "$arg" in
        --port)
            HAS_PORT_ARG=1
            expect_port_value=1
            ;;
        --port=*)
            EFFECTIVE_PORT="${arg#--port=}"
            HAS_PORT_ARG=1
            ;;
        --config)
            expect_config_value=1
            ;;
        --config=*)
            DOCTOR_ARGS+=(--config "${arg#--config=}")
            ;;
        --profile)
            expect_profile_value=1
            ;;
        --profile=*)
            DOCTOR_ARGS+=(--profile "${arg#--profile=}")
            ;;
    esac
done

echo "=== Asuna Memory System ===" >&2
echo "Version: $(asuna-memory --version)" >&2
echo "Gateway port: ${EFFECTIVE_PORT}" >&2
echo "" >&2

# Initialize database if needed
echo "Checking database..." >&2
asuna-memory doctor "${DOCTOR_ARGS[@]}" >&2 || {
    echo "Database initialization failed" >&2
    exit 1
}

# Start Gateway
# clap rejects a duplicate --port (ArgAction::Set), so only inject the default
# when the caller did not already pass one through "$@".
if [[ "$HAS_PORT_ARG" -eq 1 ]]; then
    echo "Starting AMS Gateway with caller-provided --port (${EFFECTIVE_PORT})..." >&2
    exec asuna-memory gateway "$@"
fi
echo "Starting AMS Gateway on port ${EFFECTIVE_PORT}..." >&2
exec asuna-memory gateway --port "${EFFECTIVE_PORT}" "$@"
