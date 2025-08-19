FROM debian:stable-slim

ARG BINARY_PATH
RUN apt-get update && apt-get install -y python3 python3-pip python3-venv \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Copy prebuilt binary
COPY ${BINARY_PATH} /app/Pyscman
COPY src/index.html /app/index.html

RUN chown -R pyscman:pyscman /app

EXPOSE 8080
ENV PYSCMAN_BIND=0.0.0.0:8080
CMD ["/app/Pyscman"]
