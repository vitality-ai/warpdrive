O=/private/tmp/claude-502/-Users-cj-cj-personal-claude/d590bb8b-7213-4abc-b5c3-330a952fc604/scratchpad/gcp-cores
SSHO="-i $HOME/.ssh/google_compute_engine -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=15 -o ServerAliveInterval=15 -o LogLevel=ERROR"
ext() { awk -v n="$1" '$1==n{print $3}' $O/vms.txt; }
rs() { local h=$1; shift; ssh $SSHO -n cj@$(ext $h) "$@"; }
rcp() { local dest=${@: -1}; local h=${dest%%:*} p=${dest#*:}; scp -q $SSHO "${@:1:$#-1}" cj@$(ext $h):$p; }
rget() { local h=${1%%:*} p=${1#*:}; scp -q $SSHO cj@$(ext $h):$p "$2"; }
