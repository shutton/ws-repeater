// use futures::channel::{mpsc, mpsc::UnboundedSender};
use futures::select;
// use futures_util::future::FutureExt;
// use futures_util::{future, pin_mut, StreamExt};
use futures_util::StreamExt;
use futures_util::{FutureExt, SinkExt};
use log::warn;
use std::cell::RefCell;
use std::rc::Rc;
// use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::prelude::*;
use tungstenite::protocol::Message;

/*
 * This is lifted largely from https://docs.rs/crate/tokio-tungstenite/0.10.1/source/examples/client.rs
 */
#[cfg(disabled)]
pub async fn connect<'a, T, U>(url: String, mut ifh: T, ofh: Rc<RefCell<U>>)
where
    T: AsyncRead + Unpin + Send + 'a,
    U: AsyncWrite + Unpin + ?Sized,
{
    let url = url::Url::parse(&url).expect("parsing url");
    // info!("connecting to {:?}", url);

    let (stdin_tx, stdin_rx) = mpsc::unbounded();
    tokio::spawn(async move { read_stdin(stdin_tx, ifh) });

    let (ws_stream, _) = tokio_tungstenite::connect_async(url)
        .await
        .expect("connect");
    let (write, read) = ws_stream.split();
    let stdin_to_ws = stdin_rx.map(Ok).forward(write);
    let ws_to_ofh = {
        read.for_each(|message| async {
            match message {
                Ok(m) => {
                    let data = m.into_data();
                    ofh.borrow_mut().write_all(&data).await.unwrap();
                }
                Err(e) => {
                    warn!("message: {}", e);
                    return;
                }
            }
        })
    };
    pin_mut!(stdin_to_ws, ws_to_ofh);
    future::select(stdin_to_ws, ws_to_ofh).await;
}

pub async fn new_connect<T, U>(url: String, ifh: Rc<RefCell<T>>, ofh: Rc<RefCell<U>>)
where
    T: AsyncRead + Unpin + ?Sized,
    U: AsyncWrite + Unpin + ?Sized,
{
    let url = url::Url::parse(&url).expect("parsing url");

    // let (wsbuf_sink, wsbuf_stream) = mpsc::unbounded::<Message>();

    let (ws, _) = tokio_tungstenite::connect_async(url)
        .await
        .expect("connect");

    let mut ws = ws.fuse();
    let mut ifh_stream = ifh.borrow_mut();

    loop {
        let mut stdin_buf = vec![0; 1024];
        let mut next_ws_msg = ws.next();
        let mut next_local_msg = ifh_stream.read(&mut stdin_buf).fuse();
        select! {
            ws_msg = next_ws_msg => {
                match ws_msg {
                    Some(result) => {
                        match result {
                            Ok(msg) => {
                                let data = msg.into_data();
                                /*
                                 * Local output blocks
                                 */
                                ofh.borrow_mut().write_all(&data).await.unwrap();
                            }
                            Err(e) => {
                                warn!("message: {}", e);
                                break;
                            }
                        }
                    }
                    None => {
                        return;
                    }
                }
            },
            local_msg = next_local_msg => {
                match local_msg {
                    Ok(nread) => {
                        stdin_buf.truncate(nread);
                        /*
                         * WebSocket writes block
                         */
                        ws.send(Message::binary(stdin_buf)).await.unwrap();
                    }
                    Err(_) => {},
                }
            }
        }
    }

    tokio::time::delay_for(std::time::Duration::from_secs(1)).await
}

#[cfg(disabled)]
async fn read_stdin<T>(tx: UnboundedSender<Message>, mut stdin: T)
where
    T: AsyncRead + Unpin,
{
    loop {
        let mut buf = vec![0; 1024];
        let n = match stdin.read(&mut buf).await {
            Err(_) | Ok(0) => break,
            Ok(n) => n,
        };
        buf.truncate(n);
        tx.unbounded_send(Message::binary(buf)).unwrap();
    }
    tx.unbounded_send(Message::Close(None)).unwrap();
}
