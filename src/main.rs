use anyhow::Context;
use clap::{Parser, Subcommand};
use reseller::{
    App,
    config::{Config, SECRET_PATHS},
    db, identity, stats, web,
};
use std::{
    io::{Read, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream},
    path::{Path, PathBuf},
    time::Duration,
};
use tracing_subscriber::fmt::writer::BoxMakeWriter;

#[derive(Parser)]
#[command(
    name = "reseller",
    version,
    about = "Your API. Your pricing. One binary.",
    long_about = "OpenAI-compatible streaming proxy with self-service keys, metered credit, \
                  payments, and dashboards. Run without a subcommand to start the server."
)]
struct Cli {
    /// Starter config file, read only when the database has no settings yet
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
    /// Address to listen on (`healthcheck` probes the same address)
    #[arg(long, env = "HOST", default_value = "0.0.0.0", global = true)]
    host: IpAddr,
    /// Port to listen on
    #[arg(long, env = "PORT", default_value_t = 56787, global = true)]
    port: u16,
    /// Directory with white-label files (index.html, theme.css, logos,
    /// fonts) served under /theme/
    #[arg(long, env = "RESELLER_THEME_DIR", global = true)]
    theme_dir: Option<PathBuf>,
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
    /// Back up the database to a new file, including committed WAL data
    /// (the destination must not exist)
    Backup { destination: PathBuf },
    /// Probe the local readiness endpoint (used by the container health check)
    Healthcheck,
    /// Run a local MCP server on stdio that forwards to a remote reseller
    /// (credentials and signup proof of work stay on this machine)
    Mcp {
        /// Origin of the reseller server, e.g. https://reseller.example.com
        #[arg(long, env = "RESELLER_URL")]
        url: String,
        /// Credential file [default: $XDG_CONFIG_HOME/reseller/credentials.json
        /// or ~/.config/reseller/credentials.json]
        #[arg(long, env = "RESELLER_CREDENTIALS")]
        credentials: Option<PathBuf>,
        /// Directory where generated audio and images are saved
        #[arg(long, env = "RESELLER_OUTPUT_DIR", default_value = "reseller-output")]
        output_dir: PathBuf,
        /// API key or dashboard token to use without storing it
        #[arg(long, env = "RESELLER_TOKEN", hide_env_values = true)]
        token: Option<String>,
    },
    /// Export or replace the stored settings (server must be stopped to import)
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Print the stored settings as TOML (secrets blanked unless requested)
    Export {
        /// Include secret values (tokens, pepper, API keys) in the output
        #[arg(long)]
        include_secrets: bool,
    },
    /// Validate a TOML file and replace the stored settings with it. Blank
    /// secrets (tokens, pepper, API keys) keep their stored values.
    Import { file: PathBuf },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut cli = Cli::parse();
    // The stdio bridge owns stdout for protocol messages: log to stderr only.
    let writer = if matches!(cli.command, Some(Command::Mcp { .. })) {
        BoxMakeWriter::new(std::io::stderr)
    } else {
        BoxMakeWriter::new(std::io::stdout)
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "reseller=info".into()),
        )
        .with_target(false)
        .with_writer(writer)
        .init();
    match cli.command.take().unwrap_or(Command::Serve) {
        Command::Token => {
            println!("{}", identity::secret("admin_"));
            Ok(())
        }
        Command::Init => init(&cli.config),
        Command::Mcp {
            url,
            credentials,
            output_dir,
            token,
        } => {
            reseller::mcp::bridge::run(reseller::mcp::bridge::Options {
                url,
                credentials,
                output_dir,
                token,
            })
            .await
        }
        Command::Healthcheck => healthcheck(cli.host, cli.port),
        Command::Doctor => {
            let (pool, config) = open(&cli).await?;
            pool.close().await;
            doctor(&cli, &config);
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
        Command::Config { action } => {
            let (pool, current) = open(&cli).await?;
            let result = match action {
                ConfigAction::Export { include_secrets } => {
                    print!("{}", current.to_toml(include_secrets)?);
                    Ok(())
                }
                ConfigAction::Import { file } => import(&pool, &current, &file).await,
            };
            pool.close().await;
            result
        }
        Command::Serve => {
            if let Some(dir) = &cli.theme_dir {
                anyhow::ensure!(dir.is_dir(), "theme directory {} not found", dir.display());
            }
            let (pool, config) = open(&cli).await?;
            let app = App::new(pool, config)?.with_theme_dir(cli.theme_dir.clone());
            serve(&cli, app).await
        }
    }
}

/// Replaces the stored settings with a validated TOML file.
async fn import(pool: &sqlx::SqlitePool, current: &Config, file: &Path) -> anyhow::Result<()> {
    let mut merged = serde_json::to_value(Config::from_toml_file(file)?)?;
    let stored = serde_json::to_value(current)?;
    // Blank secrets (as in `config export` output) keep their stored values.
    for path in SECRET_PATHS {
        if let (Some(slot), Some(kept)) = (merged.pointer_mut(path), stored.pointer(path))
            && slot.as_str() == Some("")
        {
            *slot = kept.clone();
        }
    }
    let mut imported: Config = serde_json::from_value(merged)?;
    // A blank provider key keeps the stored key of the provider with that id.
    imported.keep_provider_keys(current);
    imported
        .validate()
        .with_context(|| format!("{} is invalid", file.display()))?;
    db::save_settings(pool, &imported).await?;
    if imported.key_pepper != current.key_pepper {
        println!("Warning: key_pepper changed. Existing keys and unredeemed codes no longer work.");
    }
    println!(
        "Imported settings from {}. Restart reseller to apply them.",
        file.display()
    );
    Ok(())
}

/// Opens the database and loads (or, on first start, seeds) validated
/// settings. The seed file is only read when the database has no settings.
async fn open(cli: &Cli) -> anyhow::Result<(sqlx::SqlitePool, Config)> {
    let pool = db::open(&cli.database)
        .await
        .with_context(|| format!("opening database {}", cli.database.display()))?;
    let config = match db::stored_settings(&pool).await? {
        Some(stored) => stored,
        None => db::settings(&pool, Config::load(&cli.config)?).await?,
    };
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
    file.write_all(include_bytes!("../reseller.example.toml"))?;
    println!(
        "Created {} with every option documented. Add a [[providers]] entry and a [[models]] entry, then run reseller.",
        path.display()
    );
    Ok(())
}

/// Probes `/readyz` over plain HTTP/1.1, so the scratch container needs no curl.
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

fn doctor(cli: &Cli, config: &Config) {
    let state = |configured: bool, missing: &'static str| {
        if configured { "configured" } else { missing }
    };
    println!(
        "Database: {} (migrations current, WAL enabled)",
        cli.database.display()
    );
    println!("Configuration: valid");
    println!(
        "Administrator authentication: {}",
        state(!config.admin_token.is_empty(), "missing")
    );
    if config.providers.is_empty() {
        println!("Providers: none (add a [[providers]] entry before proxying)");
    }
    for provider in &config.providers {
        let key = if provider.api_key.is_empty() {
            "no API key"
        } else {
            "key configured"
        };
        let state = if provider.enabled { "" } else { ", disabled" };
        println!(
            "Provider {}: {} ({key}{state})",
            provider.id, provider.base_url
        );
    }
    for model in &config.models {
        let routes: Vec<String> = model
            .routes
            .iter()
            .map(|r| format!("{}/{}", r.provider, r.model))
            .collect();
        println!(
            "Model {} [{}, {}{}{}]: {}",
            model.id,
            model.tier.as_str(),
            model.capability().as_str(),
            if model.default { ", default" } else { "" },
            if model.enabled { "" } else { ", disabled" },
            routes.join(", ")
        );
    }
    if config.models.is_empty() {
        println!("Models: none (add a [[models]] entry; nothing is served without one)");
    }
    println!(
        "Free tier: {}",
        match (config.free.enabled, config.offers_free_tier()) {
            (false, _) => "disabled (anonymous requests are refused)".to_owned(),
            (true, false) => "enabled, but no model has tier = \"free\"".to_owned(),
            (true, true) => format!(
                "keyless access ({} requests per minute, {} per day per client)",
                config.free.rpm, config.free.rpd
            ),
        }
    );
    println!(
        "Signup: {}",
        match (config.signup.enabled, config.signup.pow_max_number) {
            (false, _) => "disabled".to_owned(),
            (true, 0) => "open (no proof of work)".to_owned(),
            (true, n) => format!("open (proof of work up to {n})"),
        }
    );
    println!("Stripe: {}", state(config.stripe.enabled(), "disabled"));
    println!(
        "Payroof crypto payments: {}",
        if config.payroof.enabled() {
            format!("configured (project {})", config.payroof.project)
        } else {
            "disabled (set payroof.project, token and webhook_secret)".into()
        }
    );
    println!("Branding: {}", config.branding.name);
    match &cli.theme_dir {
        Some(dir) if dir.is_dir() => println!("Theme directory: {}", dir.display()),
        Some(dir) => println!("Theme directory: {} (NOT FOUND)", dir.display()),
        None => println!("Theme directory: none"),
    }
}

async fn serve(cli: &Cli, app: App) -> anyhow::Result<()> {
    let interrupted = db::close_interrupted_requests(&app.db).await?;
    if interrupted > 0 {
        tracing::warn!(
            count = interrupted,
            "closed requests interrupted by the previous shutdown"
        );
    }
    // A restart must not hand every daily upstream budget back.
    let config = app.config().await;
    if let Err(e) = app.router.warm_start(&app.db, &config).await {
        tracing::warn!(error = %e, "could not restore route budgets from usage");
    }
    drop(config);
    let router = web::router(app.clone()).await;
    let listener = tokio::net::TcpListener::bind(SocketAddr::new(cli.host, cli.port))
        .await
        .with_context(|| format!("binding {}:{}", cli.host, cli.port))?;
    let addr = listener.local_addr()?;
    let port = addr.port();
    println!(
        "\n  {}  {}\n\n  Listening   {addr}\n  Website     http://localhost:{port}/\n  \
         API         http://localhost:{port}/v1\n  Customers   http://localhost:{port}/workspace/\n  \
         Admin       http://localhost:{port}/admin/\n  Database    {}\n\n  Press Ctrl-C to stop.\n",
        app.config().await.branding.name.to_uppercase(),
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
            if let Err(e) = reseller::billing::grant_all_due(&maintenance).await {
                tracing::error!(error = %e, "monthly quota grant failed");
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
