#!/usr/bin/env bash
# Run the dagger CLI with no network route at all, so it cannot upload traces
# or analytics to Dagger Cloud whatever credentials or defaults it finds.
#
# The CLI runs in a container with `--network none` and only the Docker socket
# mounted. It still reaches its engine -- the Docker daemon, outside the
# sandbox, starts the engine and does its pulls, and the CLI talks to it over
# `docker exec` on that socket -- but any other connection has no route. On a
# machine that is `dagger login`-ed this is the only proof that holds: an unset
# token or an isolated HOME only removes the reasons to upload.
#
# This proves the CLI cannot upload. The engine gets no token (none is passed),
# so it has nothing to upload with; that part is asserted, not sandboxed.
#
# Usage: DAGGER_BIN=/path/to/linux/dagger .github/dagger-sandboxed.sh <args>
# Set _EXPERIMENTAL_DAGGER_RUNNER_HOST to use an existing engine container.
set -euo pipefail
: "${DAGGER_BIN:?set DAGGER_BIN to a Linux dagger binary}"
repo=$(cd "$(dirname "$0")/.." && pwd)

exec docker run --rm -i --network none \
  -v /var/run/docker.sock:/var/run/docker.sock \
  -v "$DAGGER_BIN:/usr/local/bin/dagger:ro" \
  -v "$repo:$repo" -w "$repo" \
  -e HOME=/tmp/home -e DO_NOT_TRACK=1 -e DAGGER_NO_UPDATE_CHECK=1 \
  ${_EXPERIMENTAL_DAGGER_RUNNER_HOST:+-e _EXPERIMENTAL_DAGGER_RUNNER_HOST} \
  docker:29.4.0-cli dagger "$@"
