#!/bin/zsh
# usage: store_open.sh <material.flac>
#
# What a launch costs when the render-product store is large.
#
# env: AUDEC_BIN            the build under test
#      AUDEC_BIN_BEFORE     optional baseline build, measured on the same store
#      AUDEC_LIVE_DIR, AUDEC_CONTROL_SOCKET, AUDEC_CACHE_ROOT (fresh store)
#      AUDEC_BIG_CACHE_ROOT big store (default $TMPDIR/audec-cache-storeopen-big)
#      STORE_RECEIPTS       receipts to generate if the big store is missing
#
# The claim: launch -> socket and launch -> ready on a store of 100k receipts
# are within a second of the same launch on a fresh store, and a tile published
# into that store is still found by the next launch (on demand, without a walk).
source ${0:A:h}/common.sh
zmodload zsh/datetime
MATERIAL=${1:?material path}
RECEIPTS=${STORE_RECEIPTS:-100000}
BIG=${AUDEC_BIG_CACHE_ROOT:-${TMPDIR:-/tmp}/audec-cache-storeopen-big}
FRESH=${AUDEC_CACHE_ROOT:-${TMPDIR:-/tmp}/audec-cache-storeopen}
BIN=${AUDEC_BIN:-$REPO/target/debug/audec}
BOUND=${LAUNCH_BOUND:-1200}
NAMESPACE=render-request-v1

count_files() { find $1 -type f 2>/dev/null | wc -l | tr -d ' '; }

ensure_big_store() {
  if [ -f $BIG/render-products/refs/$NAMESPACE/.mark-complete ]; then
    echo "big store present: $(count_files $BIG) files under $BIG"
    return 0
  fi
  echo "generating a store of $RECEIPTS receipts under $BIG (minutes)"
  (cd $REPO && CARGO_INCREMENTAL=0 \
    AUDEC_STORE_FILL_ROOT=$BIG/render-products \
    AUDEC_STORE_FILL_RECEIPTS=$RECEIPTS \
    AUDEC_STORE_FILL_INDEX=1 AUDEC_STORE_FILL_WORKERS=8 \
    cargo test --lib -- render_tiles::tests::fill_a_store_for_measurement \
      --ignored --nocapture --test-threads=1 | tail -2)
}

# Launch one instance and measure it. Never touches a process it did not start.
# Leaves the pid in LAUNCHED_PID and the two times in SOCKET_S / READY_S.
timed_launch() {
  local label=$1 bin=$2 cache=$3
  rm -f $AUDEC_CONTROL_SOCKET
  local start=$EPOCHREALTIME
  (cd $REPO && AUDEC_CACHE_ROOT=$cache RUST_BACKTRACE=1 nohup $bin "$MATERIAL" \
     > $LIVE/$label.log 2>&1 &; echo $! > $LIVE/$label.pid)
  LAUNCHED_PID=$(cat $LIVE/$label.pid)
  SOCKET_S="none"; READY_S="none"; RSS_MB="none"
  local waited=0
  while (( waited < BOUND * 10 )); do
    if [ -S $AUDEC_CONTROL_SOCKET ]; then
      SOCKET_S=$(printf "%.2f" $((EPOCHREALTIME - start))); break
    fi
    kill -0 $LAUNCHED_PID 2>/dev/null || { echo "$label: the process exited before binding"; tail -3 $LIVE/$label.log; return 1; }
    sleep 0.1; (( waited += 1 ))
  done
  if [ "$SOCKET_S" = "none" ]; then
    echo "$label: did not bind its control socket within ${BOUND}s"
    echo "$label: socket none  ready none  (pid $LAUNCHED_PID)"
    return 0
  fi
  waited=0
  while (( waited < BOUND )); do
    ctl '{"op":"status"}' 2>/dev/null | grep -q '"state": "ready"' && {
      READY_S=$(printf "%.2f" $((EPOCHREALTIME - start))); break; }
    kill -0 $LAUNCHED_PID 2>/dev/null || { echo "$label: the process exited before ready"; tail -3 $LIVE/$label.log; return 1; }
    sleep 1; (( waited += 1 ))
  done
  RSS_MB=$(( $(ps -o rss= -p $LAUNCHED_PID | tr -d ' ') / 1024 ))
  echo "$label: socket ${SOCKET_S}s  ready ${READY_S}s  rss ${RSS_MB}MB  (pid $LAUNCHED_PID)"
}

stop_launched() { [ -n "$LAUNCHED_PID" ] && kill $LAUNCHED_PID 2>/dev/null; sleep 1; kill -9 $LAUNCHED_PID 2>/dev/null; LAUNCHED_PID=""; }

store_line() {
  ctl '{"op":"status"}' 2>/dev/null | python3 -c '
import sys, json
r = json.loads(sys.stdin.readline())["result"]
store = r.get("store", {})
print("  store:", json.dumps(store, sort_keys=True))
print("  memory.tile_cache_receipts:", r.get("memory", {}).get("tile_cache_receipts"))'
}

# One export of the same span, timed. Prints what the app said it did.
timed_export() {
  local label=$1 out=$2
  ctl '{"op":"select","start":2646000,"end":2998800}' '{"op":"loop","start":2646000,"end":2998800}' >/dev/null
  sleep 2
  local start=$EPOCHREALTIME
  ctl "{\"op\":\"export\",\"path\":\"$out\"}" >/dev/null
  local i
  for i in {1..600}; do
    local st=$(ctl '{"op":"status"}' 2>/dev/null)
    echo "$st" | grep -q 'EXPORTED' && break
    echo "$st" | grep -q 'FILE ERROR' && { echo "$label export failed: $st"; return 1; }
    sleep 1
  done
  echo "$label export: $(printf "%.1f" $((EPOCHREALTIME - start)))s  $(shasum -a 256 $out | cut -c1-16)  $(stat -f%z $out) bytes"
}

echo "=== material: $MATERIAL"
echo "=== binary under test: $BIN"
rm -rf $FRESH
ensure_big_store
BIG_FILES_BEFORE=$(count_files $BIG)
echo "big store: $BIG_FILES_BEFORE files"

echo
echo "=== 1. launch on a fresh store"
timed_launch fresh-after $BIN $FRESH || exit 1
store_line
FRESH_SOCKET=$SOCKET_S; FRESH_READY=$READY_S; FRESH_RSS=$RSS_MB
stop_launched

echo
echo "=== 2. launch on the big store ($RECEIPTS receipts)"
timed_launch big-after $BIN $BIG || exit 1
store_line
BIG_SOCKET=$SOCKET_S; BIG_READY=$READY_S; BIG_RSS=$RSS_MB
echo "--- publish tiles into the big store, then find them again after a restart"
timed_export "big-cold" $LIVE/big_cold.wav
echo "  $(grep -o 'incremental bounce:.*' $LIVE/big-after.log | tail -1)"
store_line
stop_launched

echo
echo "=== 3. relaunch on the big store: the tiles it just published are found on demand"
timed_launch big-warm $BIN $BIG || exit 1
timed_export "big-warm" $LIVE/big_warm.wav
echo "  $(grep -o 'incremental bounce:.*' $LIVE/big-warm.log | tail -1)"
store_line
echo "  cold vs warm export bytes: $(shasum -a 256 $LIVE/big_cold.wav | cut -c1-16) $(shasum -a 256 $LIVE/big_warm.wav | cut -c1-16)"
stop_launched

echo
echo "=== 3b. control: the same store with the request index deleted must render again"
# The mark stays, so no pass rebuilds it: every hydrate misses and the export
# has to render what it just proved it could find. If this is not slower than
# step 3, step 3 was not hydrating.
rm -rf $BIG/render-products/refs/$NAMESPACE/[0-9a-f][0-9a-f]
timed_launch big-blind $BIN $BIG || exit 1
timed_export "big-blind" $LIVE/big_blind.wav
store_line
stop_launched

echo
echo "=== 4. the same big store with its index mark removed: the walk is off the launch path"
rm -f $BIG/render-products/refs/$NAMESPACE/.mark-complete
timed_launch big-reindex $BIN $BIG || exit 1
echo "  at ready:"; store_line
for i in {1..600}; do
  ctl '{"op":"status"}' 2>/dev/null | grep -q '"state": "ready"' || break
  ctl '{"op":"status"}' 2>/dev/null | python3 -c '
import sys, json
print(json.loads(sys.stdin.readline())["result"].get("store", {}).get("index", {}).get("state"))' | grep -q complete && break
  sleep 2
done
echo "  after the index pass:"; store_line
REINDEX_SOCKET=$SOCKET_S; REINDEX_READY=$READY_S
stop_launched

if [ -n "$AUDEC_BIN_BEFORE" ]; then
  echo
  echo "=== 5. the same two launches with the baseline build ($AUDEC_BIN_BEFORE)"
  rm -rf $FRESH
  timed_launch fresh-before $AUDEC_BIN_BEFORE $FRESH
  BEFORE_FRESH_SOCKET=$SOCKET_S; BEFORE_FRESH_READY=$READY_S; BEFORE_FRESH_RSS=$RSS_MB
  stop_launched
  echo "big store now: $(count_files $BIG) files (was $BIG_FILES_BEFORE)"
  timed_launch big-before $AUDEC_BIN_BEFORE $BIG
  BEFORE_BIG_SOCKET=$SOCKET_S; BEFORE_BIG_READY=$READY_S; BEFORE_BIG_RSS=$RSS_MB
  stop_launched
  echo "big store after the baseline adopted it: $(count_files $BIG) files"
fi

echo
echo "=== launch cost, debug build, bound ${BOUND}s"
printf "%-28s %10s %10s %8s\n" "" "socket(s)" "ready(s)" "rss(MB)"
printf "%-28s %10s %10s %8s\n" "after  · fresh store" $FRESH_SOCKET $FRESH_READY $FRESH_RSS
printf "%-28s %10s %10s %8s\n" "after  · $RECEIPTS receipts" $BIG_SOCKET $BIG_READY $BIG_RSS
printf "%-28s %10s %10s %8s\n" "after  · big, no index yet" $REINDEX_SOCKET $REINDEX_READY "-"
if [ -n "$AUDEC_BIN_BEFORE" ]; then
printf "%-28s %10s %10s %8s\n" "before · fresh store" $BEFORE_FRESH_SOCKET $BEFORE_FRESH_READY $BEFORE_FRESH_RSS
printf "%-28s %10s %10s %8s\n" "before · $RECEIPTS receipts" $BEFORE_BIG_SOCKET $BEFORE_BIG_READY $BEFORE_BIG_RSS
fi
