use std::{error::Error, io, net::SocketAddr, time::Duration};

use clap::{Parser, Subcommand};
use nano_node::Node;
use nano_rpc::{RpcState, serve};
use nano_sync::SyncClient;
use reqwest::Url;
use tokio::{net::TcpListener, time::sleep};

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
    let state = RpcState::new();
    state
        .publish(
            node.view()
                .ok_or_else(|| io::Error::other("node has no view"))?,
        )
        .await;
    let listener = TcpListener::bind(listen).await?;
    let poller = poll(node, state.clone(), Duration::from_secs(poll_interval_secs));

    tokio::select! {
        result = serve(listener, state) => result.map_err(Into::into),
        result = poller => result,
    }
}

async fn poll(mut node: Node, state: RpcState, interval: Duration) -> Result<(), Box<dyn Error>> {
    loop {
        sleep(interval).await;
        node.poll().await?;
        state
            .publish(
                node.view()
                    .ok_or_else(|| io::Error::other("node has no view"))?,
            )
            .await;
    }
}
