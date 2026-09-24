# SPDX-FileCopyrightText: 2026 Ryan Moore
# SPDX-License-Identifier: Apache-2.0
#
# docker build -t sni_router .
# docker run -v $PWD/examples/sni_router.toml:/etc/sni_router/sni_router.toml \
#   -p 8080:8080 -p 8081:8081 sni_router

# Base images are pinned by digest (Dependabot keeps them current).
FROM rust:1-bookworm@sha256:93ce27a88655056a51dbdd8f5f2d7ddc071c7b0070fb288a37b5a285fc83971e AS builder
WORKDIR /src
COPY . .
RUN cargo build --release --locked --bin sni_router

FROM gcr.io/distroless/cc-debian12:nonroot@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f
COPY --from=builder /src/target/release/sni_router /usr/local/bin/sni_router
ENV SNI_ROUTER_CONFIG=/etc/sni_router/sni_router.toml
# 8080: TLS passthrough. 8081: admin (/metrics, /health) — set
# [admin].listen = "0.0.0.0:8081" to expose it outside the container.
EXPOSE 8080 8081
ENTRYPOINT ["/usr/local/bin/sni_router"]
