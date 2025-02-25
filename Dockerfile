FROM rust:1.73-slim-bullseye as builder

# Create a new directory for the app
WORKDIR /usr/src/app

# Install required dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/* \
    && apt-get clean

# Copy only dependency files first to leverage cache
COPY Cargo.toml Cargo.lock ./

# Copy actual source code
COPY src ./src

# Build the application
RUN cargo build --release

# Create the runtime image
FROM rust:1.73-slim-bullseye

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl1.1 \
    && rm -rf /var/lib/apt/lists/* \
    && apt-get clean

# Create a non-root user
RUN useradd -m -s /bin/bash -u 1000 appuser

# Create directory for the app
WORKDIR /app

# Copy the binary from the builder stage
COPY --from=builder /usr/src/app/target/release/appraisal /app/

# Set ownership
RUN chown -R appuser:appuser /app

# Switch to non-root user
USER appuser

# Set environment variables
ENV RUST_LOG=info \
    CKB_WS_URL= \
    MQTT_HOST= \
    MQTT_PORT= \
    MQTT_USERNAME= \
    MQTT_PASSWORD= \
    MQTT_CLIENT_ID=ckb-appraisal \
    CONCURRENT_REQUESTS=10

# Use exec form and full path
CMD ["/app/appraisal"]