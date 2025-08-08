# Developer's Guide

## Local Setup  
**Follow these steps to set up and run the storage server locally.**  

### Prerequisites  

- **Rust**  
- **libsqlite3-dev**  
- **FlatBuffers Compiler (`flatc`)**  

To install the FlatBuffers compiler (`flatc`), execute the following commands:  

```bash
git clone https://github.com/google/flatbuffers.git
cd flatbuffers
cmake
make
./flattests # Quick test; should print "ALL TESTS PASSED"
sudo make install # Install FlatBuffers
sudo ldconfig # Configure the dynamic linker
flatc --version # Verify FlatBuffers installation
```  

---

### Clone the Repository  

```bash
git clone https://github.com/cia-labs/Storage-service.git
cd ciaos/server
```  

---

### Building the Rust Server  

```bash
cargo build
```  

---

### Running the Application  

```bash
cargo run
```  

---

# Deploying the Storage Service as a Docker Container  

To deploy the storage service as a container, follow these steps:  

### Build the Docker Image  

Use the provided [Dockerfile](https://github.com/cia-labs/Storage-service/blob/main/Dockerfile) in the GitHub repository to build the image:  

```bash
docker build -t ciaos .
```  
---
### (Optional) Modifying the Exposed Port   

If you wish to change the exposed port:  

1. Update the **bind address** in `Storage-service/server/src/main.rs`:  

```rust
.bind(("0.0.0.0", 9710))? // Default is port 9710
```  

2. Modify the `EXPOSE` directive in the Dockerfile to reflect the new port:  

```dockerfile
# Change the exposed port
EXPOSE 9710
```  

### Run the Docker Container  

```bash
docker run -p 9710:9710 ciaos
```  

The storage service should now be accessible on the specified port.
