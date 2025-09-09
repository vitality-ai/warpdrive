# Metadata Storage Layer

This module provides an abstraction over metadata storage backends, allowing the system to use different storage implementations (SQLite, distributed databases, etc.) without affecting higher-level services.

## Architecture

The metadata storage layer consists of several components:

- **MetadataStorage trait**: Defines the interface for metadata operations
- **Metadata struct**: Represents object metadata including data chunks and properties
- **Backend implementations**: SQLite and Mock implementations
- **Configuration system**: Environment-based backend selection
- **MetadataService**: Bridge layer that maintains compatibility with existing Database interface

## Available Backends

### SQLite Backend (Default)
- Uses local SQLite database for metadata storage
- Maintains backward compatibility with existing schema
- Suitable for single-node deployments

### Mock Backend
- In-memory storage for testing purposes
- No persistence across restarts
- Useful for unit tests and development

## Configuration

The metadata backend can be configured using the `METADATA_BACKEND` environment variable:

```bash
# Use SQLite backend (default)
export METADATA_BACKEND=sqlite

# Use Mock backend (for testing)
export METADATA_BACKEND=mock
```

If an invalid backend is specified, the system will fall back to SQLite with a warning.

## Database Configuration

For SQLite backend, the database location can be configured with the `DB_FILE` environment variable:

```bash
# Custom database location
export DB_FILE=/path/to/custom/metadata.sqlite
```

If not specified, the default location is `./metadata/metadata.sqlite`.

## Usage

The metadata storage layer is automatically initialized when the application starts. Services use the `MetadataService` which provides a Database-compatible interface:

```rust
use crate::metadata_service::MetadataService;

let service = MetadataService::new("user_id")?;
service.check_key("object_key")?;
// ... other operations
```

## Future Extensions

The abstraction is designed to support future distributed backends such as:
- CockroachDB for distributed SQL
- Cassandra for NoSQL scalability
- Custom distributed metadata services
- Distributed consensus systems

To add a new backend:

1. Implement the `MetadataStorage` trait
2. Add the backend to `MetadataBackend` enum in `config.rs`
3. Update `MetadataConfig::create_store()` to handle the new backend
4. Add configuration parsing support

## Testing

Run metadata-specific tests:

```bash
cargo test metadata::
```

Test specific backends:

```bash
# Test SQLite backend
METADATA_BACKEND=sqlite cargo test

# Test Mock backend  
METADATA_BACKEND=mock cargo test
```