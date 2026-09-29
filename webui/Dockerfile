# cxu-chathub Web 控制台中间层（Rust + axum）
#
# 需要 docker CLI 操作宿主容器（docker restart），因此挂载 docker.sock。
# 采用多阶段构建：builder 里编译静态（musl）二进制，运行镜像只留二进制 + 前端。
# 国内构建慢的话，可在 builder 阶段配置 crates 镜像（见 README「构建加速」）。

# ---------- 构建阶段 ----------
FROM rust:1-alpine AS builder
# ring/rustls 等需要 C 工具链
RUN apk add --no-cache build-base musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release

# ---------- 运行阶段 ----------
FROM alpine:3.20
# docker restart 需要容器内有 docker CLI（sock 由 compose 挂载）
RUN apk add --no-cache docker-cli ca-certificates tzdata
WORKDIR /app
COPY --from=builder /src/target/release/cxu-console /app/cxu-console
COPY public ./public

# 说明：本容器挂载了 docker.sock，等价于宿主 root 能力，因此不再降权运行；
# 若不需要重启功能，请设 CXU_DOCKER_ENABLED=false 并去掉 docker.sock 挂载。
EXPOSE 9090
CMD ["/app/cxu-console"]
