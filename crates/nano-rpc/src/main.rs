#![forbid(unsafe_code)]

use std::{error::Error, net::SocketAddr, sync::Arc, time::Duration};

use clap::{Parser, Subcommand};
use nano_node::Node;
use nano_rpc::{SharedView, serve};
use nano_sync::SyncClient;
use reqwest::Url;
use tokio::{net::TcpListener, sync::RwLock, time::sleep};

#[derive(Parser)]
#[command(name = "stacks-node")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Follow an HTTP peer and serve the validated local view.
    Start {
        #[arg(long, default_value = "http://127.0.0.1:20443/")]
        peer: String,
        #[arg(long, default_value = "0.0.0.0:24443")]
        listen: SocketAddr,
        #[arg(long, default_value_t = 1)]
        poll_interval_secs: u64,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let Cli {
        command:
            Command::Start {
                peer,
                listen,
                poll_interval_secs,
            },
    } = Cli::parse();
    let client = SyncClient::new(Url::parse(&peer)?)?;
    let mut node = Node::new(client);
    node.poll().await?;
    let view = Arc::new(RwLock::new(node.view()));
    let listener = TcpListener::bind(listen).await?;
    let poller = poll(node, view.clone(), Duration::from_secs(poll_interval_secs));

    tokio::select! {
        result = serve(listener, view) => result.map_err(Into::into),
        result = poller => result,
    }
}

async fn poll(mut node: Node, view: SharedView, interval: Duration) -> Result<(), Box<dyn Error>> {
    loop {
        sleep(interval).await;
        node.poll().await?;
        *view.write().await = node.view();
    }
}
