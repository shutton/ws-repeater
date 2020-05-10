use futures::channel::mpsc::{Receiver, Sender};
use futures_util::{SinkExt, StreamExt};
use log::{debug, warn};
use std::collections::HashMap;
use std::net::SocketAddr;
use tungstenite::protocol::Message;

#[derive(Debug)]
pub enum BrokerMessage {
    Relay(Message),
    NewClient(SocketAddr, Sender<Message>),
    DelClient(SocketAddr),
}

pub struct Broker {
    buffer: Vec<Message>,
    receiver: Receiver<BrokerMessage>,
}

impl Broker {
    pub fn new(receiver: Receiver<BrokerMessage>) -> Self {
        Broker {
            buffer: vec![],
            receiver,
        }
    }

    pub async fn run(mut self) {
        let mut client_streams: HashMap<SocketAddr, Sender<Message>> = HashMap::new();

        while let Some(msg) = self.receiver.next().await {
            debug!("broker msg:{:?}", msg);
            match msg {
                BrokerMessage::NewClient(peer, mut client) => {
                    debug!("Adding client {}", peer);
                    for msg in &self.buffer {
                        match client.send(msg.clone()).await {
                            Ok(_) => (),
                            Err(e) => {
                                warn!("Unable to relay message to client {}: {}", peer, e);
                            }
                        }
                    }
                    client_streams.insert(peer, client);
                }
                BrokerMessage::DelClient(peer) => {
                    debug!("Removing client {}", peer);
                    client_streams.remove(&peer);
                }
                BrokerMessage::Relay(msg) => {
                    self.buffer.push(msg.clone());
                    let mut dead_clients: Vec<SocketAddr> = vec![];
                    for (peer, client_stream) in client_streams.iter_mut() {
                        debug!("Sending message to client {}", peer);
                        match client_stream.send(msg.clone()).await {
                            Ok(_) => (),
                            Err(e) => {
                                warn!("Unable to relay message to client {}: {}", peer, e);
                                dead_clients.push(peer.clone());
                            }
                        }
                    }
                    for peer in dead_clients {
                        debug!("Removing client {}", peer);
                        client_streams.remove(&peer);
                    }
                }
            }
        }
    }
}
