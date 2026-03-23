# time crate (transitive dep) requires rustc >= 1.88 as of time@0.3.47
FROM rust:1.88-slim-bookworm

# Install system dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    libsqlite3-dev \
    sqlite3 \
    && rm -rf /var/lib/apt/lists/*


# Set work directory
WORKDIR /server

ENV STORAGE_DIRECTORY /data
ENV DB_FILE /data/metadata.sqlite

RUN mkdir /data

# Copy project files
COPY server/ /server/

RUN cargo build --release

EXPOSE 9710

CMD ["/server/target/release/warp_drive"]