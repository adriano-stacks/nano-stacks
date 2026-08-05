//! An observer hears about a block, not just that one could be described.
//!
//! `event_observer` checks that nano builds the same `new_block` payload
//! stacks-core published. That is the harder half and says nothing about the
//! easier one: whether anything ever sends it. Until now nothing did on the
//! follow path — the dispatcher was handed to the miner alone, so a node that
//! only follows executed blocks in silence.
//!
//! So this stands up a listener and asserts the payload arrives at it, which is
//! the whole of what an observer is promised.

use std::sync::{Arc, Mutex};

use nano_rpc::{EventDispatcher, EventKind};
use tokio::net::TcpListener;

/// Accept one POST, keep its body, and answer.
async fn listen(received: Arc<Mutex<Vec<String>>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a listener");
    let address = listener.local_addr().expect("an address");
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buffer = vec![0; 64 * 1024];
            let read = stream.read(&mut buffer).await.unwrap_or(0);
            let request = String::from_utf8_lossy(&buffer[..read]).into_owned();
            if let Some((_, body)) = request.split_once("\r\n\r\n") {
                received.lock().expect("the lock").push(body.to_owned());
            }
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}")
                .await;
        }
    });
    format!("http://{address}")
}

#[tokio::test]
async fn an_observer_receives_what_the_node_dispatches() {
    let received = Arc::new(Mutex::new(Vec::new()));
    let url = listen(Arc::clone(&received)).await;

    let dispatcher = EventDispatcher::new(vec![url.parse().expect("a URL")]);
    let payload = serde_json::json!({
        "block_height": 8_665_780,
        "transactions": [],
    });
    // `dispatch` queues and returns; `settle` is how a test waits for the
    // observer's own drain task, which is what a node never does.
    dispatcher.dispatch(EventKind::NewBlock, &payload);
    assert!(
        dispatcher.settle(std::time::Duration::from_secs(10)).await,
        "the event was delivered: {:?}",
        dispatcher.status()
    );

    // The listener answers before the body is parsed, so give it a moment.
    for _ in 0..50 {
        if !received.lock().expect("the lock").is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let bodies = received.lock().expect("the lock").clone();
    let body = bodies.first().expect("the observer received a body");
    assert!(
        body.contains("8665780"),
        "the observer received the payload it was sent: {body}"
    );
}
