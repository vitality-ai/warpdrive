# pull-one.sh <run dir on wlc>: tar it there (without per-request traces), copy it here, extract into the notes folder.
. "$(dirname "$0")/lib.sh"; N=~/cj/personal/claude/warpdrive-notes/rfc6-gcp-2026-11-22; r=$1
rs wlc "tar czf ~/$r.tgz --exclude=trace.csv -C ~ $r 2>/dev/null; ls -la ~/$r.tgz" || { echo "tar on wlc failed"; exit 1; }
scp -q $SSHO cj@$(ext wlc):$r.tgz $N/$r.tgz || { echo "copy failed"; exit 1; }
cd $N && rm -rf $r && tar xzf $r.tgz 2>/dev/null; ls $N/$r
