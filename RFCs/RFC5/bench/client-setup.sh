set -e
sudo apt-get -qq update >/dev/null && sudo DEBIAN_FRONTEND=noninteractive apt-get -qq install -y build-essential pkg-config libsqlite3-dev git python3-pip >/dev/null
curl -sSf https://sh.rustup.rs | sh -s -- -y -q --profile minimal >/dev/null 2>&1
. "$HOME/.cargo/env"
git clone -q --branch feat-rfc5-one-object-service https://github.com/vitality-ai/warpdrive.git ~/warpdrive
cd ~/warpdrive/server && cargo build --release --locked --bin warp_drive --bin s3_bench 2>&1 | tail -1
echo "$(hostname): built $(ls ~/warpdrive/server/target/release/warp_drive ~/warpdrive/server/target/release/s3_bench | wc -l) binaries"
