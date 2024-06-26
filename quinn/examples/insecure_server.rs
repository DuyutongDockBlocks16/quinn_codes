//! This example demonstrates an HTTP server that serves files from a directory.
//!
//! Checkout the `README.md` for guidance.

use std::{
    ascii, fs, io,
    net::SocketAddr,
    path::{self, Path, PathBuf},
    str,
    sync::Arc,
};

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use tracing::{error, info, info_span};
use tracing_futures::Instrument as _;
use quinn::{ClientConfig, Endpoint};
use qlog;
use std::fs::File;
use std::time::Instant;
use chrono::Local;
use qlog::events::connectivity::ConnectionStarted;
use qlog::events::quic::{DatagramDropped, DatagramsSent};
use qlog::events::RawInfo;
use qlog::streamer::QlogStreamer;

mod common;
use common::make_server_endpoint;

#[derive(Parser, Debug)]
#[clap(name = "server")]
struct Opt {
    /// file to log TLS keys to for debugging
    #[clap(long = "keylog")]
    keylog: bool,
    /// directory to serve files from
    root: PathBuf,
    /// TLS private key in PEM format
    #[clap(short = 'k', long = "key", requires = "cert")]
    key: Option<PathBuf>,
    /// TLS certificate in PEM format
    #[clap(short = 'c', long = "cert", requires = "key")]
    cert: Option<PathBuf>,
    /// Enable stateless retries
    #[clap(long = "stateless-retry")]
    stateless_retry: bool,
    /// Address to listen on
    // #[clap(long = "listen", default_value = "[::1]:4433")] // loopback addr
    #[clap(long = "listen", default_value = "10.0.0.1:4433")] // mininet host addr
    listen: SocketAddr,
}



fn main() {
    tracing::subscriber::set_global_default(
        tracing_subscriber::FmtSubscriber::builder()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .finish(),
    )
    .unwrap();
    let opt = Opt::parse();
    let code = {
        if let Err(e) = run(opt) {
            eprintln!("ERROR: {e}");
            1
        } else {
            0
        }
    };
    ::std::process::exit(code);
}

#[tokio::main]
async fn run(options: Opt) -> Result<()> {
    // let (certs, key) = if let (Some(key_path), Some(cert_path)) = (&options.key, &options.cert) {
    //     let key = fs::read(key_path).context("failed to read private key")?;
    //     let key = if key_path.extension().map_or(false, |x| x == "der") {
    //         rustls::PrivateKey(key)
    //     } else {
    //         let pkcs8 = rustls_pemfile::pkcs8_private_keys(&mut &*key)
    //             .context("malformed PKCS #8 private key")?;
    //         match pkcs8.into_iter().next() {
    //             Some(x) => rustls::PrivateKey(x),
    //             None => {
    //                 let rsa = rustls_pemfile::rsa_private_keys(&mut &*key)
    //                     .context("malformed PKCS #1 private key")?;
    //                 match rsa.into_iter().next() {
    //                     Some(x) => rustls::PrivateKey(x),
    //                     None => {
    //                         anyhow::bail!("no private keys found");
    //                     }
    //                 }
    //             }
    //         }
    //     };
    //     let cert_chain = fs::read(cert_path).context("failed to read certificate chain")?;
    //     let cert_chain = if cert_path.extension().map_or(false, |x| x == "der") {
    //         vec![rustls::Certificate(cert_chain)]
    //     } else {
    //         rustls_pemfile::certs(&mut &*cert_chain)
    //             .context("invalid PEM-encoded certificate")?
    //             .into_iter()
    //             .map(rustls::Certificate)
    //             .collect()
    //     };
    //
    //     (cert_chain, key)
    // } else {
    //     let dirs = directories_next::ProjectDirs::from("org", "quinn", "quinn-examples").unwrap();
    //     let path = dirs.data_local_dir();
    //     let cert_path = path.join("cert.der");
    //     let key_path = path.join("key.der");
    //     let (cert, key) = match fs::read(&cert_path).and_then(|x| Ok((x, fs::read(&key_path)?))) {
    //         Ok(x) => x,
    //         Err(ref e) if e.kind() == io::ErrorKind::NotFound => {
    //             info!("generating self-signed certificate");
    //             let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    //             let key = cert.serialize_private_key_der();
    //             let cert = cert.serialize_der().unwrap();
    //             fs::create_dir_all(path).context("failed to create certificate directory")?;
    //             fs::write(&cert_path, &cert).context("failed to write certificate")?;
    //             fs::write(&key_path, &key).context("failed to write private key")?;
    //             (cert, key)
    //         }
    //         Err(e) => {
    //             bail!("failed to read certificate: {}", e);
    //         }
    //     };
    //
    //     let key = rustls::PrivateKey(key);
    //     let cert = rustls::Certificate(cert);
    //     (vec![cert], key)
    // };
    //
    // let mut server_crypto = rustls::ServerConfig::builder()
    //     .with_safe_defaults()
    //     .with_no_client_auth()
    //     .with_single_cert(certs, key)?;
    // server_crypto.alpn_protocols = common::ALPN_QUIC_HTTP.iter().map(|&x| x.into()).collect();
    // if options.keylog {
    //     server_crypto.key_log = Arc::new(rustls::KeyLogFile::new());
    // }
    //
    // let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(server_crypto));
    // let transport_config = Arc::get_mut(&mut server_config.transport).unwrap();
    // transport_config.max_concurrent_uni_streams(0_u8.into());
    // if options.stateless_retry {
    //     server_config.use_retry(true);
    // }
    //
    // let root = Arc::<Path>::from(options.root.clone());
    // if !root.exists() {
    //     bail!("root path does not exist");
    // }
    //
    // let endpoint = quinn::Endpoint::server(server_config, options.listen)?;
    // eprintln!("listening on {}", endpoint.local_addr()?);



    // let addr = "[::1]:4433".parse().unwrap(); // loopback addr
    // let (endpoint, _server_cert) = make_server_endpoint(addr).unwrap(); // loopback addr


    // let addr = "[::1]:4433".parse().unwrap(); // loopback addr
    let addr: SocketAddr = "10.0.0.1:4433".parse()?;  // mininet host addr
    let (endpoint, _server_cert) = make_server_endpoint(addr).unwrap(); // mininet host addr

    // accept a single connection
    // let incoming_conn = endpoint.accept().await.unwrap();
    // let conn = incoming_conn.await.unwrap();
    // println!(
    //     "[server] connection accepted: addr={}",
    //     conn.remote_address()
    // );

    let root = Arc::<Path>::from(options.root.clone());
    if !root.exists() {
        bail!("root path does not exist");
    }

    let start = Instant::now();

    while let Some(conn) = endpoint.accept().await {

        info!("connection incoming");

        let now = Local::now();

        let formatted_time = now.format("%Y%m%d%H%M%S").to_string();

        // let file_name = format!("server_{}.qlog", formatted_time);
        let file_name = format!("./quinn/server_log/server_{}.qlog", formatted_time);

        let qlog_file = File::create(file_name).unwrap();

        let trace = qlog::TraceSeq::new(
            qlog::VantagePoint {
                name: Some("Server".to_string()),
                ty: qlog::VantagePointType::Client,
                flow: None,
            },
            Some("Server qlog trace".to_string()),
            Some("Server qlog trace description".to_string()),
            Some(qlog::Configuration {
                time_offset: Some(0.0),
                original_uris: None,
            }),
            None,
        );

        let mut streamer = qlog::streamer::QlogStreamer::new(
            qlog::QLOG_VERSION.to_string(),
            Some("Server Qlog".to_string()),
            Some("Server Qlog description".to_string()),
            None,
            std::time::Instant::now(),
            trace,
            qlog::events::EventImportance::Extra,
            Box::new(qlog_file),
        );

        streamer.start_log().ok();


        let fut = handle_connection(root.clone(), conn, streamer, start);


        tokio::spawn(async move {
            if let Err(e) = fut.await {
                error!("connection failed: {reason}", reason = e.to_string())
            }
        });
    }


    Ok(())
}

async fn handle_connection(root: Arc<Path>, conn: quinn::Connecting, mut streamer: QlogStreamer, start_time: Instant) -> Result<()> {
    let connection = conn.await?;
    let span = info_span!(
        "connection",
        remote = %connection.remote_address(),
        protocol = %connection
            .handshake_data()
            .unwrap()
            .downcast::<quinn::crypto::rustls::HandshakeData>().unwrap()
            .protocol
            .map_or_else(|| "<none>".into(), |x| String::from_utf8_lossy(&x).into_owned())
    );

    let conn_start = Instant::now();

    let event_conn_start_duration = conn_start - start_time;

    let event_conn_start_time = (event_conn_start_duration.as_secs() as f32 * 1000.0)
        + (event_conn_start_duration.subsec_nanos() as f32 / 1_000_000.0);

    // 添加连接建立事件
    let event_connect = qlog::events::Event::with_time(event_conn_start_time, qlog::events::EventData::ConnectionStarted{
        0: ConnectionStarted {
            ip_version: None,
            src_ip: "".to_string(),
            dst_ip: connection.remote_address().to_string(),
            protocol: None,
            src_port: None,
            dst_port: None,
            src_cid: None,
            dst_cid: None,
        },

    });
    streamer.add_event(event_connect).ok();

    async {
        info!("established");

        // Each stream initiated by the client constitutes a new request.
        loop {
            let stream = connection.accept_bi().await;
            let stream = match stream {
                Err(quinn::ConnectionError::ApplicationClosed { .. }) => {
                    info!("connection closed");
                    return Ok(());
                }
                Err(e) => {
                    return Err(e);
                }
                Ok(s) => s,
            };


            let now = Local::now();

            let formatted_time = now.format("%Y%m%d%H%M%S").to_string();

            // let file_name = format!("request_{}.qlog", formatted_time);
            let file_name = format!("./quinn/request_log/request_{}.qlog", formatted_time);

            let qlog_file = File::create(file_name).unwrap();

            let trace = qlog::TraceSeq::new(
                qlog::VantagePoint {
                    name: Some("request".to_string()),
                    ty: qlog::VantagePointType::Client,
                    flow: None,
                },
                Some("request_streamer qlog trace".to_string()),
                Some("request_streamer qlog trace description".to_string()),
                Some(qlog::Configuration {
                    time_offset: Some(0.0),
                    original_uris: None,
                }),
                None,
            );

            let mut request_streamer = qlog::streamer::QlogStreamer::new(
                qlog::QLOG_VERSION.to_string(),
                Some("request_streamer Qlog".to_string()),
                Some("request_streamer Qlog description".to_string()),
                None,
                std::time::Instant::now(),
                trace,
                qlog::events::EventImportance::Extra,
                Box::new(qlog_file),
            );

            request_streamer.start_log().ok();

            let fut = handle_request(root.clone(), stream, request_streamer, start_time);


            tokio::spawn(
                async move {
                    if let Err(e) = fut.await {
                        error!("failed: {reason}", reason = e.to_string());
                    }
                }
                .instrument(info_span!("request")),
            );
        }
    }
    .instrument(span)
    .await?;
    Ok(())
}

async fn handle_request(
    root: Arc<Path>,
    (mut send, mut recv): (quinn::SendStream, quinn::RecvStream),
    mut request_streamer: QlogStreamer,
    start_time: Instant,
) -> Result<()> {
    let req = recv
        .read_to_end(64 * 1024)
        .await
        .map_err(|e| anyhow!("failed reading request: {}", e))?;
    let mut escaped = String::new();
    for &x in &req[..] {
        let part = ascii::escape_default(x).collect::<Vec<_>>();
        escaped.push_str(str::from_utf8(&part).unwrap());
    }
    info!(content = %escaped);
    // Execute the request
    let resp = process_get(&root, &req).unwrap_or_else(|e| {
        error!("failed: {}", e);
        format!("failed to process request: {e}\n").into_bytes()
    });

    // // Write the response
    // send.write_all(&resp)
    //     .await
    //     .map_err(|e| anyhow!("failed to send response: {}", e))?;

    let mut bytes_written = 0;
    while bytes_written < resp.len() {
        let result = send.write(&resp[bytes_written..]).await;
        match result {
            Ok(n) => {
                bytes_written += n;
                println!("{} bytes written", n);

                let now = Instant::now();

                let event_send_duration = now - start_time;

                let event_send_time = (event_send_duration.as_secs() as f32 * 1000.0)
                    + (event_send_duration.subsec_nanos() as f32 / 1_000_000.0);

                let raw_info = RawInfo {
                    length: Some(n as u64),             // 示例值，您可以根据需要进行设置
                    payload_length: None,    // 示例值，您可以根据需要进行设置
                    data: None,               // 示例值，您可以根据需要进行设置
                };

                let mut raw_info_vec = Vec::new();
                raw_info_vec.push(raw_info);

                // 添加连接建立事件
                let event_send = qlog::events::Event::with_time(event_send_time, qlog::events::EventData::DatagramsSent(DatagramsSent {
                        count: Some(1u16),
                        raw: Some(raw_info_vec),
                        datagram_ids: None,
                    })
                );
                request_streamer.add_event(event_send).ok();

            }
            Err(e) => {

                let now = Instant::now();

                let event_failed_send_duration = now - start_time;

                let event_failed_send_time = (event_failed_send_duration.as_secs() as f32 * 1000.0)
                    + (event_failed_send_duration.subsec_nanos() as f32 / 1_000_000.0);

                // 添加连接建立事件
                let event_failed_send = qlog::events::Event::with_time(event_failed_send_time, qlog::events::EventData::DatagramDropped(
                    DatagramDropped {
                        raw: None
                    })
                );
                request_streamer.add_event(event_failed_send).ok();

                return Err(anyhow!("failed to send response: {}", e));
            }
        }
    }

    // Gracefully terminate the stream
    send.finish()
        .await
        .map_err(|e| anyhow!("failed to shutdown stream: {}", e))?;
    info!("complete");
    Ok(())
}

fn process_get(root: &Path, x: &[u8]) -> Result<Vec<u8>> {
    if x.len() < 4 || &x[0..4] != b"GET " {
        bail!("missing GET");
    }
    if x[4..].len() < 2 || &x[x.len() - 2..] != b"\r\n" {
        bail!("missing \\r\\n");
    }
    let x = &x[4..x.len() - 2];
    let end = x.iter().position(|&c| c == b' ').unwrap_or(x.len());
    let path = str::from_utf8(&x[..end]).context("path is malformed UTF-8")?;
    let path = Path::new(&path);
    let mut real_path = PathBuf::from(root);
    let mut components = path.components();
    match components.next() {
        Some(path::Component::RootDir) => {}
        _ => {
            bail!("path must be absolute");
        }
    }
    for c in components {
        match c {
            path::Component::Normal(x) => {
                real_path.push(x);
            }
            x => {
                bail!("illegal component in path: {:?}", x);
            }
        }
    }

    println!("{:?}",real_path);

    let data = fs::read(&real_path).context("failed reading file")?;
    Ok(data)
}
