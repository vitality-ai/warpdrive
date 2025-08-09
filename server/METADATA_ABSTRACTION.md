# Metadata Storage Abstraction Layer (META)

This document describes the metadata storage abstraction layer implemented in CIAOS.

## Overview

The Metadata Storage layer manages object metadata, including mappings from object keys to their binary locations (offsets, sizes, checksums). This abstraction allows swapping between different metadata storage backends without affecting higher-level services.

## Architecture

### Core Components

1. **MetadataStorage Trait** (`src/metadata.rs`)
   - Abstract interface for all metadata storage backends
   - Defines operations: put, get, delete, exists, update, list

2. **SQLiteMetadataStore** (`src/sqlite_store.rs`) 
   - Default implementation using SQLite database
   - Maintains backward compatibility with existing schema

3. **MockMetadataStore** (`src/mock_store.rs`)
   - In-memory implementation for testing
   - Useful for unit tests and development

4. **Configuration** (`src/config.rs`)
   - Environment-based backend selection
   - Easy switching between implementations

5. **Legacy Database** (`src/database.rs`)
   - Backward compatibility wrapper
   - Preserves existing API for service layer

## Usage

### Using the Abstraction

```rust
use crate::metadata::{MetadataStorage, Metadata};
use crate::config::MetadataConfig;

// Create a metadata store from configuration
let config = MetadataConfig::from_env();
let store = config.create_metadata_store();

// Store metadata
let metadata = Metadata::new(vec![(0, 100), (100, 200)]);
store.put_metadata("user123", "object456", &metadata)?;

// Retrieve metadata
let retrieved = store.get_metadata("user123", "object456")?;
```

### Configuration

Set the `METADATA_BACKEND` environment variable to choose the backend:

```bash
# Use SQLite (default)
export METADATA_BACKEND=sqlite

# Use Mock implementation
export METADATA_BACKEND=mock
```

### Backward Compatibility

Existing code continues to work unchanged:

```rust
use crate::database::Database;

let db = Database::new("user123")?;
db.upload_sql("key", &offset_size_bytes)?;
let data = db.get_offset_size_lists("key")?;
```

## Metadata Format

The `Metadata` struct encapsulates object location information:

```rust
pub struct Metadata {
    pub offset_size_list: Vec<(u64, u64)>, // (offset, size) pairs
}
```

This format is:
- **Serializable**: Can be stored as binary data
- **Portable**: Independent of storage backend
- **Extensible**: Easy to add fields in the future

## Future Extensions

The abstraction enables future implementations:

- **Distributed Databases**: CockroachDB, Cassandra
- **Key-Value Stores**: Redis, etcd
- **Cloud Services**: DynamoDB, Cosmos DB
- **Custom Solutions**: Distributed metadata/data planes

## Testing

Run tests to verify all implementations:

```bash
# Run all tests
cargo test

# Run specific test suites
cargo test mock_store
cargo test integration_tests
```

## Migration Path

1. **Phase 1**: ✅ Abstraction layer implemented with backward compatibility
2. **Phase 2**: Gradual migration of service layer to use trait directly
3. **Phase 3**: Add distributed backend implementations
4. **Phase 4**: Remove legacy Database wrapper when no longer needed

## Benefits

- **Flexibility**: Easy backend switching without code changes
- **Testing**: Mock implementation for reliable unit tests
- **Scalability**: Ready for distributed metadata storage
- **Maintainability**: Clear separation of concerns
- **Future-proof**: Extensible design for new requirements