//std
use std::net::SocketAddr;
use std::sync::Arc;

//tokio
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

use clap::Parser;
use clap::Subcommand;

mod common;
use common::make_server_endpoint;

use quinn::{ClientConfig, Connection, Endpoint, RecvStream, SendStream};


#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    mode: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Server {
        #[arg(short, long, required(true))]
        addr: String,
    },
    Client {
        #[arg(short, long, required(true))]
        remote: String,
        #[arg(short, long, required(true))]
        socks5: String,
    },
}

async fn run_server(addr: String) {
    //create Endpoint
    let (endpoint, _server_cert) =
        make_server_endpoint(addr.parse::<SocketAddr>().unwrap()).unwrap();
    eprintln!("[server] listening udp: addr={}", addr);
    //new quic connection

    loop {
        match endpoint.accept().await.unwrap().await {
            Ok(conn) => {
                eprintln!(
                    "[server] connection accepted: addr={}",
                    conn.remote_address()
                );
                accept_streams(conn).await;
            }
            Err(_) => {
                eprintln!("[server] connecting failed",);
            }
        }
    }
}

async fn accept_streams(conn: Connection) {
    loop {
        match conn.close_reason() {
            Some(_e) => {
                eprintln!("[server] [ConnectionError] {}", _e.to_string());
                break;
            }
            _ => {
                match conn.accept_bi().await {
                    Ok((s, r)) => {
                        tokio::spawn(async move { exchange_data(s, r).await });
                    }
                    Err(_e) => {}
                }
            }
        }
    }
}

async fn exchange_data(mut sstream: SendStream, mut rstream: RecvStream) {
    let mut buffer = [0u8; 1024];

    match rstream.read(&mut buffer).await {
        Ok(size_opt) => {
            match size_opt {
                Some(n) => {
                    let received_data = String::from_utf8_lossy(&buffer[..n]);
                    eprintln!("[server] Received dest: {}, sz:{}", received_data, n);

                    let dest = received_data.clone().to_string();

                    //dial
                    let mut server_socket = match TcpStream::connect(dest).await {
                        Ok(socket) => socket,
                        Err(_e) => {
                            eprintln!("[server] TcpStream::connect error {:?}!", _e);
                            return;
                        }
                    };

                    match sstream.write(&[0u8; 1]).await {
                        Ok(_s) => {}
                        Err(_e) => return,
                    }

                    let (mut server_reader, mut server_writer) = server_socket.split();

                    //begin bicopy
                    let s2c_f = tokio::io::copy(&mut server_reader, &mut sstream);
                    let c2s_f = tokio::io::copy(&mut rstream, &mut server_writer);

                    tokio::select! {
                        result = c2s_f => {
                            if result.is_err() {
                                eprintln!("[remote server] Error copying data from client to server");
                                return
                            }
                        }
                        result = s2c_f => {
                            if result.is_err() {
                                //sstream.finish();
                                eprintln!("[remote server] Error copying data from server to client");
                                return
                            }
                        }
                    }
                }
                None => {
                    eprintln!("[server] stream finished! ");
                }
            }
        }
        Err(_e) => {
            eprintln!("[server] rstream.read error: {:?}.", _e);
        }
    }
}

async fn verify_connection(conn: Arc<Mutex<quinn::Connection>>, ep: Endpoint, srv: SocketAddr) {
    loop {
        {
            let mut rc = conn.lock().await;
            match rc.close_reason() {
                Some(conn_err) => {
                    eprintln!("[client] {}", conn_err);
                    //reconnect
                    *rc = loop {
                        match ep.connect(srv, "localhost") {
                            Ok(incoming) => match incoming.await {
                                Ok(new_conn) => {
                                    break new_conn;
                                }
                                Err(_e) => {}
                            },
                            Err(_e) => {}
                        }
                    };
                }
                None => {
                    //conn alive
                }
            }
        }
        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
    }
}

async fn run_client(socks5: &str, srv: &str) {
    let srv_addr = srv.parse::<SocketAddr>().unwrap();

    // handle socks5 requests
    let listener = TcpListener::bind(&socks5)
        .await
        .expect("Failed to bind to address!");

    eprintln!("SOCKS5 proxy listening on {}", &socks5);

    //quic local udp listening
    let mut ep = Endpoint::client("0.0.0.0:0".parse().unwrap()).unwrap();
    ep.set_default_client_config(configure_client());

    let init_conn = loop {
        match ep.connect(srv_addr, "localhost") {
            Ok(incoming) => match incoming.await {
                Ok(conn) => {
                    break Arc::new(Mutex::new(conn));
                }
                Err(_e) => {}
            },
            Err(_e) => {}
        }
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    };

    let conn_clone1 = init_conn.clone();
    tokio::spawn(async move {
        verify_connection(conn_clone1, ep, srv_addr).await;
    });

    loop {
        let conn_clone2 = init_conn.clone();
        match listener.accept().await {
            Ok((socket, _)) => {
                tokio::spawn(async move {
                    handle_client(socket, conn_clone2).await;
                });
            }
            Err(e) => eprintln!("Error socks5 accepting connection: {:?}", e),
        }
    }
}

async fn handle_client(mut client_socket: TcpStream, conn: Arc<Mutex<Connection>>) {
    let mut buffer = [0u8; 2048];

    // Read the initial handshake message
    if client_socket.read(&mut buffer).await.is_err() {
        return;
    }
    if client_socket.write_all(&[5, 0]).await.is_err() {
        return;
    }

    // Read the actual request
    if client_socket.read(&mut buffer).await.is_err() {
        return;
    }
    // Check if it's a SOCKS5 CONNECT request
    if buffer[0] != 5 || buffer[1] != 1 {
        return;
    }

    // Extract destination address and port
    let address = match buffer[3] {
        1 => format!(
            "{}.{}.{}.{}:{}",
            buffer[4],
            buffer[5],
            buffer[6],
            buffer[7],
            u16::from_be_bytes([buffer[8], buffer[9]])
        ),
        3 => {
            let domain_len = buffer[4] as usize;
            let domain = String::from_utf8_lossy(&buffer[5..(5 + domain_len)]).to_string();
            let port = u16::from_be_bytes([buffer[5 + domain_len], buffer[6 + domain_len]]);
            eprintln!("{}:{}", domain, port);
            format!("{}:{}", domain, port)
        }
        _ => return,
    };

    eprintln!("[client] {}", address);

    let (mut sstream, mut rstream) = loop {
        let rc = conn.lock().await;
        match rc.open_bi().await {
            Ok((mut _sstream, mut _rstream)) => {
                break (_sstream, _rstream);
            }
            Err(_e) => {
                eprintln!("[client] open bidstreams error {:?}", _e);
            }
        }
    };

    eprintln!("[client] quic_conn.lock().await.open_bi().await");

    eprintln!(
        "[client] bistream established! sid:{} rid:{}",
        sstream.id(),
        rstream.id()
    );

    sstream.write_all(address.as_bytes()).await.unwrap();

    if client_socket
        .write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0])
        .await
        .is_err()
    {
        return;
    }

    let mut rbuf = [0u8; 256];

    match rstream.read(&mut rbuf).await {
        Ok(op) => match op {
            Some(n) => {
                eprintln!("[client] rcv sz: {}", n);
            }
            None => return,
        },
        Err(_e) => return,
    }

    let (mut client_reader, mut client_writer) = client_socket.split();

    let c2s_f = tokio::io::copy(&mut client_reader, &mut sstream);
    let s2c_f = tokio::io::copy(&mut rstream, &mut client_writer);

    tokio::select! {
        result = c2s_f => {
            if result.is_err() {
                eprintln!("Error copying data from client to server");
            }
        }
        result = s2c_f => {
            if result.is_err() {
                //sstream.finish();
                eprintln!("Error copying data from server to client");
            }
        }
    }

    match sstream.finish().await {
        Ok(_o) => {}
        Err(_e) => {
            eprintln!("[client] sstream is closed");
        }
    }
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    match &cli.mode {
        Commands::Server { addr } => {
            eprintln!("Server addr: {:?}", addr);
            let addr = addr
                .to_string()
                .parse::<SocketAddr>()
                .expect("IP:port format error!");
            run_server(addr.to_string()).await;
        }
        Commands::Client { socks5, remote } => {
            eprintln!("Socks5 addr: {:?}", socks5);
            eprintln!("Remote addr: {:?}", remote);
            run_client(socks5, remote).await;
        }
    }
}


//quinn examples
struct SkipServerVerification;

impl SkipServerVerification {
    fn new() -> Arc<Self> {
        Arc::new(Self)
    }
}

impl rustls::client::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::Certificate,
        _intermediates: &[rustls::Certificate],
        _server_name: &rustls::ServerName,
        _scts: &mut dyn Iterator<Item = &[u8]>,
        _ocsp_response: &[u8],
        _now: std::time::SystemTime,
    ) -> Result<rustls::client::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::ServerCertVerified::assertion())
    }
}

fn configure_client() -> ClientConfig {
    let crypto = rustls::ClientConfig::builder()
        .with_safe_defaults()
        .with_custom_certificate_verifier(SkipServerVerification::new())
        .with_no_client_auth();

    ClientConfig::new(Arc::new(crypto))
}
