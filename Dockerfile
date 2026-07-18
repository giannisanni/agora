# agora hub. Build: docker build -t agora .
# Run:  docker run -d -p 8787:8787 -v agora-data:/data \
#         -e AGORA_ADDR=0.0.0.0:8787 -e AGORA_DB=/data/agora.db \
#         -e AGORA_INGEST_TOKEN=<secret> -e AGORA_ALLOWED_HOSTS=<your-host> agora
FROM rust:1.97-slim AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --bin agora

FROM debian:bookworm-slim
COPY --from=build /src/target/release/agora /usr/local/bin/agora
ENV AGORA_ADDR=0.0.0.0:8787 AGORA_DB=/data/agora.db
VOLUME /data
EXPOSE 8787
CMD ["agora"]
