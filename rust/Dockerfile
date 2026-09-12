# chatroom-bridge（Rust 版）镜像
# 镜像体积预期：无 Chromium 约 80MB；STATUS_IMAGE=true 带 Chromium + 中文字体约 400MB。
# 多阶段构建：rust:1-slim 只负责编译，运行时基于 debian:bookworm-slim，最终镜像不含工具链。
# 注意：构建上下文是仓库根目录（docker-compose.yml: context ., dockerfile rust/Dockerfile），
#       所以 COPY 路径都带 rust/ 前缀。

# ---------- 构建阶段 ----------
# rust:1-slim 是精简工具链镜像，不含 gcc；ring（rustls 依赖）等带 C 代码的 crate 需要编译器。
FROM rust:1-slim AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential pkg-config \
    && rm -rf /var/lib/apt/lists/*

# STATUS_IMAGE=true 时启用 /server 状态图渲染 feature（编译期开关，运行阶段配套装 Chromium）
ARG STATUS_IMAGE=false

WORKDIR /build

# 先拷清单与锁文件、再拷源码：Cargo.lock 未变时可命中 Docker 缓存，加速依赖解析
# （锁文件可能尚不存在，用 Cargo.lock* 通配，首次构建也能通过）
COPY rust/Cargo.toml rust/Cargo.lock* ./
COPY rust/src ./src

# 依赖全部来自 crates.io 官方源（未配镜像；若部署网络确实很慢再自行加源，
# 如遇 git 依赖可设 CARGO_NET_GIT_FETCH_WITH_CLI=true，当前锁文件里没有 git 依赖）
RUN if [ "$STATUS_IMAGE" = "true" ]; then \
        cargo build --release --features status-image; \
    else \
        cargo build --release; \
    fi

# ---------- 运行阶段 ----------
FROM debian:bookworm-slim

# ca-certificates：出站 HTTPS（chatroom API 等）必需；curl 供 HEALTHCHECK 用；
# tzdata：让 TZ=Asia/Shanghai 真正生效（Python 镜像自带，这里显式补上）
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl tzdata \
    && rm -rf /var/lib/apt/lists/*

# STATUS_IMAGE=true 时安装 Chromium 与 Noto CJK 中文字体（状态图里渲染中文）
ARG STATUS_IMAGE=false
RUN if [ "$STATUS_IMAGE" = "true" ]; then \
        apt-get update && apt-get install -y --no-install-recommends \
            chromium fonts-noto-cjk \
        && rm -rf /var/lib/apt/lists/* ; \
    fi

# Chromium 渲染 /server 状态图：headless_chrome 通过 CHROME_PATH 定位浏览器
# （STATUS_IMAGE=false 时未编译渲染代码，此变量仅是占位，无害）
ENV CHROME_PATH=/usr/bin/chromium \
    RUST_LOG=info \
    TZ=Asia/Shanghai

COPY --from=builder /build/target/release/chatroom-bridge /usr/local/bin/chatroom-bridge

# /app 放 config.json（只读挂载），/data 放 state.json
RUN mkdir -p /app /data

EXPOSE 6199

HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3 \
  CMD curl -sf http://127.0.0.1:6199/healthz || exit 1

ENTRYPOINT ["/usr/local/bin/chatroom-bridge", "--config", "/app/config.json"]
