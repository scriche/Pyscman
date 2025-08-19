FROM rust:latest as builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs
RUN cargo build --release && rm -f target/release/deps/pyscman*
COPY . .
RUN cargo build --release
FROM debian:stable-slim
RUN apt-get update && apt-get upgrade -y && apt-get install -y python3 python3-pip python3-venv && rm -rf /var/lib/apt/lists/*
RUN useradd -m pyscman
WORKDIR /app
COPY --from=builder /app/target/release/Pyscman /app/Pyscman
COPY --from=builder /app/src/index.html /app/index.html
RUN chown -R pyscman:pyscman /app
USER pyscman
EXPOSE 8080
ENV PYSCMAN_BIND=0.0.0.0:8080
CMD ["/app/Pyscman"]
