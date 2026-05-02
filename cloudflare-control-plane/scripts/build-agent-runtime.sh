#!/usr/bin/env bash
#
# Build the helm/agent-runtime container image and tag it with the current
# git SHA. Intended to be run from the cloudflare-control-plane directory
# (or via `npm run build:container`).

set -euo pipefail

cd "$(dirname "$0")/.."

IMAGE_NAME="${HELM_AGENT_IMAGE:-helm/agent-runtime}"
GIT_SHA="$(git rev-parse --short=12 HEAD 2>/dev/null || echo "unknown")"
BUILD_DATE="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
CLAUDE_CODE_VERSION="${CLAUDE_CODE_VERSION:-latest}"
CODEX_VERSION="${CODEX_VERSION:-latest}"

echo "Building ${IMAGE_NAME} (sha=${GIT_SHA}, date=${BUILD_DATE})"

docker build \
  --file Dockerfile.agent-runtime \
  --tag "${IMAGE_NAME}:${GIT_SHA}" \
  --tag "${IMAGE_NAME}:latest" \
  --build-arg "GIT_SHA=${GIT_SHA}" \
  --build-arg "BUILD_DATE=${BUILD_DATE}" \
  --build-arg "CLAUDE_CODE_VERSION=${CLAUDE_CODE_VERSION}" \
  --build-arg "CODEX_VERSION=${CODEX_VERSION}" \
  .

echo
echo "Built ${IMAGE_NAME}:${GIT_SHA}"
echo "Tagged ${IMAGE_NAME}:latest"
