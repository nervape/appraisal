FROM rust:1.73-slim-bullseye as builder

# Create a new directory for the app
WORKDIR /usr/src/app

# Install required dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Copy the Cargo files first to cache dependencies
COPY Cargo.toml Cargo.lock ./

# Create a dummy main.rs to build dependencies
RUN mkdir src && \
    echo "fn main() {}" > src/main.rs && \
    cargo build --release && \
    rm -rf src/

# Copy the actual source code
COPY src ./src

# Build the application
RUN cargo build --release

# Create the runtime image
FROM debian:bullseye-slim

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl1.1 \
    && rm -rf /var/lib/apt/lists/*

# Copy the binary from the builder stage
COPY --from=builder /usr/src/app/target/release/appraisal /usr/local/bin/

# Create a non-root user
RUN useradd -m appuser
USER appuser

# Set environment variables
ENV RUST_LOG=info
ENV CKB_WS_URL
ENV MQTT_HOST
ENV MQTT_PORT
ENV MQTT_CLIENT_ID=ckb-appraisal
ENV CONCURRENT_REQUESTS=10

CMD ["appraisal"]
