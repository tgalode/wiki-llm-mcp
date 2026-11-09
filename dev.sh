#!/usr/bin/env bash
# Runs cargo in a throwaway container, so the host needs no Rust toolchain.
# Usage: ./dev.sh test | ./dev.sh clippy --all-targets | ./dev.sh build --release
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
image=wiki-llm-mcp-dev
docker image inspect "$image" >/dev/null 2>&1 || docker build -q -t "$image" -f Dockerfile.dev . >/dev/null
exec docker run --rm -i -u "$(id -u):$(id -g)" --memory 4g --cpus 3 \
  -e HOME=/tmp -e CARGO_HOME=/work/target/.cargo -e CARGO_BUILD_JOBS=2 \
  -v "$PWD:/work" -w /work "$image" cargo "$@"
