use clap::{Parser, Subcommand};
use reseller::{App, config::Config, db};
use std::{
    net::{IpAddr, SocketAddr},
    path::PathBuf,
};

#[derive(Parser)]
#[command(
    name = "reseller",
    version,
    about = "Your API. Your pricing. One binary.",
    long_about = "OpenAI-compatible streaming proxy with self-service keys, metered credit, payments, and dashboards. Run without a subcommand to start the server."
)]
struct Cli {
    #[arg(
        long,
        env = "RESELLER_CONFIG",
        default_value = "reseller.toml",
        global = true
    )]
    config: PathBuf,
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
    /// Start the proxy and embedded dashboards
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
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Token) => {
            println!("{}", reseller::identity::secret("admin_"));
            return Ok(());
        }
        Some(Command::Init) => {
            use std::io::Write;
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let mut f = opts.open(&cli.config)?;
            f.write_all(include_bytes!("../assets/reseller.example.toml"))?;
            println!(
                "Created {} with every option documented. Set [llm].api_key, then run reseller.",
                cli.config.display()
            );
            return Ok(());
        }
        Some(Command::Healthcheck) => {
            use std::io::{Read as _, Write as _};
            let host = if cli.host.is_unspecified() {
                IpAddr::from([127, 0, 0, 1])
            } else {
                cli.host
            };
            let address = SocketAddr::new(host, cli.port);
            let timeout = std::time::Duration::from_secs(3);
            let mut stream = std::net::TcpStream::connect_timeout(&address, timeout)?;
            stream.set_read_timeout(Some(timeout))?;
            stream.set_write_timeout(Some(timeout))?;
            stream.write_all(
                b"GET /readyz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
            )?;
            let mut response = String::new();
            stream.read_to_string(&mut response)?;
            anyhow::ensure!(
                response.starts_with("HTTP/1.1 200") || response.starts_with("HTTP/1.0 200"),
                "readiness probe failed: {}",
                response.lines().next().unwrap_or("no response")
            );
            return Ok(());
        }
        _ => {}
    }
    let seed = Config::load(&cli.config)?;
    let pool = db::open(&cli.database).await?;
    let mut config = db::settings(&pool, seed).await?;
    config.validate()?;
    match cli.command {
        Some(Command::Doctor) => {
            println!(
                "Database: {} (migrations current, WAL enabled)\nConfiguration: valid\nAdministrator authentication: {}\nLLM upstream key: {}\nStripe: {}",
                cli.database.display(),
                if config.admin_token.is_empty() {
                    "missing"
                } else {
                    "configured"
                },
                if config.llm.api_key.is_empty() {
                    "missing — configure before proxying"
                } else {
                    "configured"
                },
                if config.stripe_secret_key.is_empty() {
                    "disabled"
                } else {
                    "configured"
                }
            );
            pool.close().await;
            return Ok(());
        }
        Some(Command::ResetAdmin) => {
            config.admin_token = reseller::identity::secret("admin_");
            sqlx::query("UPDATE settings SET value=? WHERE id=1")
                .bind(serde_json::to_string(&config)?)
                .execute(&pool)
                .await?;
            println!(
                "New administrator token: {}\nRestart reseller to use it.",
                config.admin_token
            );
            pool.close().await;
            return Ok(());
        }
        Some(Command::Backup { destination }) => {
            anyhow::ensure!(!destination.exists(), "backup destination already exists");
            sqlx::query("VACUUM INTO ?")
                .bind(destination.to_string_lossy().as_ref())
                .execute(&pool)
                .await?;
            println!("Backup saved to {}", destination.display());
            pool.close().await;
            return Ok(());
        }
        _ => {}
    }
    if config.admin_token.is_empty() {
        anyhow::bail!("Administrator token is missing; run reseller reset-admin");
    }
    let app = App::new(pool, config)?;
    let router = reseller::web::router(app.clone()).await;
    let listener = tokio::net::TcpListener::bind(SocketAddr::new(cli.host, cli.port)).await?;
    let addr = listener.local_addr()?;
    println!(
        "\n  RESELLER  {}\n\n  Listening   {addr}\n  Website     http://localhost:{}/\n  API         http://localhost:{}/v1\n  Customers   http://localhost:{}/workspace/\n  Admin       http://localhost:{}/admin/\n  Database    {}\n\n  Press Ctrl-C to stop.\n",
        env!("CARGO_PKG_VERSION"),
        addr.port(),
        addr.port(),
        addr.port(),
        addr.port(),
        cli.database.display()
    );
    let maintenance = app.clone();
    let task = tokio::spawn(async move {
        let mut timer = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            timer.tick().await;
            if let Err(e) = reseller::stats::prune(&maintenance).await {
                tracing::error!(error=%e,"log maintenance failed");
            }
        }
    });
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown())
    .await?;
    task.abort();
    app.tasks.close();
    app.tasks.wait().await;
    app.db.close().await;
    Ok(())
}
async fn shutdown() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {_=ctrl_c=>{},_=terminate=>{}}
    tracing::info!("draining connections and billing tasks");
}
