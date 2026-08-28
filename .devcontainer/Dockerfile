# Ignium 开发容器
# 定位:在宿主(Arch)上隔离出与 AGENTS.md 约定的 WSL2 Ubuntu 24.04
#       等价环境 —— Rust 1.97.1(锁定)+ riscv64gc 目标 + QEMU(OpenSBI)。
# 宿主 rustup 不受影响;构建产物属主与宿主用户对齐(默认 uid/gid=1000)。
#
# 构建:
#   docker build -t ignium-dev:1.97.1 \
#     --build-arg UID=$(id -u) --build-arg GID=$(id -g) \
#     -f .devcontainer/Dockerfile .
# 运行(make 门禁):
#   docker run --rm -v $PWD:/work -w /work ignium-dev:1.97.1 make clippy
#   docker run --rm -v $PWD:/work -w /work ignium-dev:1.97.1 make test

FROM ubuntu:24.04

# 注意:ARG 必须声明在 FROM 之后才在 RUN 中可见;
# 命名避免 UID/GID(shell 只读变量会覆盖 ARG 值)。
ARG HOST_UID=1000
ARG HOST_GID=1000
ARG RUST_CHANNEL=1.97.1

ENV DEBIAN_FRONTEND=noninteractive

# 基础工具 + QEMU(qemu-system-misc 提供 qemu-system-riscv64,含内置 OpenSBI)
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        curl \
        file \
        git \
        qemu-system-misc \
    && rm -rf /var/lib/apt/lists/*

# 与宿主用户对齐的容器内用户(便于挂载 repo 后属主一致)
# ubuntu:24.04 自带 uid/gid=1000 的 ubuntu 用户,需复用而非新建:
# 已有组/用户则改名,没有才新建。最终保证 uid/gid 与宿主一致。
RUN set -eux; \
    if getent group ${HOST_GID} >/dev/null 2>&1; then \
        GROUP_NAME="$(getent group ${HOST_GID} | cut -d: -f1)"; \
    else \
        groupadd -g ${HOST_GID} dev; GROUP_NAME="dev"; \
    fi; \
    if id ${HOST_UID} >/dev/null 2>&1; then \
        USER_NAME="$(id -un ${HOST_UID})"; \
        usermod -l dev -d /home/dev -m -g "${GROUP_NAME}" "${USER_NAME}"; \
    else \
        useradd -m -u ${HOST_UID} -g "${GROUP_NAME}" -s /bin/bash dev; \
    fi

# 工具链装到 dev 用户主目录(非 root 也可运行时自更新组件)。
# 源用 rsproxy.cn(有 1.97.1,国内速度快);static.rust-lang.org 直连易卡死,
# tuna 等镜像缺 1.97.1(404)。换源时注意版本要同时存在。
ENV RUSTUP_HOME=/home/dev/.rustup \
    CARGO_HOME=/home/dev/.cargo \
    PATH=/home/dev/.cargo/bin:$PATH \
    RUSTUP_DIST_SERVER=https://rsproxy.cn \
    RUSTUP_UPDATE_ROOT=https://rsproxy.cn/rustup

USER dev
WORKDIR /home/dev

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path --profile minimal \
    && rustup toolchain install ${RUST_CHANNEL} \
        --component rust-src,llvm-tools,clippy,rustfmt \
        --target riscv64gc-unknown-none-elf \
    && rustup default ${RUST_CHANNEL} \
    && rustup show

WORKDIR /work
