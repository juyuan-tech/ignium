#!/usr/bin/env bash
# 在 ignium-dev 容器内执行 make(等同 WSL2 里的 make)。
# 用法:.devcontainer/docker-make.sh test | clippy | fmt | qemu | ...
# 注意:仓库需已构建镜像 ignium-dev:1.97.1(见同目录 Dockerfile)。
set -euo pipefail
IMG="${IGNIUM_DEV_IMAGE:-ignium-dev:1.97.1}"
exec docker run --rm \
    -v "$PWD:/work" \
    -w /work \
    "$IMG" \
    make "$@"
