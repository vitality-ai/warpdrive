# Developer's Guide

## 🚀 Local Setup

Follow these steps to set up and run the storage server locally:

---

### 1. Prerequisites

Install the required dependencies first:

- **Rust**  
  [Install Rust](https://rustup.rs/) with:
  ```bash
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
  ```

- **libsqlite3-dev** & **FlatBuffers Compiler (`flatc`)**

  **For Ubuntu/Debian:**
  ```bash
  sudo apt-get update && sudo apt-get install -y libsqlite3-dev flatbuffers-compiler
  ```

  **For macOS (with Homebrew):**
  ```bash
  brew install sqlite3 flatbuffers
  ```

  **Manual FlatBuffers install (if needed):**
  ```bash
  git clone https://github.com/google/flatbuffers.git
  cd flatbuffers
  cmake .
  make
  sudo make install
  sudo ldconfig
  flatc --version
  ```

---

### 2. Clone the Repository

```bash
git clone https://github.com/cia-labs/ciaos.git
cd ciaos/server
```

---

### 3. Build the Rust Server

```bash
cargo build
```

---

### 4. Run the Application

```bash
cargo run
```

---

## 🐳 Docker (Optional)

Deploy the storage service in a Docker container:

**Build the Docker image:**
```bash
docker build -t ciaos .
```

**(Optional) Change the exposed port:**
- Update the port in `server/src/main.rs` (e.g., `9710`)
- Update the `EXPOSE` line in the Dockerfile (e.g., `EXPOSE 9710`)

**Run the Docker container:**
```bash
docker run -p 9710:9710 ciaos
```

---

## 💡 Troubleshooting

- Ensure all prerequisites are installed and available in your PATH.
- For advanced help, see [Rust docs](https://doc.rust-lang.org/book/) or [FlatBuffers docs](https://google.github.io/flatbuffers/).

---

Happy Hacking! 🚀
