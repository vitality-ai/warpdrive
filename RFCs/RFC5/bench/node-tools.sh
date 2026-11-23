sudo DEBIAN_FRONTEND=noninteractive apt-get -qq install -y sysstat linux-tools-$(uname -r) linux-tools-gcp >/dev/null 2>&1
mkdir -p ~/bins; echo "$(hostname): sar=$(which sar >/dev/null && echo ok) iostat=$(which iostat >/dev/null && echo ok) perf=$(which perf >/dev/null && echo ok)"
