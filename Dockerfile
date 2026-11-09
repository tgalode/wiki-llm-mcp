FROM rust:1.99-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN CARGO_BUILD_JOBS=2 cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends git openssh-client ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --uid 1000 --home-dir /tmp --no-create-home wiki
COPY --from=build /src/target/release/wiki-llm-mcp /usr/local/bin/wiki-llm-mcp
USER 1000:1000
ENV HOME=/tmp
ENTRYPOINT ["wiki-llm-mcp"]
