# WarpDrive - Next Generation Object Storage Engine

WarpDrive is a high-performance object storage engine written in Rust, built for storage disaggregated architectures and AI/ML training workloads. It implements a key-value/object store based on Facebook's Haystack paper with FlatBuffers serialization.

**ALWAYS follow these instructions first and fallback to search or bash commands only when you encounter unexpected information that does not match the info here.**

## Working Effectively

### Prerequisites (REQUIRED)
Install these dependencies BEFORE building:

```bash
# Install Rust (if not present)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env

# Install system dependencies 
sudo apt-get update && sudo apt-get install -y libsqlite3-dev flatbuffers-compiler
```

Verify installations:
```bash
rustc --version
cargo --version
flatc --version
```

### Initialize Repository
```bash
git clone --recurse-submodules https://github.com/vitality-ai/warpdrive.git
cd warpdrive
git submodule update --init --recursive
```

### Build the Rust Server
**CRITICAL TIMING INFO:**
- Initial build (with dependencies): 2.5 minutes - **NEVER CANCEL**. Set timeout to 180+ seconds.
- Clean build (dependencies cached): 45 seconds - **NEVER CANCEL**. Set timeout to 90+ seconds.
- Release build: 1.5 minutes - **NEVER CANCEL**. Set timeout to 120+ seconds.

```bash
cd server

# Debug build (for development)
cargo build
# Initial: ~2.5 min, Clean: ~45 sec. NEVER CANCEL - set timeout to 180+ seconds

# Release build (for production)
cargo build --release  
# Takes ~1.5 minutes. NEVER CANCEL - set timeout to 120+ seconds
```

### Run Tests
Tests complete quickly (~3 seconds):
```bash
cd server
cargo test
# Takes ~3 seconds - includes unit tests and integration tests
```

### Lint Code
**TIMING:** Clippy takes ~22 seconds - **NEVER CANCEL**
```bash
cd server
cargo clippy -- -A warnings
# Takes ~22 seconds. Reports only errors, not warnings
```

### Run the Application
```bash
cd server
cargo run
# Server starts on port 9710 (0.0.0.0:9710)
# Ready when you see: "starting service: actix-web-service-0.0.0.0:9710"
```

**Server Configuration:**
- **Port**: 9710 (hardcoded in main.rs)
- **Storage**: Uses SQLite database (./metadata/metadata.sqlite)
- **Data Format**: Requires FlatBuffers formatted requests
- **Headers**: All requests must include `User` header

## Validation

### Manual Testing Scenarios
**ALWAYS run these validation steps after making changes:**

1. **Build and Start Server:**
   ```bash
   cd server && cargo build && cargo run
   ```

2. **Verify Server Health:**
   - Server should start on port 9710
   - Look for log: "starting service: actix-web-service-0.0.0.0:9710"

3. **Test API Endpoints:**
   The server provides these REST endpoints (all require FlatBuffers format):
   - `POST /put/{key}` - Store data
   - `GET /get/{key}` - Retrieve data  
   - `POST /update/{key}` - Update existing data
   - `POST /append/{key}` - Append to existing data
   - `PUT /update_key/{old_key}/{new_key}` - Rename key
   - `DELETE /delete/{key}` - Delete data

4. **Python Client Testing:**
   ```bash
   cd client/python-sdk
   pip install -e .
   # Note: Demo client needs FlatBuffers format fixes
   ```

### CI Requirements
**ALWAYS run these before committing:**
```bash
cd server
cargo build      # Must succeed
cargo test       # All tests must pass  
cargo clippy -- -A warnings  # Must have no errors
```

## Docker Deployment (Optional)

**Note**: Docker build may fail due to network limitations in some environments.

```bash
# Build Docker image
docker build -t warpdrive .

# Run container  
docker run -p 9710:9710 warpdrive
```

**Docker Configuration:**
- Exposed port: 9710
- Data volume: `/data` 
- Database: `/data/metadata.sqlite`

## Common Issues and Solutions

### Build Failures
- **Missing FlatBuffers**: Install with `sudo apt-get install flatbuffers-compiler`
- **Missing SQLite**: Install with `sudo apt-get install libsqlite3-dev`
- **Submodules not initialized**: Run `git submodule update --init --recursive`

### Runtime Issues  
- **Port 9710 in use**: Kill existing process or change port in `server/src/main.rs`
- **Database permissions**: Ensure write access to `./metadata/` directory
- **FlatBuffers format errors**: Requests must be properly serialized FlatBuffers

### Test Failures
- **Database conflicts**: Tests create temporary databases, ensure cleanup
- **Port conflicts**: Stop running server before integration tests

## Codebase Navigation

### Key Directories
```
server/src/          # Rust server source code
├── lib.rs          # Main library exports
├── main.rs         # Application entry point  
├── service.rs      # HTTP handlers and business logic
├── api.rs          # REST API route definitions
├── storage.rs      # File storage layer
├── database.rs     # Legacy database interface  
├── metadata/       # Metadata storage abstraction
├── metadata_service.rs  # Metadata service wrapper
└── util/           # Utilities and serialization

server/tests/       # Integration tests
client/python-sdk/  # Python client library
demo/              # Demo applications
docs/              # Documentation
tla/               # TLA+ formal specifications
.github/workflows/ # CI/CD configuration
```

### Important Files to Check After Changes
- **After modifying server/src/service.rs**: Run integration tests
- **After modifying metadata**: Test both SQLite and Mock backends
- **After changing API contracts**: Update client SDKs
- **After storage changes**: Run metadata abstraction tests

### Metadata Storage
The system supports pluggable metadata backends:
- **SQLite** (default): `METADATA_BACKEND=sqlite` 
- **Mock** (testing): `METADATA_BACKEND=mock`

Test specific backends:
```bash
METADATA_BACKEND=sqlite cargo test metadata::
METADATA_BACKEND=mock cargo test metadata::
```

## Development Tips

### Quick Development Cycle
```bash
# 1. Build and test changes
cd server && cargo build && cargo test

# 2. Run linting
cargo clippy -- -A warnings

# 3. Start server for testing
cargo run

# 4. In another terminal, test endpoints
curl -H "User: testuser" http://localhost:9710/get/somekey
```

### Performance Testing
- **Load testing**: Use tools like wrk or Apache Bench against port 9710
- **Memory profiling**: Use valgrind or cargo flamegraph
- **Benchmark**: `cargo bench` (if benchmark tests exist)

### Debugging
- **Logs**: Check console output and log files in server directory
- **Database inspection**: SQLite database in `./metadata/metadata.sqlite`
- **Network debugging**: Use tcpdump or Wireshark on port 9710

**REMEMBER**: 
- **NEVER CANCEL** long-running builds (2.5+ minutes)
- **ALWAYS** run tests and linting before committing
- **Server expects FlatBuffers format** - raw HTTP requests will fail
- **Set appropriate timeouts** for all build operations (120+ seconds minimum)