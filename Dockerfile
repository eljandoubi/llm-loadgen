FROM rust:1-slim AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /src/target/release/llm-loadgen /llm-loadgen
USER nonroot
ENTRYPOINT ["/llm-loadgen"]
CMD ["--help"]
