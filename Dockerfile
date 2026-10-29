FROM rust:1.88-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY assets ./assets
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN useradd --system --home /data --shell /usr/sbin/nologin --uid 10001 ait
COPY --from=build /src/target/release/ait /usr/local/bin/ait
COPY docker/entrypoint.sh /entrypoint.sh
RUN chmod 755 /entrypoint.sh
EXPOSE 7878
VOLUME /data
ENV AIT_BIND=0.0.0.0:7878
ENTRYPOINT ["/entrypoint.sh"]
