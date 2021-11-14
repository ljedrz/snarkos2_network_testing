use pea2pea::{
    protocols::{Handshake, Reading, Writing},
    Connection, Node, Pea2Pea,
};
use snarkos::Environment;
use snarkos_ledger::BlockLocators;
use snarkvm::{dpc::testnet2::Testnet2, traits::Network};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::Mutex,
    task,
};
use tracing::*;

use std::{collections::HashSet, io, net::SocketAddr, sync::Arc, time::Duration};

// consts & helper types
const CHALLENGE_HEIGHT: u32 = 0;
const PING_INTERVAL_SECS: u64 = 5;
const PEER_INTERVAL_SECS: u64 = 3;
const DESIRED_CONNECTIONS: usize = 10;

type SnarkosMessage = snarkos::Message<Testnet2, snarkos::Client<Testnet2>>;

// pea2pea impls

#[derive(Clone, Default)]
struct State {
    pub peers: Arc<Mutex<HashSet<SocketAddr>>>,
}

#[derive(Clone)]
pub struct TestNode {
    node: Node,
    state: State,
}

impl TestNode {
    pub fn new(node: Node) -> Self {
        Self {
            node,
            state: Default::default(),
        }
    }

    pub fn run_periodic_tasks(&self) {
        // pings
        let node = self.clone();
        task::spawn(async move {
            let genesis = Testnet2::genesis_block();
            let ping_msg = SnarkosMessage::Ping(
                <snarkos::Client<Testnet2>>::MESSAGE_VERSION,
                genesis.height(),
                genesis.hash(),
            );

            loop {
                if node.node().num_connected() != 0 {
                    info!(parent: node.node().span(), "sending out Pings");
                    node.send_broadcast(ping_msg.clone());
                }
                tokio::time::sleep(Duration::from_secs(PING_INTERVAL_SECS)).await;
            }
        });

        // peer maintenance
        let node = self.clone();
        task::spawn(async move {
            loop {
                let num_conns = node.node().num_connected() + node.node().num_connecting();

                if num_conns < DESIRED_CONNECTIONS && node.node().num_connected() != 0 {
                    info!(parent: node.node().span(), "I'd like to have {} more peers; asking peers for their peers", DESIRED_CONNECTIONS - num_conns);
                    node.send_broadcast(SnarkosMessage::PeerRequest);
                }
                tokio::time::sleep(Duration::from_secs(PEER_INTERVAL_SECS)).await;
            }
        });
    }
}

impl Pea2Pea for TestNode {
    fn node(&self) -> &Node {
        &self.node
    }
}

#[async_trait::async_trait]
impl Handshake for TestNode {
    async fn perform_handshake(&self, mut conn: Connection) -> io::Result<Connection> {
        let mut locked_peers = self.state.peers.lock().await;
        // extra safeguard against double connections
        if locked_peers.contains(&conn.addr) {
            return Err(io::ErrorKind::AlreadyExists.into());
        }

        let own_addr = self.node().listening_addr()?;
        let peer_addr = conn.addr;

        let genesis_block_header = Testnet2::genesis_block().header();

        // Send a challenge request to the peer.
        let own_request = SnarkosMessage::ChallengeRequest(own_addr.port(), CHALLENGE_HEIGHT);
        trace!(parent: self.node().span(), "sending a challenge request to {}", peer_addr);
        let msg = own_request.serialize().unwrap();
        let len = u32::to_le_bytes(msg.len() as u32);
        conn.writer().write_all(&len).await?;
        conn.writer().write_all(&msg).await?;

        let mut buf = [0u8; 1024];

        // Read the challenge request from the peer.
        conn.reader().read_exact(&mut buf[..4]).await?;
        let len = u32::from_le_bytes(buf[..4].try_into().unwrap()) as usize;
        conn.reader().read_exact(&mut buf[..len]).await?;
        let peer_request = SnarkosMessage::deserialize(&buf[..len]);

        let peer_listening_addr = if let Ok(snarkos::Message::ChallengeRequest(
            peer_listening_port,
            _block_height,
        )) = peer_request
        {
            trace!(parent: self.node().span(), "received a challenge request from {}", peer_addr);
            SocketAddr::from((peer_addr.ip(), peer_listening_port))
        } else {
            error!(parent: self.node().span(), "invalid challenge request from {}", peer_addr);
            return Err(io::ErrorKind::InvalidData.into());
        };

        // Respond with own challenge request.
        let own_response = SnarkosMessage::ChallengeResponse(genesis_block_header.clone());
        trace!(parent: self.node().span(), "sending a challenge response to {}", peer_addr);
        let msg = own_response.serialize().unwrap();
        let len = u32::to_le_bytes(msg.len() as u32);
        conn.writer().write_all(&len).await?;
        conn.writer().write_all(&msg).await?;

        // Wait for the challenge response to come in.
        conn.reader().read_exact(&mut buf[..4]).await?;
        let len = u32::from_le_bytes(buf[..4].try_into().unwrap()) as usize;
        conn.reader().read_exact(&mut buf[..len]).await?;
        let peer_response = SnarkosMessage::deserialize(&buf[..len]);

        if let Ok(snarkos::Message::ChallengeResponse(block_header)) = peer_response {
            trace!(parent: self.node().span(), "received a challenge response from {}", peer_addr);
            if block_header.height() == CHALLENGE_HEIGHT
                && &block_header == genesis_block_header
                && block_header.is_valid()
            {
                locked_peers.insert(peer_listening_addr);
                debug!(parent: self.node().span(), "added {} to my list of peers", peer_listening_addr);

                Ok(conn)
            } else {
                error!(parent: self.node().span(), "invalid challenge response from {}", peer_addr);
                Err(io::ErrorKind::InvalidData.into())
            }
        } else {
            error!(parent: self.node().span(), "invalid challenge response from {}", peer_addr);
            Err(io::ErrorKind::InvalidData.into())
        }
    }
}

#[async_trait::async_trait]
impl Reading for TestNode {
    type Message = SnarkosMessage;

    fn read_message<R: io::Read>(
        &self,
        source: SocketAddr,
        reader: &mut R,
    ) -> io::Result<Option<Self::Message>> {
        let mut buf = [0u8; 8 * 1024];

        reader.read_exact(&mut buf[..4])?;
        let len = u32::from_le_bytes(buf[..4].try_into().unwrap()) as usize;

        if reader.read_exact(&mut buf[..len]).is_err() {
            return Ok(None);
        }

        match SnarkosMessage::deserialize(&buf[..len]) {
            Ok(msg) => {
                info!(parent: self.node().span(), "received a {} from {}", msg.name(), source);
                Ok(Some(msg))
            }
            Err(e) => {
                error!("a message from {} failed to deserialize: {}", source, e);
                Err(io::ErrorKind::InvalidData.into())
            }
        }
    }

    async fn process_message(&self, source: SocketAddr, message: Self::Message) -> io::Result<()> {
        match message {
            SnarkosMessage::BlockRequest(_start_block_height, _end_block_height) => {}
            SnarkosMessage::BlockResponse(_block) => {}
            SnarkosMessage::Disconnect => {}
            SnarkosMessage::PeerRequest => {
                let peers = self
                    .state
                    .peers
                    .lock()
                    .await
                    .iter()
                    .copied()
                    .collect::<Vec<_>>();
                let msg = SnarkosMessage::PeerResponse(peers);
                info!(parent: self.node().span(), "sending a PeerResponse to {}", source);
                self.send_direct_message(source, msg)?;
            }
            SnarkosMessage::PeerResponse(peer_addrs) => {
                let num_conns = self.node().num_connected() + self.node().num_connecting();

                let node = self.clone();
                task::spawn(async move {
                    for peer_addr in peer_addrs
                        .into_iter()
                        .filter(|addr| node.node().listening_addr().unwrap() != *addr)
                        .take(DESIRED_CONNECTIONS.saturating_sub(num_conns))
                    {
                        if !node.node().is_connected(peer_addr)
                            && !node.state.peers.lock().await.contains(&peer_addr)
                        {
                            info!(parent: node.node().span(), "trying to connect to {}'s peer {}", source, peer_addr);
                            let _ = node.node().connect(peer_addr).await;
                        }
                    }
                });
            }
            SnarkosMessage::Ping(version, block_height, _block_hash) => {
                // Ensure the message protocol version is not outdated.
                if version < <snarkos::Client<Testnet2>>::MESSAGE_VERSION {
                    warn!(parent: self.node().span(), "dropping {} due to outdated version ({})", source, version);
                    return Err(io::ErrorKind::InvalidData.into());
                }
                debug!(parent: self.node().span(), "peer {} is at height {}", source, block_height);
                let genesis = Testnet2::genesis_block();
                let msg = SnarkosMessage::Pong(
                    None,
                    BlockLocators::<Testnet2>::from(vec![(genesis.height(), genesis.hash(), None)]),
                );
                info!(parent: self.node().span(), "sending a Pong to {}", source);
                self.send_direct_message(source, msg)?;
            }
            SnarkosMessage::Pong(_is_fork, _block_locators) => {}
            SnarkosMessage::UnconfirmedBlock(_block) => {}
            SnarkosMessage::UnconfirmedTransaction(_transaction) => {}
            _ => return Err(io::ErrorKind::InvalidData.into()), // Peer is not following the protocol.
        }

        Ok(())
    }
}

impl Writing for TestNode {
    type Message = SnarkosMessage;

    fn write_message<W: io::Write>(
        &self,
        _target: SocketAddr,
        payload: &Self::Message,
        writer: &mut W,
    ) -> io::Result<()> {
        let serialized = payload.serialize().unwrap();
        let len = u32::to_le_bytes(serialized.len() as u32);

        writer.write_all(&len)?;
        writer.write_all(&serialized)
    }
}
