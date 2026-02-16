# Use Rust official image as base (1.82+ not supported by some deps; use 1.84+)
FROM rust:1.84-slim-bookworm

# Install system dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libsqlite3-dev \
    && rm -rf /var/lib/apt/lists/*


# Set work directory
WORKDIR /server

ENV STORAGE_DIRECTORY /data
ENV DB_FILE /data/metadata.sqlite

RUN mkdir /data

# Copy project files
COPY server/ /server/

# Build the application
RUN cargo build 

# Expose port
EXPOSE 9710

# Run the server
CMD ["cargo", "run"]