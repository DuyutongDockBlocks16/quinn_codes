//! This example demonstrates an HTTP client that requests files from a server.
//!
//! Checkout the `README.md` for guidance.

use std::{
    fs,
    io::{self, Write},
    net::ToSocketAddrs,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{anyhow, Result};
use clap::Parser;
use tracing::{error, info};
use url::Url;
use quinn::{ClientConfig};
use qlog;
use std::fs::File;
use qlog::events::connectivity::ConnectionStarted;
use qlog::events::quic::{PacketHeader, PacketReceived, PacketType};
use chrono::{Local, Datelike, Timelike};

mod common;

/// HTTP/0.9 over QUIC client
#[derive(Parser, Debug)]
#[clap(name = "client")]
struct Opt {
    /// Perform NSS-compatible TLS key logging to the file specified in `SSLKEYLOGFILE`.
    #[clap(long = "keylog")]
    keylog: bool,

    url: Url,

    /// Override hostname used for certificate verification
    #[clap(long = "host")]
    host: Option<String>,

    /// Custom certificate authority to trust, in DER format
    #[clap(long = "ca")]
    ca: Option<PathBuf>,

    /// Simulate NAT rebinding after connecting
    #[clap(long = "rebind")]
    rebind: bool,
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
    let url = options.url;
    let url_host = strip_ipv6_brackets(url.host_str().unwrap());
    let remote = (url_host, url.port().unwrap_or(4433))
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| anyhow!("couldn't resolve to an address"))?;

    let now = Local::now();

    let formatted_time = now.format("%Y%m%d%H%M%S").to_string();

    let file_name = format!("client_{}.qlog", formatted_time);

    // 创建 Qlog 流处理器
    let qlog_file = File::create(file_name).unwrap();


    let trace = qlog::TraceSeq::new(
       qlog::VantagePoint {
           name: Some("Client".to_string()),
          ty: qlog::VantagePointType::Client,
           flow: None,
      },
       Some("Client qlog trace".to_string()),
      Some("Client qlog trace description".to_string()),
      Some(qlog::Configuration {
          time_offset: Some(0.0),
          original_uris: None,
      }),
       None,
    );

    let mut streamer = qlog::streamer::QlogStreamer::new(
        qlog::QLOG_VERSION.to_string(),
        Some("Client Qlog".to_string()),
        Some("Client Qlog description".to_string()),
        None,
        std::time::Instant::now(),
        trace,
        qlog::events::EventImportance::Extra,
        Box::new(qlog_file),
    );

    streamer.start_log().ok();

    //
    // let mut roots = rustls::RootCertStore::empty();
    // if let Some(ca_path) = options.ca {
    //     roots.add(&rustls::Certificate(fs::read(ca_path)?))?;
    // } else {
    //     let dirs = directories_next::ProjectDirs::from("org", "quinn", "quinn-examples").unwrap();
    //     match fs::read(dirs.data_local_dir().join("cert.der")) {
    //         Ok(cert) => {
    //             roots.add(&rustls::Certificate(cert))?;
    //         }
    //         Err(ref e) if e.kind() == io::ErrorKind::NotFound => {
    //             info!("local server certificate not found");
    //         }
    //         Err(e) => {
    //             error!("failed to open local server certificate: {}", e);
    //         }
    //     }
    // }
    // let mut client_crypto = rustls::ClientConfig::builder()
    //     .with_safe_defaults()
    //     .with_root_certificates(roots)
    //     .with_no_client_auth();
    //
    // client_crypto.alpn_protocols = common::ALPN_QUIC_HTTP.iter().map(|&x| x.into()).collect();
    // if options.keylog {
    //     client_crypto.key_log = Arc::new(rustls::KeyLogFile::new());
    // }
    //
    // let client_config = quinn::ClientConfig::new(Arc::new(client_crypto));
    let mut endpoint = quinn::Endpoint::client("[::]:0".parse().unwrap())?;
    endpoint.set_default_client_config(configure_client());

    let request = format!("GET {}\r\n", url.path());

    let rebind = options.rebind;
    let host = options.host.as_deref().unwrap_or(url_host);

    eprintln!("connecting to {host} at {remote}");

    let start = Instant::now();

    let conn = endpoint
        .connect(remote, host)?
        .await
        .map_err(|e| anyhow!("failed to connect: {}", e))?;



    eprintln!("connected at {:?}", start.elapsed());

    let conn_start = Instant::now();

    let event_conn_start_duration = conn_start - start;

    let event_conn_start_time = (event_conn_start_duration.as_secs() as f32 * 1000.0)
        + (event_conn_start_duration.subsec_nanos() as f32 / 1_000_000.0);

    // 添加连接建立事件
    let event_connect = qlog::events::Event::with_time(event_conn_start_time, qlog::events::EventData::ConnectionStarted{
        0: ConnectionStarted {
            ip_version: None,
            src_ip: "".to_string(),
            dst_ip: "".to_string(),
            protocol: None,
            src_port: None,
            dst_port: None,
            src_cid: None,
            dst_cid: None,
        },

    });
    streamer.add_event(event_connect).ok();


    let (mut send, mut recv) = conn
        .open_bi()
        .await
        .map_err(|e| anyhow!("failed to open stream: {}", e))?;
    if rebind {
        let socket = std::net::UdpSocket::bind("[::]:0").unwrap();
        let addr = socket.local_addr().unwrap();
        eprintln!("rebinding to {addr}");
        endpoint.rebind(socket).expect("rebind failed");
    }

    send.write_all(request.as_bytes())
        .await
        .map_err(|e| anyhow!("failed to send request: {}", e))?;
    send.finish()
        .await
        .map_err(|e| anyhow!("failed to shutdown stream: {}", e))?;

    let response_start = Instant::now();

    let event_request_sent_duration = response_start - start;

    eprintln!("request sent at {:?}", event_request_sent_duration);

    let event_request_sent_time = (event_request_sent_duration.as_secs() as f32 * 1000.0)
        + (event_request_sent_duration.subsec_nanos() as f32 / 1_000_000.0);

    // 添加发送请求事件
    let event_request_sent = qlog::events::Event::with_time(event_request_sent_time , qlog::events::EventData::PacketSent( qlog::events::quic::PacketSent{
        header: PacketHeader {
            packet_type: PacketType::Initial,
            packet_number: None,
            flags: None,
            token: None,
            length: Some(request.as_bytes().len() as u16),
            version: None,
            scil: None,
            dcil: None,
            scid: None,
            dcid: None,
        },
        is_coalesced: None,
        retry_token: None,
        stateless_reset_token: None,
        supported_versions: None,
        raw: None,
        datagram_id: None,
        trigger: None,
        send_at_time: Some(response_start.elapsed().as_micros() as f32),
        frames: None,
    }

    ));
    streamer.add_event(event_request_sent).ok();

    let resp = recv
        .read_to_end(usize::max_value())
        .await
        .map_err(|e| anyhow!("failed to read response: {}", e))?;


    let duration = response_start.elapsed();
    eprintln!(
        "response received in {:?} - {} KiB/s",
        duration,
        resp.len() as f32 / (duration_secs(&duration) * 1024.0)
    );

    let event_resp_received_time = (duration.as_secs() as f32 * 1000.0)
        + (duration.subsec_nanos() as f32 / 1_000_000.0);

    let event_resp_received = qlog::events::Event::with_time(event_resp_received_time, qlog::events::EventData::PacketReceived(PacketReceived {
            header: PacketHeader {
                packet_type: PacketType::Initial,
                packet_number: None,
                flags: None,
                token: None,
                length: None,
                version: None,
                scil: None,
                dcil: None,
                scid: None,
                dcid: None,
            },
            is_coalesced: None,
            retry_token: None,
            stateless_reset_token: None,
            supported_versions: None,
            raw: None,
            datagram_id: None,
            trigger: None,
            frames: None,
        })
    );

    streamer.add_event(event_resp_received).ok();

    streamer.finish_log().expect("TODO: Cant finish log");

    // io::stdout().write_all(&resp).unwrap();
    io::stdout().flush().unwrap();
    conn.close(0u32.into(), b"done");

    // Give the server a fair chance to receive the close packet
    endpoint.wait_idle().await;

    Ok(())
}

fn strip_ipv6_brackets(host: &str) -> &str {
    // An ipv6 url looks like eg https://[::1]:4433/Cargo.toml, wherein the host [::1] is the
    // ipv6 address ::1 wrapped in brackets, per RFC 2732. This strips those.
    if host.starts_with('[') && host.ends_with(']') {
        &host[1..host.len() - 1]
    } else {
        host
    }
}

fn duration_secs(x: &Duration) -> f32 {
    x.as_secs() as f32 + x.subsec_nanos() as f32 * 1e-9
}


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
