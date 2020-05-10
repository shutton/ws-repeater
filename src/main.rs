#![recursion_limit = "1024"]
use clap::{App, AppSettings, Arg};
use log::{info, warn};
use nix::{sys::stat, unistd};
use std::{
    cell::RefCell,
    env,
    net::{IpAddr, SocketAddr},
    rc::Rc,
};
use tokio::{prelude::*, process::Command};

mod broker;
mod client;
mod error;
mod server;
use error::*;
use server::Server;

const DEFAULT_LISTEN_ADDR: std::net::Ipv4Addr = std::net::Ipv4Addr::UNSPECIFIED;
const DEFAULT_LISTEN_PORT: u16 = 7368;

#[tokio::main]
async fn main() {
    pretty_env_logger::formatted_timed_builder()
        .filter(Some("repeater"), log::LevelFilter::Debug)
        .filter(None, log::LevelFilter::Warn)
        .init();
    /*
     * Handle the command line
     */

    let cmdline = App::new(env!("CARGO_PKG_NAME"))
        .setting(AppSettings::TrailingVarArg)
        .arg(
            Arg::with_name("listen-addr")
                .help("Listen address")
                .short("-l")
                .long("listen")
                .value_name("ADDR")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("server")
                .help("Operate in server mode")
                .short("-s")
                .long("server")
                .takes_value(false),
        )
        .arg(
            Arg::with_name("asciinema-client")
                .help("Be an Asciinema client")
                .short("-c")
                .long("asciinema-client")
                .takes_value(false),
        )
        .arg(
            Arg::with_name("asciinema-publish")
                .help("Be an Asciinema publisher")
                .short("-p")
                .long("asciinema-publish")
                .takes_value(false),
        )
        .arg(Arg::with_name("url").help("be a client"))
        .get_matches();

    if cmdline.is_present("server") {
        let listen_sockaddr = get_listen_sockaddr(&cmdline);
        if let Err(e) = run_server(listen_sockaddr).await {
            warn!("Couldn't start server: {}", e);
        }
        return;
    }

    let mut asciinema_proc = None;
    let tempdir = tempfile::tempdir().unwrap();

    type Ofh = Rc<RefCell<dyn AsyncWrite + Unpin>>;
    type Ifh = Rc<RefCell<dyn AsyncRead + Unpin>>;

    enum AsciinemaMode {
        None,
        Client,
        Publisher,
    }

    let asciinema_mode = if cmdline.is_present("asciinema-publish") {
        AsciinemaMode::Publisher
    } else if cmdline.is_present("asciinema-client") {
        AsciinemaMode::Client
    } else {
        AsciinemaMode::None
    };

    let ifh: Ifh;
    let ofh: Ofh;

    match asciinema_mode {
        AsciinemaMode::Publisher => {
            /*
             * In record mode:
             *
             * - Asciinema will be in "record" mode.
             * - Asciinema will be told to record to the named pipe (stdin doesn't work)
             * - The websocket will be given:
             *   - the output side of the pipe as its input
             *   - stdout as its output (but there won't be any)
             */
            let mut fifo_path = tempdir.path().to_path_buf();
            fifo_path.push(".fifo-publisher");
            unistd::mkfifo(&fifo_path, stat::Mode::S_IRWXU).expect("mkfifo");
            asciinema_proc = Some(
                Command::new("asciinema")
                    .arg("rec")
                    .arg(&fifo_path)
                    .spawn()
                    .unwrap(),
            );
            let pipe_from = tokio::fs::OpenOptions::new()
                .read(true)
                .write(false)
                .create(false)
                .open(&fifo_path)
                .await
                .unwrap();
            ifh = Rc::new(RefCell::new(pipe_from));
            ofh = Rc::new(RefCell::new(tokio::io::stdout()));
        }
        AsciinemaMode::Client => {
            /*
             * In client mode:
             *
             * - Asciinema will be in "play" mode
             * - Asciinema will be told to play from the named pipe
             * - The websocket will be told to write to the named pipe
             * - The websocket will take input from stdin (but the server will ignore it)
             */
            let mut fifo_path = tempdir.path().to_path_buf();
            fifo_path.push(".fifo-client");
            unistd::mkfifo(&fifo_path, stat::Mode::S_IRWXU).expect("mkfifo");
            asciinema_proc = Some(
                Command::new("asciinema")
                    .arg("play")
                    .arg(&fifo_path)
                    .arg("-i")
                    .arg("0.001")
                    .spawn()
                    .unwrap(),
            );
            let pipe_into = tokio::fs::OpenOptions::new()
                .read(false)
                .write(true)
                .create(false)
                .open(&fifo_path)
                .await
                .unwrap();
            ifh = Rc::new(RefCell::new(tokio::io::stdin()));
            ofh = Rc::new(RefCell::new(pipe_into));
        }
        AsciinemaMode::None => {
            ifh = Rc::new(RefCell::new(tokio::io::stdin()));
            ofh = Rc::new(RefCell::new(tokio::io::stdout()));
        }
    };

    if let Some(url) = cmdline.value_of("url") {
        let ws_cxn = client::new_connect(url.to_owned(), ifh.clone(), ofh.clone());

        match asciinema_mode {
            AsciinemaMode::Publisher => {
                /*
                 * Mostly interested in the recording process
                 */
                ws_cxn.await;
                asciinema_proc
                    .unwrap()
                    .wait_with_output()
                    .await
                    .expect("asciinema");
                asciinema_proc = None;
            }
            _ => {
                /*
                 * Just wait for the websocket to close
                 */
                ws_cxn.await;
            }
        }
    }

    if let Some(proc) = asciinema_proc {
        proc.wait_with_output().await.expect("asciinema");
    }
}

async fn run_server(listen_sockaddr: SocketAddr) -> Result<()> {
    info!("listen on {:?}", listen_sockaddr);

    let server = Server::new(&listen_sockaddr);

    let publish_uri = server.publish_uri();
    let client_uri = server.client_uri();

    println!("Publishing path: {:?}", publish_uri);
    println!("    Client path: {:?}", client_uri);

    server.run().await?;

    Ok(())
}

fn get_listen_sockaddr(cmdline: &clap::ArgMatches) -> SocketAddr {
    const ARGNAME: &str = "listen-addr";

    match cmdline.value_of(ARGNAME) {
        Some(arg) => match arg.parse::<SocketAddr>() {
            Ok(sockaddr) => sockaddr,
            Err(_) => match arg.parse::<IpAddr>() {
                // Perhaps it's just an IP address
                Ok(addr) => SocketAddr::from((addr, DEFAULT_LISTEN_PORT)),
                Err(_) => match arg.parse::<u16>() {
                    // Perhaps it's just a port address
                    Ok(port) => SocketAddr::from((DEFAULT_LISTEN_ADDR, port)),
                    Err(_) => clap::Error::with_description(
                        format!("unable to parse {}", ARGNAME).as_str(),
                        clap::ErrorKind::InvalidValue,
                    )
                    .exit(),
                },
            },
        },
        None => SocketAddr::from((DEFAULT_LISTEN_ADDR, DEFAULT_LISTEN_PORT)),
    }
}
