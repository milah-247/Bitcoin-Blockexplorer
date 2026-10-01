//! HTTP-level tests that run the real binary on a free port.
//!
//!   cargo test                              # tests that need no node
//!   TEST_RPC_USER=user TEST_RPC_PASS=pass cargo test -- --ignored regtest_smoke
//!   cargo test -- --ignored mainnet_smoke   # uses RPC_URL / RPC_API_KEY (env or .env)

use std::{
    net::{TcpListener, TcpStream},
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use serde_json::{json, Value};

/// Variables the binary reads; cleared so the developer's shell/.env can't leak in.
const VARS: &[&str] = &[
    "NETWORK", "RPC_URL", "RPC_USER", "RPC_PASS", "RPC_COOKIE", "RPC_API_KEY", "RPC_API_KEY_HEADER",
    "RPC_RATE_LIMIT_PER_MIN", "RPC_MAX_CONCURRENCY", "RPC_TIMEOUT_SECS", "RPC_MAX_RETRIES", "BIND",
    "CORS_ORIGINS", "REQUEST_TIMEOUT_SECS", "INDEX_ENABLED", "INDEX_DB_PATH", "START_HEIGHT",
    "INDEX_CONCURRENCY", "INDEX_POLL_SECS", "FRONTEND_DIR", "CACHE_MAX_MB",
];

struct Server {
    child: Child,
    base: String,
    http: reqwest::Client,
    tmp: Option<PathBuf>,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(d) = &self.tmp {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}

fn start(envs: &[(&str, String)], tmp: Option<PathBuf>) -> Server {
    let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_block-explorer"));
    for v in VARS {
        cmd.env_remove(v);
    }
    cmd.env("ENV_FILE", "none")
        .env("BIND", format!("127.0.0.1:{port}"))
        .env("RUST_LOG", "warn")
        .env("FRONTEND_DIR", concat!(env!("CARGO_MANIFEST_DIR"), "/frontend"))
        .stdout(Stdio::null());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let child = cmd.spawn().expect("spawn block-explorer");
    let deadline = Instant::now() + Duration::from_secs(15);
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < deadline, "server did not start");
        std::thread::sleep(Duration::from_millis(50));
    }
    Server { child, base: format!("http://127.0.0.1:{port}"), http: reqwest::Client::new(), tmp }
}

impl Server {
    async fn get(&self, path: &str) -> (u16, Value) {
        let r = self.http.get(format!("{}{path}", self.base)).send().await.expect("request");
        let status = r.status().as_u16();
        (status, r.json().await.unwrap_or(Value::Null))
    }

    /// Assert the status and, for errors, the JSON error shape. Returns the body.
    async fn check(&self, name: &str, want: u16, path: &str) -> Value {
        let (got, body) = self.get(path).await;
        assert_eq!(got, want, "{name}: GET {path} -> {got}, body {body}");
        if want >= 400 {
            assert!(body["error"]["code"].is_string() && body["error"]["message"].is_string(), "{name}: bad error shape {body}");
        }
        body
    }
}

fn tmp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("be-it-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

// ------------------------------------------------------------------ no node

#[tokio::test]
async fn no_node_validation_errors_and_static_files() {
    let s = start(
        &[
            ("NETWORK", "regtest".into()),
            ("RPC_URL", "http://127.0.0.1:9".into()), // nothing listens here
            ("RPC_USER", "u".into()),
            ("RPC_PASS", "p".into()),
            ("RPC_MAX_RETRIES", "0".into()),
            ("INDEX_ENABLED", "false".into()),
            ("CORS_ORIGINS", "https://allowed.example".into()),
        ],
        None,
    );

    // Input validation happens before any RPC call.
    for (name, path) in [
        ("block bad input", "/api/block/hello"),
        ("block bad height", "/api/block/99999999999999999999999"),
        ("block txs bad input", "/api/block/hello/txs"),
        ("tx bad input", "/api/tx/abc"),
        ("tx bad block hint", &format!("/api/tx/{}?block=nope", "0".repeat(64))),
        ("address bad input", "/api/address/notanaddress"),
        ("address wrong network", "/api/address/bc1qxy2kgdygjrsqtzq2n0yrf2493p83kkfjhx0wlh"),
        ("address txs bad input", "/api/address/notanaddress/txs"),
        ("search empty", "/api/search"),
        ("search blank", "/api/search?q=%20%20"),
        ("search garbage", "/api/search?q=garbage"),
        ("search too long", &format!("/api/search?q={}", "x".repeat(200))),
        ("blocks bad limit", "/api/blocks?limit=abc"),
        ("blocks negative start", "/api/blocks?start=-1"),
    ] {
        let b = s.check(name, 400, path).await;
        assert_eq!(b["error"]["code"], "invalid_input", "{name}");
    }

    let b = s.check("unknown api path", 404, "/api/nope").await;
    assert_eq!(b["error"]["code"], "not_found");
    let b = s.check("node down", 502, "/api/tip").await;
    assert_eq!(b["error"]["code"], "upstream_error");
    let b = s.check("address history without index", 501, "/api/address/bcrt1qerxxrhrpwsem4zctn6fksh8sngp3yxwy8mfej3/txs").await;
    assert_eq!(b["error"]["code"], "not_supported");

    let (code, h) = s.get("/api/health").await;
    assert_eq!(code, 503);
    assert_eq!(h["status"], "degraded");
    assert_eq!(h["network"], "regtest");

    // Static frontend, same origin.
    let r = s.http.get(format!("{}/", s.base)).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.text().await.unwrap().contains("Block Explorer"));
    assert_eq!(s.http.get(format!("{}/app.js", s.base)).send().await.unwrap().status(), 200);

    // CORS only for configured origins.
    let cors = |origin: &'static str| {
        let req = s.http.get(format!("{}/api/search", s.base)).header("Origin", origin);
        async move { req.send().await.unwrap().headers().get("access-control-allow-origin").cloned() }
    };
    assert_eq!(cors("https://allowed.example").await.unwrap(), "https://allowed.example");
    assert!(cors("https://evil.example").await.is_none());
}

// ------------------------------------------------------------------ regtest

struct Node {
    url: String,
    user: String,
    pass: String,
    http: reqwest::Client,
}

impl Node {
    fn from_env() -> Node {
        let need = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("set {k} for this test"));
        Node {
            url: std::env::var("TEST_RPC_URL").unwrap_or_else(|_| "http://127.0.0.1:18443".into()),
            user: need("TEST_RPC_USER"),
            pass: need("TEST_RPC_PASS"),
            http: reqwest::Client::new(),
        }
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, Value> {
        let body = json!({ "jsonrpc": "1.0", "id": 1, "method": method, "params": params });
        let r: Value = self
            .http
            .post(&self.url)
            .basic_auth(&self.user, Some(&self.pass))
            .json(&body)
            .send()
            .await
            .expect("node reachable")
            .json()
            .await
            .unwrap();
        if r["error"].is_null() {
            Ok(r["result"].clone())
        } else {
            Err(r["error"].clone())
        }
    }

    async fn ok(&self, method: &str, params: Value) -> Value {
        self.call(method, params).await.unwrap_or_else(|e| panic!("{method}: {e}"))
    }
}

/// Same checks as smoke_test.sh, plus the address index.
#[tokio::test]
#[ignore = "needs a regtest bitcoind with txindex and a wallet; set TEST_RPC_USER/TEST_RPC_PASS"]
async fn regtest_smoke() {
    let n = Node::from_env();
    if n.call("createwallet", json!(["test"])).await.is_err() {
        let _ = n.call("loadwallet", json!(["test"])).await;
    }
    let addr = n.ok("getnewaddress", json!([])).await;
    let height = n.ok("getblockcount", json!([])).await.as_u64().unwrap();
    if height < 101 {
        n.ok("generatetoaddress", json!([101 - height, addr])).await;
    }
    let dest = n.ok("getnewaddress", json!([])).await;
    let txid = n.ok("sendtoaddress", json!([dest, 1])).await;
    let txid = txid.as_str().unwrap();
    n.ok("generatetoaddress", json!([1, addr])).await;
    let tip = n.ok("getblockcount", json!([])).await.as_u64().unwrap();
    let hash1 = n.ok("getblockhash", json!([1])).await;
    let hash1 = hash1.as_str().unwrap();
    let tiphash = n.ok("getblockhash", json!([tip])).await;
    let tiphash = tiphash.as_str().unwrap();
    let addr = addr.as_str().unwrap();

    let dir = tmp_dir("regtest");
    let s = start(
        &[
            ("NETWORK", "regtest".into()),
            ("RPC_URL", n.url.clone()),
            ("RPC_USER", n.user.clone()),
            ("RPC_PASS", n.pass.clone()),
            ("INDEX_DB_PATH", dir.join("idx.sqlite").display().to_string()),
            ("START_HEIGHT", "0".into()),
            ("INDEX_POLL_SECS", "1".into()),
        ],
        Some(dir),
    );

    // Endpoint checks
    let t = s.check("tip", 200, "/api/tip").await;
    assert_eq!(t["height"], tip);
    assert_eq!(t["chain"], "regtest");
    let b = s.check("blocks", 200, "/api/blocks?limit=5").await;
    assert_eq!(b["blocks"].as_array().unwrap().len(), 5);
    assert_eq!(b["next_start"], tip - 5);
    s.check("blocks (start > tip)", 400, "/api/blocks?start=999999").await;
    let b = s.check("block by height", 200, "/api/block/1").await;
    assert_eq!(b["hash"], hash1);
    s.check("block by hash", 200, &format!("/api/block/{hash1}")).await;
    let bt = s.check("block txs", 200, &format!("/api/block/{tiphash}/txs?limit=10")).await;
    assert!(bt["txids"].as_array().unwrap().iter().any(|t| t == txid));
    let tx = s.check("tx detail", 200, &format!("/api/tx/{txid}")).await;
    assert!(tx["fee_sat"].as_u64().unwrap() > 0, "fee should be known: {tx}");
    assert!(tx["fee_rate_sat_vb"].as_f64().unwrap() > 0.0);
    assert_eq!(tx["status"]["block_height"], tip);
    s.check("tx detail with block hint", 200, &format!("/api/tx/{txid}?block={tip}")).await;
    s.check("search height", 200, "/api/search?q=1").await;
    assert_eq!(s.check("search block hash", 200, &format!("/api/search?q={hash1}")).await["type"], "block");
    assert_eq!(s.check("search txid", 200, &format!("/api/search?q={txid}")).await["type"], "tx");
    assert_eq!(s.check("search address", 200, &format!("/api/search?q={addr}")).await["type"], "address");

    // Error checks
    s.check("block not found", 404, "/api/block/99999").await;
    s.check("tx not found", 404, &format!("/api/tx/{}", "0".repeat(64))).await;
    s.check("search unknown hash", 404, &format!("/api/search?q={}", "0".repeat(64))).await;

    // Address index: wait for sync, then compare with the node's own UTXO scan.
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let (_, h) = s.get("/api/health").await;
        if h["index"]["synced"] == true && h["index"]["indexed_height"] == tip {
            break;
        }
        assert!(Instant::now() < deadline, "index did not sync: {h}");
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    let a = s.check("address", 200, &format!("/api/address/{addr}")).await;
    assert_eq!(a["source"], "index");
    assert_eq!(a["index"]["complete_history"], true);
    let scan = n.ok("scantxoutset", json!(["start", [format!("addr({addr})")]])).await;
    let scan_sat = (scan["total_amount"].as_f64().unwrap() * 1e8).round() as u64;
    assert_eq!(a["balance_sat"], scan_sat, "index balance must match scantxoutset");
    assert_eq!(a["utxo_count"], scan["unspents"].as_array().unwrap().len());
    let n_txs = a["tx_count"].as_u64().unwrap();
    assert!(n_txs >= 1);
    let h = s.check("address txs", 200, &format!("/api/address/{addr}/txs?limit=100")).await;
    assert_eq!(h["txs"].as_array().unwrap().len() as u64, n_txs.min(100));
    // the funding tx sent from the wallet shows up in the destination's history
    let h = s.check("dest txs", 200, &format!("/api/address/{}/txs", dest.as_str().unwrap())).await;
    assert_eq!(h["txs"][0]["txid"], txid);
    assert_eq!(h["txs"][0]["received_sat"], 100_000_000);
    assert!(h["next_start"].is_null());
}

// ------------------------------------------------------------------ mainnet

/// Read-only checks against the configured hosted endpoint (about 10 RPC calls).
#[tokio::test]
#[ignore = "needs RPC_URL and RPC_API_KEY for a mainnet endpoint (env or .env)"]
async fn mainnet_smoke() {
    let _ = dotenvy::dotenv();
    let need = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("set {k} (env or .env)"));
    let s = start(
        &[
            ("NETWORK", "mainnet".into()),
            ("RPC_URL", need("RPC_URL")),
            ("RPC_API_KEY", need("RPC_API_KEY")),
            ("RPC_RATE_LIMIT_PER_MIN", "60".into()),
            ("INDEX_ENABLED", "false".into()),
        ],
        None,
    );

    let t = s.check("tip", 200, "/api/tip").await;
    assert_eq!(t["chain"], "main");
    assert!(t["height"].as_u64().unwrap() > 900_000);

    let b = s.check("halving block", 200, "/api/block/840000").await;
    assert_eq!(b["hash"], "0000000000000000000320283a032748cef8227873ff4872689bf23f1cda83a5");
    assert_eq!(b["subsidy_sat"], 312_500_000);
    assert!(b["total_fees_sat"].as_u64().unwrap() > 0);

    let blocks = s.check("blocks", 200, "/api/blocks?limit=3").await;
    assert_eq!(blocks["blocks"].as_array().unwrap().len(), 3);

    // The pizza transaction: needs a block hint because the provider has no txindex.
    let pizza = "a1075db55d416d3ca199f55b6084e2115b9345e16c5cf302fc80e9d5fbf5d48d";
    // (Order matters: once fetched with a hint, the tx is cached and found without one.)
    s.check("pizza tx without hint", 404, &format!("/api/tx/{pizza}")).await;
    let tx = s.check("pizza tx with hint", 200, &format!("/api/tx/{pizza}?block=57043")).await;
    assert_eq!(tx["fee_sat"], 99_000_000);
    assert_eq!(tx["outputs"][0]["value_sat"], 1_000_000_000_000u64);
    s.check("pizza tx from cache", 200, &format!("/api/tx/{pizza}")).await;

    // scantxoutset is not permitted by the provider; with the index off we say so.
    let a = s.check("address without index", 501, "/api/address/1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa").await;
    assert_eq!(a["error"]["code"], "not_supported");
}
