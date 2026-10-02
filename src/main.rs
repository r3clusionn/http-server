use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use tinyhttp::{demo, Config, FileServer, Server};

/// A small HTTP/1.1 file server.
#[derive(Parser)]
#[command(name = "tinyhttp", version)]
struct Cli {
    /// Directory to serve
    #[arg(default_value = ".")]
    dir: PathBuf,
    /// Address to listen on (use 0.0.0.0 to serve other machines)
    #[arg(short, long, default_value = "127.0.0.1")]
    bind: String,
    #[arg(short, long, default_value_t = 8080)]
    port: u16,
    /// List the contents of directories that have no index.html
    #[arg(short, long)]
    listing: bool,
    /// Serve files whose names start with a dot
    #[arg(long)]
    hidden: bool,
    /// Worker threads, which is how many clients are served at the same time (default: 2 per CPU, at least 4)
    #[arg(short, long)]
    threads: Option<usize>,
    /// Seconds a kept-alive connection may stay idle
    #[arg(long, default_value_t = 5)]
    idle: u64,
    /// Requests served on one connection before it is closed (0 for no limit)
    #[arg(long, default_value_t = 1000)]
    max_requests: usize,
    /// Largest request body in KiB
    #[arg(long, default_value_t = 1024)]
    max_body: usize,
    /// Value of the Cache-Control header for files
    #[arg(long)]
    cache_control: Option<String>,
    /// Add the demonstration endpoints under /_/ (hello, echo, headers, stream, sleep, big, stats)
    #[arg(long)]
    demo: bool,
    /// Do not print an access log
    #[arg(short, long)]
    quiet: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let mut files = match FileServer::new(&cli.dir) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("tinyhttp: {}: {e}", cli.dir.display());
            return ExitCode::from(2);
        }
    };
    files.listing = cli.listing;
    files.hidden = cli.hidden;
    files.cache_control = cli.cache_control.clone();
    let root = files.root().to_path_buf();

    let mut config = Config::default();
    if let Some(t) = cli.threads {
        config.threads = t.max(1);
    }
    config.idle_timeout = Duration::from_secs(cli.idle);
    config.max_requests = cli.max_requests;
    config.limits.max_body = cli.max_body * 1024;
    if !cli.quiet {
        config.logger = Some(Arc::new(|e| {
            let peer = e.peer.map(|p| p.ip().to_string()).unwrap_or_else(|| "-".into());
            println!("{peer} \"{} {}\" {} {}B {:.2}ms", e.method, e.target, e.status, e.bytes, e.elapsed.as_secs_f64() * 1000.0);
        }));
    }
    let router = if cli.demo { demo::router(Some(config.stats.clone())) } else { tinyhttp::Router::new() };
    let files = Arc::new(files);
    let router = router.fallback(move |req, _| files.handle(req));

    let server = match Server::start((cli.bind.as_str(), cli.port), router, config) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("tinyhttp: cannot listen on {}:{}: {e}", cli.bind, cli.port);
            return ExitCode::from(2);
        }
    };
    println!("serving {} on http://{} (Ctrl+C to stop)", root.display(), server.local_addr());
    let (tx, rx) = std::sync::mpsc::channel();
    if ctrlc::set_handler(move || {
        let _ = tx.send(());
    })
    .is_err()
    {
        eprintln!("tinyhttp: cannot install the Ctrl+C handler");
    }
    let _ = rx.recv();
    println!("shutting down");
    server.shutdown();
    ExitCode::SUCCESS
}
