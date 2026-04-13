// Tiny axum-based mock Solana upstream for benchmarking solmux without
// confounding Python-threading overhead. Responds to any POST with a static
// JSON-RPC result.
//
//   cargo run --release --example mock_upstream
//   ab -k -n 10000 -c 50 -p req.json -T application/json http://127.0.0.1:18000/

use axum::{response::IntoResponse, routing::post, Router};
use std::net::SocketAddr;

const BODY: &[u8] = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":123456789}";

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let app = Router::new().route("/", post(handler));
    let addr: SocketAddr = "127.0.0.1:18000".parse().unwrap();
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    println!("mock_upstream listening on {addr}");
    axum::serve(listener, app).await.unwrap();
}

async fn handler() -> impl IntoResponse {
    ([("content-type", "application/json")], BODY)
}
