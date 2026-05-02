#!/usr/bin/env bash
#
# Helm agent-runtime container entrypoint.
#
# Responsibilities:
#   1. Source environment overrides from /workspace/.env if present
#      (this is how the SessionDO/PDX-20 will inject per-session config).
#   2. Exec the requested agent CLI command with stdout/stderr unbuffered
#      so the Worker can stream events line-by-line back to the client.
#   3. On SIGTERM, forward to the child and give it 30s to shut down
#      gracefully before SIGKILL.
#
# Invocation:
#   /usr/local/bin/agent-entrypoint <cmd> [args...]
# Defaults to `claude --help` (set by Dockerfile CMD) so a bare `docker run`
# is a quick smoke-test.

set -euo pipefail

WORKSPACE="${HELM_WORKSPACE:-/workspace}"
GRACE_SECONDS="${HELM_SHUTDOWN_GRACE_SECONDS:-30}"

# 1) Source per-session env if mounted from R2/DO.
if [[ -f "${WORKSPACE}/.env" ]]; then
  # shellcheck disable=SC1091
  set -a
  . "${WORKSPACE}/.env"
  set +a
fi

# 2) Default to a no-op help command if nothing was passed.
if [[ "$#" -eq 0 ]]; then
  set -- claude --help
fi

# 3) Force unbuffered stdio for line-streaming.
export PYTHONUNBUFFERED=1
export NODE_NO_WARNINGS=1
# stdbuf isn't available in distroless-style images; we rely on each CLI
# auto-flushing when stdout is a pipe + Node's default line buffering.

child_pid=0

forward_signal() {
  local sig="$1"
  if [[ "${child_pid}" -gt 0 ]]; then
    # Send the signal to the child; ignore failures if it already exited.
    kill -s "${sig}" "${child_pid}" 2>/dev/null || true
  fi
}

graceful_shutdown() {
  echo "agent-entrypoint: received SIGTERM, forwarding and waiting up to ${GRACE_SECONDS}s" >&2
  forward_signal TERM
  # Give the child up to GRACE_SECONDS to exit cleanly.
  local waited=0
  while kill -0 "${child_pid}" 2>/dev/null; do
    if (( waited >= GRACE_SECONDS )); then
      echo "agent-entrypoint: grace period exceeded, sending SIGKILL" >&2
      forward_signal KILL
      break
    fi
    sleep 1
    waited=$((waited + 1))
  done
}

trap 'forward_signal INT' INT
trap 'graceful_shutdown' TERM
trap 'forward_signal HUP' HUP

# Run the agent command in the background so the shell can keep handling
# signals; then `wait` on it and propagate the exit code.
"$@" &
child_pid=$!

# `wait` returns 128+signo if interrupted by a signal; loop until the
# child actually exits so trap handlers can run to completion.
set +e
while true; do
  wait "${child_pid}"
  exit_code=$?
  if ! kill -0 "${child_pid}" 2>/dev/null; then
    break
  fi
done
set -e

exit "${exit_code}"
