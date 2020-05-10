use crate::broker::{Broker, BrokerMessage};
use crate::error::*;
use futures::channel::mpsc::{channel, Sender};
use futures::select;
use futures_util::{FutureExt, SinkExt, StreamExt};
use log::{debug, info, warn};
use std::{
    cell::RefCell,
    net::SocketAddr,
    rc::Rc,
    time::{Duration, Instant},
};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::WebSocketStream;
use tungstenite::protocol::Message;
use uuid::Uuid;

// type WStream = WebSocketStream<Stream<TcpStream, TlsStream<TcpStream>>>;
type WStream = WebSocketStream<TcpStream>;

#[derive(Clone)]
pub struct Server {
    publish_uuid: Uuid,
    client_uuid: Uuid,
    listen_addr: SocketAddr,
    broker: RefCell<Option<Sender<BrokerMessage>>>,
}

impl Server {
    pub fn new(addr: &SocketAddr) -> Rc<Self> {
        Rc::new(Server {
            listen_addr: *addr,
            publish_uuid: Uuid::new_v4(),
            client_uuid: Uuid::new_v4(),
            broker: RefCell::new(None),
            // buffer: RefCell::new(vec![]),
        })
    }

    fn uuid_to_uri(&self, guid: Uuid) -> http::uri::Uri {
        http::uri::Builder::new()
            .scheme("ws")
            .authority(format!("{}:{}", self.listen_addr.ip(), self.listen_addr.port()).as_str())
            .path_and_query(format!("/{}", guid).as_str())
            .build()
            .unwrap()
    }

    pub fn client_uri(&self) -> http::uri::Uri {
        self.uuid_to_uri(self.client_uuid)
    }

    pub fn publish_uri(&self) -> http::uri::Uri {
        self.uuid_to_uri(self.publish_uuid)
    }

    pub async fn run(&self) -> Result<()> {
        let mut listener = TcpListener::bind(&self.listen_addr).await?;

        let (sender, receiver) = channel::<BrokerMessage>(10240);
        let broker = Broker::new(receiver);
        *self.broker.borrow_mut() = Some(sender);

        tokio::spawn(async move { broker.run().await });

        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(self.clone().accept_connection(stream));
        }

        Ok(())
    }

    async fn accept_connection(self, stream: TcpStream) -> Result<()> {
        let peer = stream.peer_addr()?;
        info!("Connection from {:?}", peer);

        let mut path = None;
        let hdr_callback =
            |req: &tungstenite::handshake::server::Request,
             resp: tungstenite::handshake::server::Response| {
                path = Some(req.uri().path().to_string());
                Ok(resp)
            };
        let ws_stream = tokio_tungstenite::accept_hdr_async(stream, hdr_callback).await;
        match ws_stream {
            Err(e) => {
                info!("Failed WS negotiation: {}", e);
                return Err(Error::with_chain(e, "new connection failed: "));
            }
            Ok(ws_stream) => {
                info!("New WS connection from {:?}", peer);
                if let Some(path) = path {
                    if path.starts_with('/') {
                        let maybe_guid = &path[1..];
                        if maybe_guid == self.publish_uuid.to_string() {
                            info!("Publisher connected! Let's start!");
                            tokio::spawn(self.clone().run_publisher(ws_stream));
                        } else if maybe_guid == self.client_uuid.to_string() {
                            info!("We have an audience.");
                            tokio::spawn(self.clone().run_client(ws_stream, peer));
                        } else {
                            info!("invalid guid");
                        }
                    }
                }
            }
        }

        Ok(())
    }

    async fn run_publisher(self, mut ws_stream: WStream) -> Result<()> {
        let mut broker = self.broker.into_inner().unwrap();
        loop {
            match ws_stream.next().await {
                None => {
                    debug!("done.");
                    break Ok(());
                }
                Some(item) => {
                    let msg = item?;
                    broker.send(BrokerMessage::Relay(msg)).await?;
                }
            }
        }
    }

    async fn run_client(self, ws_stream: WStream, peer: SocketAddr) -> Result<()> {
        /*
         * Set up a channel for the broker to send us messages
         */
        let (sender, mut receiver) = channel::<Message>(10240);
        let mut broker = self.broker.into_inner().unwrap();
        /*
         * And give it the sending end
         */
        broker.send(BrokerMessage::NewClient(peer, sender)).await?;

        /*
         * Create a "fused" version of the stream.  This allows us to poll it repeatedly without
         * missing a message if the other stream returns a result.
         */
        let mut ws_stream = ws_stream.fuse();
        let wait_dur = Duration::from_secs(5);
        let max_idle = Duration::from_secs(15);
        let mut last_ping_reply = None;
        let mut last_ping = None;

        let ping = Message::Ping(vec![0; 8]);
        loop {
            let mut next_pub_msg = receiver.next();
            let mut next_cli_msg = ws_stream.next();
            let mut idle_check = tokio::time::delay_for(wait_dur).fuse();
            select! {
                    pub_msg = next_pub_msg => {
                        if let Some(msg) = pub_msg {
                            ws_stream.send(msg).await?;
                            continue;
                        } else {
                            /*
                             * Publisher is done
                             */
                            break;
                        }
                    },
                    cli_msg = next_cli_msg => {
                        match cli_msg {
                            Some(msg) => match msg {
                                Ok(msg) => match msg {
                                    Message::Pong(_) => {
                                        debug!("peer({}) pong", peer);
                                        last_ping_reply = Some(Instant::now());
                                    },
                                    _ => debug!("Ignoring client msg {:?}", msg),
                                },
                                Err(e) => {
                                    warn!("peer({}) {}", peer, e);
                                    break;
                                }
                            },
                            None => {
                                /*
                                 * Client is done
                                 */
                                broker.send(BrokerMessage::DelClient(peer)).await?;
                                break;
                            }
                        }
                    },
                    _ = idle_check => {
                        if last_ping.is_some() {
                            let now = Instant::now();
                            if last_ping_reply.is_none() || now - last_ping_reply.unwrap() > max_idle {
                                debug!("peer({}) {:?} since last ping reply", peer, now - last_ping_reply.unwrap());
                                break;
                            }
                            debug!("peer({}) pinging", peer);
                            last_ping = Some(now);
                            ws_stream.send(ping.clone()).await?;
                        }
                    }
            }
        }

        #[cfg(disabled)]
        while let Some(msg) = receiver.next().await {
            debug!("run_client: msg={:?}", msg);
            ws_stream.send(msg).await?;
        }

        drop(receiver);
        debug!("ending read loop");
        Ok(())
    }
}
