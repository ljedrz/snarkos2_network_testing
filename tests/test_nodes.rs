use pea2pea::{
    protocols::{Handshake, Reading, Writing},
    Config, Node, Pea2Pea,
};
use snarkos2_network_testing::TestNode;
use tokio::task;
use tracing::*;
use tracing_subscriber::filter::EnvFilter;

use std::net::{IpAddr, Ipv4Addr};

fn start_logger() {
    let filter = match EnvFilter::try_from_default_env() {
        Ok(filter) => filter.add_directive("mio=off".parse().unwrap()),
        _ => EnvFilter::default().add_directive("mio=off".parse().unwrap()),
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        //.without_time()
        .with_target(false)
        .init();
}

#[tokio::test]
async fn spawn_inert_node_at_port() {
    start_logger();

    const PORT: u16 = 4132;

    let config = Config {
        listener_ip: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        desired_listening_port: Some(PORT),
        ..Default::default()
    };

    let test_node = TestNode::new(Node::new(Some(config)).await.unwrap());
    test_node.enable_handshake();
    test_node.enable_reading();
    test_node.enable_writing();
    test_node.run_periodic_tasks();

    std::future::pending::<()>().await;
}

#[tokio::test]
async fn spawn_inert_nodes() {
    start_logger();

    const NUM_NODES: usize = 10;
    const TARGET_ADDR: &str = "127.0.0.1:4132";

    let common_config = Config {
        listener_ip: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        ..Default::default()
    };

    let mut nodes = Vec::with_capacity(NUM_NODES);
    for _ in 0..NUM_NODES {
        let test_node = TestNode::new(Node::new(Some(common_config.clone())).await.unwrap());
        test_node.enable_handshake();
        test_node.enable_reading();
        test_node.enable_writing();

        nodes.push(test_node);
    }

    for test_node in &nodes {
        debug!(parent: test_node.node().span(), "attempting to connect to {}", TARGET_ADDR);

        let test_node = test_node.clone();
        task::spawn(async move {
            if let Err(e) = test_node.node().connect(TARGET_ADDR.parse().unwrap()).await {
                error!(parent: test_node.node().span(), "couldn't connect to {}: {}", TARGET_ADDR, e);
            }

            test_node.run_periodic_tasks();
        });
    }

    std::future::pending::<()>().await;
}
