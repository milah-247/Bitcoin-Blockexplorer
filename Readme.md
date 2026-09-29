# Block Explorer

JSON API over a Bitcoin Core node. Rust, axum, bitcoin, bitcoincore-rpc.

## Requirements
- Bitcoin Core **25+** (needed for `getblock` verbosity 3, which includes prevouts for fee calculation)
- `txindex=1`

Example `bitcoin.conf` for local development:

```
regtest=1
server=1
txindex=1
rpcuser=user
rpcpassword=pass
```

## Run

```bash
bitcoind -daemon
bitcoin-cli -regtest createwallet test
ADDR=$(bitcoin-cli -regtest getnewaddress)
bitcoin-cli -regtest generatetoaddress 101 $ADDR

RPC_USER=user RPC_PASS=pass NETWORK=regtest cargo run
```

Environment variables:

| Var          | Default                          | Notes                                        |
|--------------|----------------------------------|----------------------------------------------|
| `NETWORK`    | `regtest`                        | `mainnet`, `testnet`, `signet`, `regtest`    |
| `RPC_URL`    | `http://127.0.0.1:<network port>`|                                              |
| `RPC_USER` / `RPC_PASS` | required unless cookie|                                              |
| `RPC_COOKIE` | none                             | Path to `.cookie` file, alternative to user/pass |
| `BIND`       | `127.0.0.1:3000`                 |                                              |

## Endpoints

```
GET /api/tip
GET /api/blocks?start=<height>&limit=20          (limit max 50, newest first, returns next_start)
GET /api/block/:height_or_hash
GET /api/block/:height_or_hash/txs?start=0&limit=25   (limit max 100, returns next_start)
GET /api/tx/:txid
GET /api/address/:addr
GET /api/search?q=<height|blockhash|txid|address>
```

Try it:

```bash
curl localhost:3000/api/tip
curl "localhost:3000/api/blocks?limit=5"
curl localhost:3000/api/block/1
curl localhost:3000/api/search?q=1
curl localhost:3000/api/address/$ADDR
```

Errors share one shape:

```json
{ "error": { "code": "not_found", "message": "..." } }
```

| HTTP | code             | When                                          |
|------|------------------|-----------------------------------------------|
| 400  | `invalid_input`  | bad height/hash/txid/address, wrong network   |
| 404  | `not_found`      | node reports RPC error -5, or height > tip    |
| 503  | `node_busy`      | another `scantxoutset` is already running     |
| 502  | `upstream_error` | node unreachable or other RPC failure         |
| 500  | `internal_error` | task join failure                             |

## Known limitations
- **Address endpoint uses `scantxoutset`.** It gives balance and UTXOs (confirmed only), not history. It takes minutes on mainnet and Core runs only one scan at a time. Use regtest/signet/testnet for development. Real history needs your own index: walk blocks and store `scriptPubKey -> (txid, vout, amount, spent_by)` in SQLite/RocksDB.
- **Transaction lookup for confirmed txs loads the whole block** (`getblock` verbosity 3) to get prevouts. Fine for dev, heavy for large mainnet blocks; add an LRU cache as a next step.
- **Amounts are parsed from Core's BTC floats** and rounded to satoshis.

## Ideas for next steps
- Address index + `/api/address/:addr/txs` with pagination
- Caching (blocks are immutable once buried)
- Integration tests using regtest and `generatetoaddress`
- CORS + a small frontend