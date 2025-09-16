# Binary Storage Layer

This module provides an abstraction over binary storage backends, allowing the system to use different storage implementations (Local XFS, distributed file systems, etc.) without affecting higher-level services.

## Architecture

The binary storage layer consists of several components:

- **BinaryStorage trait**: Defines the interface for binary storage operations
- **Backend implementations**: LocalXFS and Mock implementations
- **Configuration system**: Environment-based backend selection
- **Integration with existing storage functions**: Seamless replacement of legacy file operations

## Available Backends

### LocalXFS Backend (Default)
- Uses local XFS-based file system for binary data storage
- Maintains backward compatibility with existing single binary file per user approach
- Suitable for single-node deployments
- Stores data in `.bin` files and deletion logs in `.json` files

### Mock Backend
- In-memory storage for testing purposes
- No persistence across restarts
- Useful for unit tests and development
- Supports all the same operations as LocalXFS but stores data in memory

## Configuration

The binary storage backend can be configured using the `BINARY_BACKEND` environment variable:

```bash
# Use LocalXFS backend (default)
export BINARY_BACKEND=localxfs

# Use Mock backend (for testing)
export BINARY_BACKEND=mock
```

If an invalid backend is specified, the system will fall back to LocalXFS with a warning.

## Storage Directory Configuration

For LocalXFS backend, the storage directory can be configured with the `STORAGE_DIRECTORY` environment variable:

```bash
# Custom storage directory
export STORAGE_DIRECTORY=/path/to/custom/storage
```

If not specified, the default location is `./storage`.

## Usage

The binary storage layer is automatically initialized when the application starts. The existing storage functions in `storage.rs` now use the binary storage abstraction:

```rust
// These functions now use the binary storage abstraction internally
write_files_to_storage(user, body)?;
get_files_from_storage(user, offset_size_list)?;
delete_and_log(user, key, offset_size_list)?;
```

## API Interface

The `BinaryStorage` trait provides the following methods:

- `put_object(user_id, object_id, data)` - Store a single object
- `get_object(user_id, object_id, offset, size)` - Retrieve a single object
- `delete_object(user_id, object_id, offset_size_list)` - Delete/log object deletion
- `verify_object(user_id, object_id, checksum)` - Verify object integrity (optional)
- `put_objects_batch(user_id, data_list)` - Store multiple objects efficiently
- `get_objects_batch(user_id, offset_size_list)` - Retrieve multiple objects efficiently

## Future Extensions

The abstraction is designed to support future distributed backends such as:
- CephFS for distributed object storage
- Lustre for high-performance computing workloads
- JuiceFS for cloud-native storage
- Custom distributed file layers
- Erasure coding and replication systems

To add a new backend:

1. Implement the `BinaryStorage` trait
2. Add the backend to `BinaryBackend` enum in `config.rs`
3. Update `BinaryConfig::create_store()` to handle the new backend
4. Add configuration parsing support

## Benefits

- **Decouples business logic from storage backend**: Higher-level services don't need to change when switching storage implementations
- **Enables future migration**: Can switch to distributed file systems without code changes
- **Facilitates testing**: Mock storage enables testing without disk I/O
- **Supports advanced features**: Foundation for replication, erasure coding, encryption
- **Configuration-driven**: Backend selection through environment variables
- **Backward compatibility**: Existing API unchanged, legacy code preserved

## Testing

Run binary storage specific tests:

```bash
cargo test binary::
```

Test specific backends:

```bash
# Test LocalXFS backend
BINARY_BACKEND=localxfs cargo test

# Test Mock backend  
BINARY_BACKEND=mock cargo test
```

## Legacy Compatibility

The original `OpenFile` and `DeleteFile` implementations are preserved (but unused) in `storage.rs` for reference and potential backward compatibility needs. The public API remains exactly the same, ensuring no breaking changes to existing code.