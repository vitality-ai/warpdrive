. "$(dirname "$0")/lib.sh"
seen=""
while true; do
  out=$(rs wlc 'for f in comp oversize comp2 soak micro/getprof micro/scanstall micro/disk; do sed "s|^|$f |" ~/$f/progress.log 2>/dev/null; done | cut -c1-175' 2>/dev/null || true)
  while IFS= read -r l; do [ -n "$l" ] && ! grep -qxF "$l" <<<"$seen" && echo "$l"; done <<<"$out"
  seen="$out"
  grep -q "^micro/disk .*ALL DONE" <<<"$out" && exit 0
  sleep 60
done
