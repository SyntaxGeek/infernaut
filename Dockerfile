FROM rust:1.89-bookworm AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked --bins

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 10001 --create-home infernaut \
    && mkdir /config && chown infernaut:infernaut /config
COPY --from=build /build/target/release/infernaut /build/target/release/infernaut-sniffer /usr/local/bin/
USER infernaut
ENV HOME=/home/infernaut
WORKDIR /config
EXPOSE 4040
ENTRYPOINT ["infernaut"]
CMD ["/config/infernaut.toml"]
