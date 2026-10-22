FROM rust:1.99-bookworm AS build
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates ffmpeg \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /app/target/release/emby-bridge /usr/local/bin/emby-bridge
RUN mkdir -p /data /media && chown -R 10001:10001 /data /media
USER 10001:10001
EXPOSE 8096 8097
ENTRYPOINT ["/usr/local/bin/emby-bridge"]
