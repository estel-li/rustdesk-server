# syntax=docker/dockerfile:1.6
#
# 多 target 编译 hbbs / hbbr,共享一次 cargo build。
# CI:.github/workflows/ce-publish.yml(matrix.target = hbbs|hbbr)。
# 本地手动:
#   docker buildx build -f deploy/Dockerfile --target hbbs -t rustdesk-ce/hbbs:dev .
#   docker buildx build -f deploy/Dockerfile --target hbbr -t rustdesk-ce/hbbr:dev .
# build 上下文必须包含已 init 的 libs/hbb_common submodule
# (指向 estel-li/hbb_common-ce @ ce/base,见 docs/operations/hbb_common-ce.md)。

# ---- 编译阶段 ----
# Rust 1.88+ 必需:Cargo.lock 里 time / time-core / time-macros 要 1.88,
# wayland-protocols 要 1.86。1.85 装下来 cargo 一开局直接拒绝解依赖。
FROM rust:1.88-slim-bookworm AS builder

# CE-M0-2 引入了 sqlx + postgres feature,需要 pkg-config / libssl-dev / libpq-dev
# CE 全部 build deps:protoc(hbb_common build.rs)、cmake/clang(部分上游 crate)
# make + g++ 是 libsodium-sys 自带的 ./configure && make 流程所必需,slim 镜像不带
RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config \
        libssl-dev \
        libpq-dev \
        protobuf-compiler \
        cmake \
        clang \
        make \
        g++ \
        git \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src

# 先复制 manifest 让 cargo deps 命中 layer cache
COPY Cargo.toml Cargo.lock ./
COPY libs/hbb_common/Cargo.toml libs/hbb_common/Cargo.toml
COPY build.rs build.rs

# 然后复制完整源码(libs/hbb_common 也在内)
COPY . .

# 编两个 bin。release profile 与 cargo check 兼容,触发 CE-M0-2/M0-3/M0-6/M0-7 改动。
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --bin hbbs --bin hbbr \
    && mkdir -p /out \
    && cp target/release/hbbs target/release/hbbr /out/

# ---- 公共 runtime 基础层 ----
FROM debian:bookworm-slim AS runtime-base

# tini = pid1,signals;libssl3/libpq5 = sqlx 运行期需要;ca-certificates = TLS;
# adduser/passwd = 创建 rustdesk 系统用户
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        libssl3 \
        libpq5 \
        tini \
        adduser \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd -r -g 1000 rustdesk \
    && useradd -r -u 1000 -g rustdesk -d /var/lib/rustdesk-server -s /usr/sbin/nologin rustdesk \
    && mkdir -p /var/lib/rustdesk-server /var/log/rustdesk-server /run/rustdesk-server \
    && chown -R rustdesk:rustdesk /var/lib/rustdesk-server /var/log/rustdesk-server /run/rustdesk-server \
    && chmod 0750 /var/lib/rustdesk-server /var/log/rustdesk-server \
    && chmod 0770 /run/rustdesk-server

WORKDIR /var/lib/rustdesk-server

# ---- hbbs target ----
FROM runtime-base AS hbbs
COPY --from=builder /out/hbbs /usr/local/bin/hbbs
USER rustdesk:rustdesk
# 21115 NAT、21116 tcp+udp ID 注册、21118 WebSocket、21120 metrics(loopback 默认)
EXPOSE 21115 21116 21116/udp 21118 21120
VOLUME ["/var/lib/rustdesk-server"]
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/hbbs"]

# ---- hbbr target ----
FROM runtime-base AS hbbr
COPY --from=builder /out/hbbr /usr/local/bin/hbbr
USER rustdesk:rustdesk
# 21117 TCP relay、21119 WebSocket relay、21121 metrics(loopback 默认)
EXPOSE 21117 21119 21121
VOLUME ["/var/lib/rustdesk-server"]
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/hbbr"]
