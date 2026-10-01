#!/usr/bin/env bash
set -u

API=${API:-http://127.0.0.1:3000}
CLI="bitcoin-cli -regtest"
RESP=$(mktemp)
trap 'rm -f "$RESP"' EXIT
pass=0
fail=0

check() { # name expected_status path
  local name=$1 want=$2 path=$3
  local got
  got=$(curl -s -o "$RESP" -w '%{http_code}' "$API$path")
  if [ "$got" = "$want" ]; then
    echo "PASS  $name ($got)"
    pass=$((pass + 1))
  else
    echo "FAIL  $name: expected $want, got $got"
    cat "$RESP"; echo
    fail=$((fail + 1))
  fi
}

show() { echo "--- $1"; curl -s "$API$2"; echo; }

echo "== Setup =="
$CLI createwallet test >/dev/null 2>&1 || $CLI loadwallet test >/dev/null 2>&1
ADDR=$($CLI getnewaddress)
HEIGHT=$($CLI getblockcount)
if [ "$HEIGHT" -lt 101 ]; then
  $CLI generatetoaddress $((101 - HEIGHT)) "$ADDR" >/dev/null
fi
TXID=$($CLI sendtoaddress "$($CLI getnewaddress)" 1)
$CLI generatetoaddress 1 "$ADDR" >/dev/null
TIP=$($CLI getblockcount)
HASH1=$($CLI getblockhash 1)
TIPHASH=$($CLI getblockhash "$TIP")
echo "tip=$TIP  txid=$TXID"

echo; echo "== Endpoint checks =="
check "tip"                    200 "/api/tip"
check "blocks"                 200 "/api/blocks?limit=5"
check "blocks (start > tip)"   400 "/api/blocks?start=999999"
check "block by height"        200 "/api/block/1"
check "block by hash"          200 "/api/block/$HASH1"
check "block txs"              200 "/api/block/$TIPHASH/txs?limit=10"
check "tx detail"              200 "/api/tx/$TXID"
check "address"                200 "/api/address/$ADDR"
check "search height"          200 "/api/search?q=1"
check "search block hash"      200 "/api/search?q=$HASH1"
check "search txid"            200 "/api/search?q=$TXID"
check "search address"         200 "/api/search?q=$ADDR"
check "address txs"            200 "/api/address/$ADDR/txs?limit=5"
check "health"                 200 "/api/health"
check "frontend"               200 "/"

echo; echo "== Error checks =="
check "block not found"        404 "/api/block/99999"
check "block bad input"        400 "/api/block/hello"
check "tx bad input"           400 "/api/tx/abc"
check "tx not found"           404 "/api/tx/0000000000000000000000000000000000000000000000000000000000000000"
check "address bad input"      400 "/api/address/notanaddress"
check "search empty"           400 "/api/search"
check "search garbage"         400 "/api/search?q=garbage"
check "search unknown hash"    404 "/api/search?q=0000000000000000000000000000000000000000000000000000000000000000"

echo; echo "== Sample output (inspect by eye) =="
show "tip"          "/api/tip"
show "block 1"      "/api/block/1"
show "transaction (check fee_sat is a sensible number)" "/api/tx/$TXID"
show "address (compare balance with getbalance)" "/api/address/$ADDR"
echo "bitcoin-cli getbalance: $($CLI getbalance)"

echo; echo "Passed: $pass   Failed: $fail"
[ "$fail" -eq 0 ]
