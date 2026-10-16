use anyhow::Context;
use clap::{Parser, Subcommand};
use reseller::{App, config::Config, db, identity, stats, web};
use std::{
    io::{Read, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream},
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Parser)]
#[command(
    name = "reseller",
    version,
    about = "Your API. Your pricing. One binary.",
    long_about = "OpenAI-compatible streaming proxy with self-service keys, metered credit, \
                  payments, and dashboards. Run without a subcommand to start the server."
)]
struct Cli {
    /// Starter configuration, read on first start only
    #[arg(
        long,
        env = "RESELLER_CONFIG",
        default_value = "reseller.toml",
        global = true
    )]
    config: PathBuf,
    /// SQLite database file
    #[arg(
        long,
        env = "DB_FILE",
        default_value = ".reseller/reseller.db",
        global = true
    )]
    database: PathBuf,
    #[arg(long, env = "HOST", default_value = "0.0.0.0", global = true)]
    host: IpAddr,
    #[arg(long, env = "PORT", default_value_t = 56787, global = true)]
    port: u16,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Start the proxy and embedded dashboards (default)
    Serve,
    /// Write a documented starter configuration (never overwrites a file)
    Init,
    /// Validate configuration and database connectivity
    Doctor,
    /// Print a cryptographically random administrator token
    Token,
    /// Replace the administrator token in the database (server must be stopped)
    ResetAdmin,
    /// Back up SQLite safely, including committed WAL data
    Backup { destination: PathBuf },
    /// Probe the local readiness endpoint (used by the container health check)
    Healthcheck,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "reseller=info".into()),
        )
        .with_target(false)
        .init();
    let mut cli = Cli::parse();
    match cli.command.take().unwrap_or(Command::Serve) {
        Command::Token => {
            println!("{}", identity::secret("admin_"));
            Ok(())
        }
        Command::Init => init(&cli.config),
        Command::Healthcheck => healthcheck(cli.host, cli.port),
        Command::Doctor => {
            let (pool, config) = open(&cli).await?;
            pool.close().await;
            doctor(&cli.database, &config);
            Ok(())
        }
        Command::ResetAdmin => {
            let (pool, mut config) = open(&cli).await?;
            config.admin_token = identity::secret("admin_");
            db::save_settings(&pool, &config).await?;
            pool.close().await;
            println!(
                "New administrator token: {}\nRestart reseller to use it.",
                config.admin_token
            );
            Ok(())
        }
        Command::Backup { destination } => {
            anyhow::ensure!(
                !destination.exists(),
                "backup destination {} already exists",
                destination.display()
            );
            let (pool, _) = open(&cli).await?;
            sqlx::query("VACUUM INTO ?")
                .bind(destination.to_string_lossy().as_ref())
                .execute(&pool)
                .await?;
            pool.close().await;
            println!("Backup saved to {}", destination.display());
            Ok(())
        }
        Command::Serve => {
            let (pool, config) = open(&cli).await?;
            serve(&cli, App::new(pool, config)?).await
        }
    }
}

/// Opens the database and loads (or seeds) validated settings.
async fn open(cli: &Cli) -> anyhow::Result<(sqlx::SqlitePool, Config)> {
    let seed = Config::load(&cli.config)?;
    let pool = db::open(&cli.database)
        .await
        .with_context(|| format!("opening database {}", cli.database.display()))?;
    let config = db::settings(&pool, seed).await?;
    config.validate().context("stored settings are invalid")?;
    Ok((pool, config))
}

fn init(path: &Path) -> anyhow::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    file.write_all(include_bytes!("../assets/reseller.example.toml"))?;
    println!(
        "Created {} with every option documented. Set [llm].api_key, then run reseller.",
        path.display()
    );
    Ok(())
}

/// Minimal HTTP/1.1 probe so the scratch container needs no curl.
fn healthcheck(host: IpAddr, port: u16) -> anyhow::Result<()> {
    let host = if host.is_unspecified() {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    } else {
        host
    };
    let timeout = Duration::from_secs(3);
    let mut stream = TcpStream::connect_timeout(&SocketAddr::new(host, port), timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    stream.write_all(b"GET /readyz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    let status_line = response.lines().next().unwrap_or("no response");
    anyhow::ensure!(
        status_line.starts_with("HTTP/1.1 200") || status_line.starts_with("HTTP/1.0 200"),
        "readiness probe failed: {status_line}"
    );
    Ok(())
}

fn doctor(database: &Path, config: &Config) {
    let state = |configured: bool, missing: &'static str| {
        if configured { "configured" } else { missing }
    };
    println!(
        "Database: {} (migrations current, WAL enabled)",
        database.display()
    );
    println!("Configuration: valid");
    println!(
        "Administrator authentication: {}",
        state(!config.admin_token.is_empty(), "missing")
    );
    println!(
        "LLM upstream key: {}",
        state(
            !config.llm.api_key.is_empty(),
            "missing — configure before proxying"
        )
    );
    println!(
        "Stripe: {}",
        state(
            !config.stripe_secret_key.is_empty() && !config.stripe_webhook_secret.is_empty(),
            "disabled"
        )
    );
}

async fn serve(cli: &Cli, app: App) -> anyhow::Result<()> {
    let router = web::router(app.clone()).await;
    let listener = tokio::net::TcpListener::bind(SocketAddr::new(cli.host, cli.port))
        .await
        .with_context(|| format!("binding {}:{}", cli.host, cli.port))?;
    let addr = listener.local_addr()?;
    let port = addr.port();
    println!(
        "\n  RESELLER  {}\n\n  Listening   {addr}\n  Website     http://localhost:{port}/\n  \
         API         http://localhost:{port}/v1\n  Customers   http://localhost:{port}/workspace/\n  \
         Admin       http://localhost:{port}/admin/\n  Database    {}\n\n  Press Ctrl-C to stop.\n",
        env!("CARGO_PKG_VERSION"),
        cli.database.display()
    );
    let maintenance = app.clone();
    let pruner = tokio::spawn(async move {
        let mut timer = tokio::time::interval(Duration::from_secs(60));
        loop {
            timer.tick().await;
            if let Err(e) = stats::prune(&maintenance).await {
                tracing::error!(error = %e, "log maintenance failed");
            }
        }
    });
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;
    pruner.abort();
    // Let dropped streams finish settling before closing the database.
    app.tasks.close();
    app.tasks.wait().await;
    app.db.close().await;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
    tracing::info!("draining connections and billing tasks");
}
