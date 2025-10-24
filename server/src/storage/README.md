# Binary Storage Layer

This module provides an abstraction over binary storage backends, allowing the system to use different storage implementations (local files, distributed file systems, etc.) without affecting higher-level services.

## Architecture

The binary storage layer consists of several components:

- **Storage trait**: Defines the interface for binary storage operations
- **Backend implementations**: LocalXFS and Mock storage backends
- **Configuration system**: Environment-based backend selection
- **Legacy API compatibility**: Maintains backward compatibility with existing storage functions

## Available Backends

### LocalXFS Backend (Default)
- Uses local XFS-backed binary files for storage (one file per user)
- Maintains offset/size semantics for direct file access
- Suitable for single-node deployments
- Preserves original append-only behavior

### Mock Backend
- In-memory storage for testing purposes
- No persistence across restarts
- Simulates offset/size behavior for compatibility
- Useful for unit tests and development

## Configuration

The storage backend can be configured using the `STORAGE_BACKEND` environment variable:

```bash
# Use LocalXFS backend (default)
export STORAGE_BACKEND=localxfs

# Use Mock backend (for testing)
export STORAGE_BACKEND=mock
```

Accepted values for `STORAGE_BACKEND`:
- `localxfs`, `local`, or `xfs` - LocalXFS backend
- `mock` - Mock backend

If an invalid backend is specified, the system will fall back to LocalXFS with a warning.

## Storage Directory Configuration

For LocalXFS backend, the storage directory can be configured with the `STORAGE_DIRECTORY` environment variable:

```bash
# Custom storage directory
export STORAGE_DIRECTORY=/path/to/custom/storage
```

If not specified, the default location is `./storage`.

## Usage

The storage layer provides both high-level object-oriented and low-level offset/size interfaces:

### High-Level Object Interface

```rust
use crate::storage::{Storage, config::StorageConfig};

let config = StorageConfig::from_env();
let store = config.create_store();

// Store an object
store.put_object("user_id", "object_id", b"data")?;

// Retrieve an object
let data = store.get_object("user_id", "object_id")?;

// Delete an object
store.delete_object("user_id", "object_id")?;

// Verify object integrity
let checksum = calculate_checksum(&data);
let is_valid = store.verify_object("user_id", "object_id", &checksum)?;
```

### Low-Level Offset/Size Interface

```rust
// Write data and get offset/size (legacy compatibility)
let (offset, size) = store.write_data("user_id", b"data")?;

// Read data from specific offset/size
let data = store.read_data("user_id", offset, size)?;

// Log deletion
store.log_deletion("user_id", "key", &[(offset, size)])?;
```

### Legacy API Functions

The module provides backward-compatible functions that match the original storage API:

```rust
use crate::storage::{write_files_to_storage, get_files_from_storage};

// Process flatbuffer data and write files
let offset_size_list = write_files_to_storage("user", &flatbuffer_data)?;

// Retrieve files as flatbuffer
let flatbuffer_data = get_files_from_storage("user", offset_size_list)?;

// Delete and log files
// Delete is now handled through StorageService
```

## Storage Trait Interface

The `Storage` trait defines the complete interface:

```rust
pub trait Storage: Send + Sync {
    // Low-level offset/size interface (legacy compatibility)
    fn write_data(&self, user_id: &str, data: &[u8]) -> Result<(u64, u64), Error>;
    fn read_data(&self, user_id: &str, offset: u64, size: u64) -> Result<Vec<u8>, Error>;
    fn log_deletion(&self, user_id: &str, key: &str, offset_size_list: &[(u64, u64)]) -> Result<(), Error>;
    
    // High-level object interface
    fn put_object(&self, user_id: &str, object_id: &str, data: &[u8]) -> Result<(), Error>;
    fn get_object(&self, user_id: &str, object_id: &str) -> Result<Vec<u8>, Error>;
    fn delete_object(&self, user_id: &str, object_id: &str) -> Result<(), Error>;
    fn verify_object(&self, user_id: &str, object_id: &str, checksum: &[u8]) -> Result<bool, Error>;
}
```

## Future Extensions

The abstraction is designed to support future distributed backends such as:
- CephFS for distributed block storage
- Lustre for high-performance computing environments
- JuiceFS for cloud-native object storage
- Custom distributed file layers with replication
- Amazon S3 or compatible object storage
- Distributed hash tables (DHT) for peer-to-peer storage

To add a new backend:

1. Implement the `Storage` trait
2. Add the backend to `StorageBackend` enum in `config.rs`
3. Update `StorageConfig::create_store()` to handle the new backend
4. Add configuration parsing support

## Design Principles

- **Backward Compatibility**: Legacy API functions are preserved
- **Performance**: Direct offset/size access for efficient operations
- **Abstraction**: Clean interface separation allows backend swapping
- **Testing**: Mock backend enables thorough testing without I/O
- **Configuration**: Environment-based backend selection for flexibility

## Testing

Run storage-specific tests:

```bash
cargo test storage::
```

Test specific backends:

```bash
# Test LocalXFS backend
STORAGE_BACKEND=localxfs cargo test

# Test Mock backend  
STORAGE_BACKEND=mock cargo test
```

Test with custom storage directory:

```bash
STORAGE_DIRECTORY=/tmp/test_storage cargo test storage::
```