# CIAOS - Next Generation Object Storage Engine

<img src="https://github.com/user-attachments/assets/654f3add-74ab-4c34-8b73-234852ea11c7" alt="Example Image" width="800"  height="250">


## About
CIAOS is a general purpose KV/Object store focused on workloads that require high throughput. Practical Applications which drives our developemt is to support Storage Disaggregation Architectures and AI/ML Workloads . Our current implementation of the object store is based on Facebook's 2008 haystack paper and our road map for our future versions will be driven the next generation's storage needs with solid fundamental understanding of the history of these storage systems [Technical Roadmap](https://github.com/vitality-ai/Storage-service/blob/main/Technical-Architecture.md). 

## System Offerings that are currently being built. 
1. Storage - Key/Value, Files and Blobs. 
2. Fault Tolerance - Uses Erasure Coding to Optimise Data replication - Seeks contribution for design - [Discussion](https://github.com/cia-labs/Storage-service/issues/72)
3. User Access Management - [Discussion](https://github.com/cia-labs/Storage-service/issues/36)
4. Search - Seeks contribution for design. -   [Discussion](https://github.com/cia-labs/Storage-service/issues/35)
5. Availability - Seeks contribution for design. [Discussion]()
6. Client Library - Client package is currently available for Python only.

---



## Getting Started

```bash
pip install ciaos
```

#### Configuration
To use CIAOS, you need to initialize a `Config` object with the necessary details:

- **`user_id`**: Your user ID .
- **`api_url`**: The URL of the storage server.
- **`user_access_key`**: Your user access key for authentication.

Here's how to set up and use the library:

```python
from ciaos import Ciaos,Config

# Initialize Config
config = Config(
    user_id="your_user_id", 
    api_url="https://api.ciaos.com",
    user_access_key="xxxx"
)

# Initialize CIAOS Client
ciaos_client = Ciaos(config)
```

---
### Functions and Usage

#### **Functionality**

- **`PUT`**: Uploads a file to the server using a unique key. If no key is provided, the filename is used as the key.

- **`PUT_BINARY`**: Uploads binary data to the server, accepting a list of binary data and associating it with a unique key.

- **`GET`**: Retrieves the binary data associated with a specific key.

- **`UPDATE`**: Updates the content of an existing key with new binary data, replacing the existing data.

- **`UPDATE_KEY`**: Renames the identifier (key) of existing data without changing the associated content.

- **`DELETE`**: Removes data associated with a specific key from the server.

- **`APPEND`**: Appends new binary data to an existing key without replacing the existing content.

---

### **Function Usage Examples**

#### **PUT**
- Upload a file to the server. Use the filename as the key if no `key` is provided.

```python
ciaos_client.put(file_path="path/to/your/file.txt", key="optional_unique_key")
```

#### **PUT_BINARY**
- Upload binary data to the server with a unique key.

```python
ciaos_client.put_binary(key="unique_key", data_list=[b"file1_binary", b"file2_binary_data"])
```

#### **GET**
- Retrieve the binary data associated with a specific key.

```python
data = ciaos_client.get(key="your_key")
```

#### **UPDATE**
- Replace the content of an existing key with new binary data.

```python
ciaos_client.update(key="your_key", data_list=[b"file1_updated_data", b"file2_updated_data"])
```

#### **UPDATE_KEY**
- Change the identifier (key) of existing data while retaining the associated content.

```python
ciaos_client.update_key(old_key="old_key", new_key="new_key")
```

#### **DELETE**
- Remove data associated with a specific key.

```python
ciaos_client.delete(key="your_key")
```

#### **APPEND**
- Add new data to an existing key without replacing the current content.

```python
ciaos_client.append(key="your_key", data_list=[b"additional_data"])
```

## Developer's Corner
For more advanced usage and development details, visit the [Developer's Documentation](https://github.com/cia-labs/Storage-service/blob/main/docs.md).
