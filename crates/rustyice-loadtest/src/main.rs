//! rustyice-loadtest — open N concurrent listeners against a running rustyice
//! server and report per-second throughput plus dropped connections.
//!
//! Excluded from default workspace builds — invoke explicitly:
//!
//!   cargo run --release -p rustyice-loadtest -- http://localhost:8000/stream -n 1000 -d 60
//!
//! Raise the fd limit before going past a few hundred listeners:
//!
//!   ulimit -n 65535

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use clap::Parser;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[derive(Parser, Debug)]
#[command(about = "Open N concurrent rustyice listeners and report throughput + drops.")]
struct Args {
    /// Target stream URL, e.g. http://localhost:8000/stream
    url: String,

    /// Number of concurrent listeners to hold open
    #[arg(short = 'n', long, default_value_t = 100)]
    listeners: usize,

    /// Ramp-up window in seconds (listeners dialed evenly over this period)
    #[arg(short = 'r', long, default_value_t = 5)]
    ramp_secs: u64,

    /// Hold duration in seconds (after ramp completes)
    #[arg(short = 'd', long, default_value_t = 60)]
    duration_secs: u64,
}

#[derive(Default)]
struct Stats {
    connected: AtomicUsize,
    connect_errors: AtomicU64,
    drops_eof: AtomicU64,
    drops_err: AtomicU64,
    bytes_read: AtomicU64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let (host, port, path) = parse_url(&args.url)?;
    let target = format!("{host}:{port}");
    let stats = Arc::new(Stats::default());

    let stagger = if args.listeners > 0 && args.ramp_secs > 0 {
        Duration::from_millis((args.ramp_secs * 1000) / args.listeners as u64)
    } else {
        Duration::ZERO
    };

    println!(
        "dialing {} listeners against {} (ramp {}s, hold {}s)",
        args.listeners, args.url, args.ramp_secs, args.duration_secs,
    );

    let ramp_start = Instant::now();
    for _ in 0..args.listeners {
        let stats = Arc::clone(&stats);
        let target = target.clone();
        let host = host.clone();
        let path = path.clone();
        tokio::spawn(async move {
            run_listener(&target, &host, &path, &stats).await;
        });
        if !stagger.is_zero() {
            tokio::time::sleep(stagger).await;
        }
    }
    println!(
        "ramp done in {:.1}s — holding open",
        ramp_start.elapsed().as_secs_f64(),
    );
    println!("time      connected    rx KiB/s  drop_eof  drop_err  connect_err");

    let t0 = Instant::now();
    let deadline = t0 + Duration::from_secs(args.duration_secs);
    let mut prev_bytes = 0u64;
    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let now_bytes = stats.bytes_read.load(Ordering::Relaxed);
        let delta = now_bytes.saturating_sub(prev_bytes);
        prev_bytes = now_bytes;
        println!(
            "{:>5.1}s    {:>9}   {:>9}  {:>8}  {:>8}  {:>11}",
            t0.elapsed().as_secs_f64(),
            stats.connected.load(Ordering::Relaxed),
            delta / 1024,
            stats.drops_eof.load(Ordering::Relaxed),
            stats.drops_err.load(Ordering::Relaxed),
            stats.connect_errors.load(Ordering::Relaxed),
        );
    }

    println!("\n=== final ===");
    println!("connected:        {}", stats.connected.load(Ordering::Relaxed));
    println!("connect errors:   {}", stats.connect_errors.load(Ordering::Relaxed));
    println!("drops (eof):      {}", stats.drops_eof.load(Ordering::Relaxed));
    println!("drops (err):      {}", stats.drops_err.load(Ordering::Relaxed));
    println!("total bytes read: {}", stats.bytes_read.load(Ordering::Relaxed));

    Ok(())
}

async fn run_listener(target: &str, host: &str, path: &str, stats: &Stats) {
    let stream = match TcpStream::connect(target).await {
        Ok(s) => s,
        Err(_) => {
            stats.connect_errors.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };
    let (mut rd, mut wr) = stream.into_split();
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: rustyice-loadtest\r\nIcy-MetaData: 0\r\nAccept: */*\r\n\r\n"
    );
    if wr.write_all(req.as_bytes()).await.is_err() {
        stats.drops_err.fetch_add(1, Ordering::Relaxed);
        return;
    }

    // Consume response headers; count any body bytes that came in the same read.
    let mut buf = [0u8; 8192];
    let mut hdr = Vec::with_capacity(512);
    let initial_body_bytes = loop {
        let n = match rd.read(&mut buf).await {
            Ok(0) => {
                stats.drops_eof.fetch_add(1, Ordering::Relaxed);
                return;
            }
            Ok(n) => n,
            Err(_) => {
                stats.drops_err.fetch_add(1, Ordering::Relaxed);
                return;
            }
        };
        hdr.extend_from_slice(&buf[..n]);
        if let Some(pos) = find_double_crlf(&hdr) {
            break (hdr.len() - (pos + 4)) as u64;
        }
        if hdr.len() > 16 * 1024 {
            stats.drops_err.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };
    stats.bytes_read.fetch_add(initial_body_bytes, Ordering::Relaxed);
    stats.connected.fetch_add(1, Ordering::Relaxed);

    loop {
        match rd.read(&mut buf).await {
            Ok(0) => {
                stats.connected.fetch_sub(1, Ordering::Relaxed);
                stats.drops_eof.fetch_add(1, Ordering::Relaxed);
                return;
            }
            Ok(n) => {
                stats.bytes_read.fetch_add(n as u64, Ordering::Relaxed);
            }
            Err(_) => {
                stats.connected.fetch_sub(1, Ordering::Relaxed);
                stats.drops_err.fetch_add(1, Ordering::Relaxed);
                return;
            }
        }
    }
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn parse_url(s: &str) -> anyhow::Result<(String, u16, String)> {
    let s = s
        .strip_prefix("http://")
        .ok_or_else(|| anyhow::anyhow!("only http:// URLs are supported"))?;
    let (host_port, path) = match s.find('/') {
        Some(i) => (&s[..i], &s[i..]),
        None => (s, "/"),
    };
    let (host, port) = match host_port.rfind(':') {
        Some(i) => (host_port[..i].to_string(), host_port[i + 1..].parse()?),
        None => (host_port.to_string(), 80u16),
    };
    Ok((host, port, path.to_string()))
}
