# User Guide

> **Note:**  
> These instructions assume you have installed and are running the Storage Service locally.  
> For setup instructions, see the [Developer's Documentation](../docs/setup.md).  
> Once our cloud offering is available, we will update this guide with details for connecting to the managed service.

## Getting Started

Install the CIAOS Python client:
```bash
pip install ciaos
```

### Configuration

To use CIAOS, initialize a `Config` object with your details:

- **user_id**: Your user ID.
- **api_url**: The storage server URL.
- **user_access_key**: Your user access key.

Example:
```python
from ciaos import Ciaos, Config

config = Config(
    user_id="your_user_id",
    api_url="https://api.ciaos.com",
    user_access_key="xxxx"
)

ciaos_client = Ciaos(config)
```

---

## API Overview

CIAOS provides an interface for storing, retrieving, and managing binary data and files using unique keys.

### Main Methods

- `put`: Upload a file to the server.
- `put_binary`: Upload binary data with a unique key.
- `get`: Retrieve binary data by key.
- `update`: Replace the content of an existing key.
- `update_key`: Rename the identifier (key) of existing data.
- `delete`: Remove data by key.
- `append`: Add binary data to an existing key.

---

## Usage Examples

```python
# PUT: Upload a file. Uses the filename as the key if no key is provided.
ciaos_client.put(file_path="path/to/your/file.txt", key="optional_unique_key")

# PUT_BINARY: Upload binary data with a key.
ciaos_client.put_binary(key="unique_key", data_list=[b"file1_binary", b"file2_binary_data"])

# GET: Retrieve data by key.
data = ciaos_client.get(key="your_key")

# UPDATE: Replace the content at the given key.
ciaos_client.update(key="your_key", data_list=[b"file1_updated_data", b"file2_updated_data"])

# UPDATE_KEY: Change the identifier (key) for existing data.
ciaos_client.update_key(old_key="old_key", new_key="new_key")

# DELETE: Remove data by key.
ciaos_client.delete(key="your_key")

# APPEND: Add data to an existing key.
ciaos_client.append(key="your_key", data_list=[b"additional_data"])
```

---

For advanced usage and development details, see the [Developer's Documentation](../docs/setup.md).
