# syntax=docker/dockerfile:1.7
#
# 构建上下文必须是【仓库根目录】（同时包含 autofilm/ 与 mediawarp/）：
#   docker build -f autofilm/docker/Dockerfile -t autofilm:local .
# mediawarp/ 是 git submodule，构建前先 `git submodule update --init`；两个项目的源码
# 都从本地上下文取，构建过程不再 clone GitHub。

FROM rust:alpine AS builder

WORKDIR /workspace

RUN apk add --no-cache build-base musl-dev

COPY autofilm/Cargo.toml autofilm/Cargo.lock ./
COPY autofilm/alist-client-rs ./alist-client-rs
COPY autofilm/src ./src
COPY autofilm/build.rs ./build.rs
# build.rs 里的 vergen-gix 需要 git 元数据生成版本信息；只存在于 builder 阶段，
# 不会进入最终镜像。若从源码 zip（无 .git）构建，兜底开关保证 env!() 仍能编译。
COPY .git ./.git
ENV VERGEN_DEFAULT_ON_ERROR=1

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/workspace/target \
    cargo build --release --locked && \
    cp /workspace/target/release/autofilm /tmp/autofilm

# MediaWarp 使用 submodule mediawarp/（zian-yan/MediaWarp，当前指向 v1.2.3）的源码编译。
# Go 依赖仍由 go mod download 从模块代理获取；如需完全离线，先在 mediawarp/
# 下执行 `go mod vendor`，并把下面的构建改成 `go build -mod=vendor`。
FROM --platform=$BUILDPLATFORM golang:1.26-alpine AS mediawarp-builder

ARG TARGETARCH=amd64
ENV GOTOOLCHAIN=auto \
    CGO_ENABLED=0

WORKDIR /src
COPY mediawarp/ .
RUN GOOS=linux GOARCH="${TARGETARCH}" go build -trimpath -ldflags "-s -w" \
        -o /tmp/mediawarp .

FROM alpine:latest

ENV TZ=Asia/Shanghai

RUN apk add --no-cache ca-certificates tzdata

COPY --from=builder /tmp/autofilm /usr/local/bin/autofilm
COPY --from=mediawarp-builder --chmod=755 /tmp/mediawarp /usr/local/bin/mediawarp

VOLUME ["/config", "/logs", "/media", "/fonts"]
WORKDIR /

ENTRYPOINT ["/usr/local/bin/autofilm"]
