set -e
. "$HOME/.cargo/env"
sudo DEBIAN_FRONTEND=noninteractive apt-get -qq install -y sysstat >/dev/null
cd ~/warpdrive && git fetch -q origin && git checkout -q feat-rfc5-one-object-service && git reset -q --hard origin/feat-rfc5-one-object-service
git worktree add -q -f ~/wd-main origin/main 2>/dev/null || (cd ~/wd-main && git checkout -q --detach origin/main)
mkdir -p ~/bins
(cd ~/warpdrive/server && CARGO_TARGET_DIR=$HOME/tgt-proto cargo build --release --locked --bin warp_drive --bin s3_bench 2>&1 | tail -1)
cp ~/tgt-proto/release/warp_drive ~/bins/wd-proto; cp ~/tgt-proto/release/s3_bench ~/bins/s3_bench
(cd ~/wd-main/server && CARGO_TARGET_DIR=$HOME/tgt-main cargo build --release --locked --bin warp_drive 2>&1 | tail -1)
cp ~/tgt-main/release/warp_drive ~/bins/wd-main
cp ~/warpdrive/server/server_log.yaml ~/bins/
cd ~/bins && md5sum wd-main wd-proto s3_bench && echo "proto has inline: $(strings wd-proto | grep -c WARPDRIVE_INLINE_MAX_BYTES) main has inline: $(strings wd-main | grep -c WARPDRIVE_INLINE_MAX_BYTES)"
echo "main commit: $(git -C ~/wd-main log --oneline -1)  proto commit: $(git -C ~/warpdrive log --oneline -1)"
