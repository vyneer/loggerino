FROM lukemathwalker/cargo-chef:latest-rust-slim-bullseye as planner
LABEL builder=true multistage_tag="loggerino-planner"
WORKDIR /app
COPY . .
RUN cargo chef prepare  --recipe-path recipe.json

FROM lukemathwalker/cargo-chef:latest-rust-slim-bullseye as builder
LABEL builder=true multistage_tag="loggerino-builder"
WORKDIR /app
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
RUN apt-get update && apt-get install -y protobuf-compiler
COPY . .
RUN cargo build --release --bin loggerino

FROM debian:bullseye-slim
WORKDIR /app
COPY --from=builder /app/target/release/loggerino .
CMD ["./loggerino"]