# chatroom-bridge（Rust 版）镜像
# 镜像体积预期：约 80MB（状态图渲染为纯 Rust，不再需要 Chromium；仅额外装中文字体）。
# 多阶段构建：rust:1-slim 只负责编译，运行时基于 debian-slim，最终镜像不含工具链。
# 注意：构建上下文是仓库根目录（docker-compose.yml: context ., dockerfile rust/Dockerfile），
#       所以 COPY 路径都带 rust/ 前缀。

# ---------- 构建阶段 ----------
# rust:1-slim 是精简工具链镜像，不含 gcc；ring（rustls 依赖）等带 C 代码的 crate 需要编译器。
FROM rust:1-slim AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# 先拷清单与锁文件、再拷源码：Cargo.lock 未变时可命中 Docker 缓存，加速依赖解析
# （锁文件可能尚不存在，用 Cargo.lock* 通配，首次构建也能通过）
COPY rust/Cargo.toml rust/Cargo.lock* ./
COPY rust/src ./src
COPY rust/assets ./assets

# 依赖全部来自 crates.io 官方源（未配镜像；若部署网络确实很慢再自行加源，
# 如遇 git 依赖可设 CARGO_NET_GIT_FETCH_WITH_CLI=true，当前锁文件里没有 git 依赖）
RUN cargo build --release

# ---------- 运行阶段 ----------
FROM debian:bookworm-slim

# ca-certificates：出站 HTTPS（chatroom API 等）必需；curl 供 HEALTHCHECK 用；
# tzdata：让 TZ=Asia/Shanghai 真正生效（Python 镜像自带，这里显式补上）
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl tzdata \
    && rm -rf /var/lib/apt/lists/*

# 状态图渲染中文用 Noto Sans CJK SC 的「子集」：只含状态图会出现的字形（~100KB）。
# 不放完整 fonts-noto-cjk —— 那 4 个 ttc（~93MB）会被 fontdb 全部载入、常驻数十 MB。
# 子集由 tools/make_font_subset.py 生成，源文件在 rust/assets/fonts/。
COPY rust/assets/fonts/ /usr/share/fonts/opentype/noto-subset/

ENV RUST_LOG=info \
    TZ=Asia/Shanghai

COPY --from=builder /build/target/release/chatroom-bridge /usr/local/bin/chatroom-bridge

# /app 放 config.json（只读挂载），/data 放 state.json
RUN mkdir -p /app /data

EXPOSE 6199

HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3 \
  CMD curl -sf http://127.0.0.1:6199/healthz || exit 1

ENTRYPOINT ["/usr/local/bin/chatroom-bridge", "--config", "/app/config.json"]
