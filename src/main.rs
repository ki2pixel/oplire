use clap::{Parser, Subcommand};
use colored::Colorize;
use oplire_reset::{AppConfig, ProxyPool};
use std::fs;
use std::path::PathBuf;
use std::process::Command;

const VERSION: &str = "3.0.0";

#[derive(Parser, Debug)]
#[command(name = "oplire")]
#[command(version = VERSION)]
#[command(
    about = "OpenCode Proxy Pool Manager",
    long_about = "High-performance proxy pool manager and rate-limit rotation tool for OpenCode (CLI & Desktop)."
)]
struct Cli {
    /// Enable verbose debug logging (or set RUST_LOG=debug)
    #[arg(short, long, global = true)]
    verbose: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Install OpenCode dependencies and assistant tools
    Install {
        #[command(subcommand)]
        target: InstallTarget,
    },
    /// Show version, author, and system information
    About {},
    /// Start local HTTP CONNECT forward proxy server
    Proxy {
        /// Socket address to listen on (e.g. 127.0.0.1:8080)
        #[arg(long)]
        listen: Option<String>,
        /// Custom proxy list file to load
        #[arg(long)]
        proxy_file: Option<PathBuf>,
    },
    /// Monitor OpenCode logs in real time and rotate proxies on rate limits
    Watch {
        /// OpenCode Zen upstream URL to monitor when logs are unavailable
        #[arg(long)]
        upstream: Option<String>,
        /// Explicit path to OpenCode log file (default: auto-detected)
        #[arg(long)]
        log_file: Option<PathBuf>,
        /// Custom proxy list file to rotate
        #[arg(long)]
        proxy_file: Option<PathBuf>,
        /// Debounce delay in milliseconds after rotation before resuming checks
        #[arg(long, default_value_t = oplire_reset::watch::DEFAULT_DEBOUNCE_MS)]
        delay_ms: u64,
    },
    /// Run proxy server and log watcher together as a background daemon
    Daemon {
        /// Socket address to listen on (e.g. 127.0.0.1:8080)
        #[arg(long)]
        listen: Option<String>,
        /// Custom proxy list file to load
        #[arg(long)]
        proxy_file: Option<PathBuf>,
    },
    /// Manage residential and datacenter proxies (list, test, next, import, fetch)
    Proxies {
        #[command(subcommand)]
        action: ProxiesAction,
    },
    /// Print environment variable export commands for OpenCode
    Env {
        /// Proxy listen address to reference in export commands
        #[arg(long, default_value = "127.0.0.1:8080")]
        listen: String,
    },
    /// Manage oplire configuration file (~/.config/oplire/config.json)
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Diagnose OpenCode installation, upstream reachability, and network ports
    Doctor {},
    /// Interactive guided setup wizard for all required components
    Setup {},
    /// List available upstream models from OpenCode Zen
    Models {
        /// OpenCode Zen upstream URL
        #[arg(long, default_value = "http://localhost:3000")]
        upstream: String,
    },
}

#[derive(Subcommand, Debug)]
enum ProxiesAction {
    /// List all configured proxies and highlight the active one
    List {},
    /// Test connectivity and exit IP for all proxies in the pool
    Test {},
    /// Rotate to the next proxy in the pool
    Next {},
    /// Import proxies from a file (e.g. proxiware-isp.txt) into ~/.config/oplire/proxies.txt
    Import {
        /// Path to proxy list file
        file: PathBuf,
    },
    /// Fetch static ISP proxies from Proxiware API
    Fetch {
        /// Proxiware API key (or set via PROXIWARE_API_KEY environment variable)
        #[arg(long, env = "PROXIWARE_API_KEY")]
        api_key: String,
    },
}

#[derive(Subcommand, Debug)]
enum InstallTarget {
    /// Install OpenCode desktop app
    Opencode {},
}

#[derive(Subcommand, Debug)]
enum ConfigAction {
    /// Show current configuration values and config file path
    Show {},
    /// Set persistent configuration values
    Set {
        /// Default listen address (e.g. 127.0.0.1:8080)
        #[arg(long)]
        listen: Option<String>,
        /// Default OpenCode Zen upstream URL (e.g. http://localhost:3000)
        #[arg(long)]
        upstream: Option<String>,
    },
    /// Reset configuration to default values
    Reset {},
}

fn config_path() -> PathBuf {
    let mut path = dirs_config_dir().unwrap_or_else(|| PathBuf::from("."));
    path.push("oplire");
    path.push("config.json");
    path
}

fn dirs_config_dir() -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        if let Ok(home) = std::env::var("HOME") {
            let mut p = PathBuf::from(home);
            p.push("Library");
            p.push("Application Support");
            return Some(p);
        }
    }
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(xdg));
    }
    if let Ok(home) = std::env::var("HOME") {
        let mut p = PathBuf::from(home);
        p.push(".config");
        return Some(p);
    }
    None
}

fn load_config() -> AppConfig {
    let path = config_path();
    if path.exists() {
        if let Ok(content) = fs::read_to_string(&path) {
            if let Ok(config) = serde_json::from_str(&content) {
                return config;
            }
        }
    }
    AppConfig::default()
}

fn save_config(config: &AppConfig) -> Result<(), String> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let content = serde_json::to_string_pretty(config).map_err(|e| e.to_string())?;
    fs::write(&path, content).map_err(|e| e.to_string())?;
    Ok(())
}

fn check_opencode_installed() -> bool {
    Command::new("opencode").arg("--version").output().is_ok()
        || Command::new("opencode").arg("--help").output().is_ok()
}

fn check_opencode_running(base_url: &str) -> bool {
    let url = format!("{}/v1/models", base_url.trim_end_matches('/'));
    reqwest::blocking::Client::new()
        .get(&url)
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

fn check_node_installed() -> bool {
    Command::new("node").arg("--version").output().is_ok()
}

fn run_interactive(cmd: &str, args: &[&str]) -> Result<(), String> {
    let status = Command::new(cmd)
        .args(args)
        .status()
        .map_err(|e| e.to_string())?;

    if status.success() {
        Ok(())
    } else {
        Err(format!("{} exited with {}", cmd, status))
    }
}

fn print_banner() {
    println!(
        "{}",
        r#"
  ___      ___    _       ___     ___     ___
 / _ \    | _ \  | |     |_ _|   | _ \   | __|
| (_) |   |  _/  | |__    | |    |   /   | _|
 \___/   _|_|_   |____|  |___|   |_|_\   |___|
_|"""""|_| """ |_|"""""|_|"""""|_|"""""|_|"""""|
"`-0-0-'"`-0-0-'"`-0-0-'"`-0-0-'"`-0-0-'"`-0-0-'
"#
        .bold()
        .cyan()
    );
}

fn print_step(num: usize, total: usize, text: &str) {
    println!(
        "  {} {}",
        format!("[{}/{}]", num, total).dimmed(),
        text.bold()
    );
}

fn print_success(text: &str) {
    println!("  {} {}", "✓".green().bold(), text);
}

fn print_fail(text: &str) {
    println!("  {} {}", "✗".red().bold(), text);
}

fn print_info_formatted(label: &str, desc: &str) {
    println!("  {} {} — {}", "→".cyan(), label.bold(), desc.dimmed());
}

fn main() {
    let cli = Cli::parse();

    let filter = if cli.verbose { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(filter)),
        )
        .with_target(false)
        .init();

    if cli.verbose {
        eprintln!("{} Verbose mode enabled", "[DEBUG]".yellow());
    }

    match &cli.command {
        Commands::Models { upstream } => {
            print_banner();
            println!("{}", "Available Models".bold().cyan());
            println!();
            println!("{} Fetching models from {}...", "→".cyan(), upstream.bold());
            println!();

            let models_url = format!("{}/v1/models", upstream.trim_end_matches('/'));

            match reqwest::blocking::Client::new()
                .get(&models_url)
                .timeout(std::time::Duration::from_secs(10))
                .send()
            {
                Ok(resp) if resp.status().is_success() => match resp.json::<serde_json::Value>() {
                    Ok(data) => {
                        if let Some(models) = data.get("data").and_then(|v| v.as_array()) {
                            if models.is_empty() {
                                println!("{} No models found", "[INFO]".yellow());
                                return;
                            }

                            println!("  {}  {:<30}  {}", "#".dimmed(), "ID".bold(), "Name".bold());
                            println!("  {}", "─".repeat(60).dimmed());

                            for (i, m) in models.iter().enumerate() {
                                let id = m.get("id").and_then(|v| v.as_str()).unwrap_or("?");
                                let name: String = match m
                                    .get("name")
                                    .or_else(|| m.get("display_name"))
                                    .and_then(|v| v.as_str())
                                {
                                    Some(n) => n.to_string(),
                                    None => id
                                        .replace('-', " ")
                                        .split_whitespace()
                                        .map(|w| {
                                            let mut c = w.chars();
                                            match c.next() {
                                                None => String::new(),
                                                Some(ch) => format!(
                                                    "{}{}",
                                                    ch.to_uppercase().collect::<String>(),
                                                    c.as_str()
                                                ),
                                            }
                                        })
                                        .collect::<Vec<_>>()
                                        .join(" "),
                                };

                                println!(
                                    "  {}  {:<30}  {}",
                                    format!("[{}]", i + 1).dimmed(),
                                    id.yellow(),
                                    name
                                );
                            }

                            println!();
                            println!("{}", "Usage:".bold());
                            println!(
                                "  {}",
                                "opencode run -m <id> \"Hello from OpenCode!\""
                                    .bold()
                                    .yellow()
                            );
                            println!();
                            println!("{}", "Examples:".bold());
                            for m in models.iter().take(3) {
                                let id = m.get("id").and_then(|v| v.as_str()).unwrap_or("");
                                if !id.is_empty() {
                                    println!(
                                        "  {}",
                                        format!("opencode run -m {} \"Hello\"", id).dimmed()
                                    );
                                }
                            }
                        } else {
                            println!("{} No 'data' field in response", "[ERROR]".red());
                        }
                    }
                    Err(e) => {
                        println!("{} Failed to parse response: {}", "[ERROR]".red(), e);
                    }
                },
                Ok(resp) => {
                    println!(
                        "{} Upstream returned status: {}",
                        "[ERROR]".red(),
                        resp.status()
                    );
                    println!(
                        "{} Is OpenCode Zen running on {}?",
                        "Tip:".cyan(),
                        upstream.bold()
                    );
                }
                Err(e) => {
                    println!(
                        "{} Failed to connect to {}",
                        "[ERROR]".red(),
                        upstream.bold()
                    );
                    println!("{} {}", "Error:".dimmed(), e.to_string().dimmed());
                    println!("{} Is OpenCode Zen running?", "Tip:".cyan());
                }
            }
        }

        Commands::Setup {} => {
            print_banner();
            println!("{}", "Welcome to oplire Setup Wizard".bold().green());
            println!();
            println!(
                "{}",
                "This will check and install all required components.".dimmed()
            );
            println!();

            let mut steps_needed = Vec::new();

            if !check_node_installed() {
                steps_needed.push((
                    "Node.js",
                    "npm install -g npm",
                    "Required to install OpenCode",
                ));
            }
            if !check_opencode_installed() {
                steps_needed.push(("OpenCode", "oplire install opencode", "AI coding assistant"));
            }

            if steps_needed.is_empty() {
                println!("{}", "Everything is already installed!".green().bold());
                println!();
                println!("{} Run to get started:", "Tip:".cyan().bold());
                println!("  {}", "oplire proxies import <file>".bold().yellow());
                println!("  {}", "oplire proxy".bold().yellow());
                return;
            }

            println!(
                "{}",
                "The following components need to be installed:".bold()
            );
            println!();
            for (name, _, desc) in &steps_needed {
                print_info_formatted(name, desc);
            }
            println!();

            for (i, (name, _cmd, _)) in steps_needed.iter().enumerate() {
                println!();
                print_step(
                    i + 1,
                    steps_needed.len(),
                    &format!("Installing {}...", name),
                );

                match *name {
                    "Node.js" => {
                        println!("  {} Node.js must be installed manually", "ℹ".yellow());
                        println!("  {}", "curl -o- https://raw.githubusercontent.com/nvm-sh/nvm/v0.40.1/install.sh | bash".dimmed());
                        println!("  {}", "Then restart your terminal".dimmed());
                    }
                    "OpenCode" => {
                        let _ = run_interactive("oplire", &["install", "opencode"]);
                    }
                    _ => {}
                }
            }

            println!();
            println!("{}", "Setup complete!".green().bold());
            println!();
            println!("{}", "Next steps:".bold());
            println!("  1. {}", "oplire proxies import <file>".bold().yellow());
            println!("  2. {}", "oplire proxy".bold().yellow());
            println!("  3. {}", "eval $(oplire env) && opencode".bold().yellow());
        }

        Commands::Install { target } => match target {
            InstallTarget::Opencode {} => {
                print_banner();
                println!("{}", "Installing OpenCode".bold().green());
                println!();

                if check_opencode_installed() {
                    println!("{} OpenCode is already installed", "[INFO]".green());
                    return;
                }

                if !check_node_installed() {
                    println!("{} Node.js is required but not found", "[ERROR]".red());
                    println!(
                        "{} Install Node.js first: https://nodejs.org/",
                        "Fix:".cyan()
                    );
                    std::process::exit(1);
                }

                print_step(1, 1, "Installing OpenCode via npm...");
                match run_interactive("npm", &["install", "-g", "opencode-ai"]) {
                    Ok(()) => {
                        print_success("OpenCode installed");
                        println!();
                        println!("{}", "Next steps:".bold());
                        println!("  1. {}", "opencode".bold().yellow());
                        println!("  2. {}", "oplire doctor".bold().yellow());
                        println!("  3. {}", "oplire proxy".bold().yellow());
                    }
                    Err(e) => {
                        print_fail(&format!("Installation failed: {}", e));
                        println!();
                        println!("{} Try manually:", "Fix:".cyan());
                        println!("  {}", "npm install -g opencode-ai".bold().yellow());
                    }
                }
            }
        },

        Commands::Watch {
            upstream,
            log_file,
            proxy_file,
            delay_ms,
        } => {
            let app_cfg = load_config();
            let effective_upstream = upstream.clone().unwrap_or(app_cfg.upstream);

            print_banner();
            println!("{}", "OpenCode Watch Mode (Proxy Rotation)".bold().yellow());
            println!();
            if let Some(ref lf) = log_file {
                println!(
                    "{} Log monitor: {}",
                    "→".green(),
                    lf.display().to_string().bold()
                );
            } else {
                println!(
                    "{} Log monitor: {}",
                    "→".green(),
                    "auto-detecting OpenCode log".green().bold()
                );
            }

            let pool = if let Some(ref pf) = proxy_file {
                match ProxyPool::load_from_file(pf, "custom") {
                    Ok(p) => Some(p),
                    Err(e) => {
                        eprintln!(
                            "{} Failed to load proxy file {}: {}",
                            "[WARN]".yellow(),
                            pf.display(),
                            e
                        );
                        None
                    }
                }
            } else {
                let p = ProxyPool::load_default();
                if p.is_empty_sync() {
                    None
                } else {
                    Some(p)
                }
            };

            if let Some(ref p) = pool {
                let rt_test = tokio::runtime::Runtime::new().unwrap();
                if let Some(active) = rt_test.block_on(p.get_active()) {
                    println!(
                        "{} Proxy Pool:  {} ({}:{})",
                        "→".green(),
                        active.provider.bold(),
                        active.host,
                        active.port
                    );
                }
            } else {
                println!(
                    "{} Upstream fallback: {}",
                    "→".green(),
                    effective_upstream.dimmed()
                );
            }
            println!();
            println!(
                "{}",
                "Watching for OpenCode rate limits in real time...".dimmed()
            );
            println!("{} Press Ctrl+C to stop", "Tip:".cyan());
            println!();

            let upstream_clone = effective_upstream.clone();
            let log_file_clone = log_file.clone();
            let pool_clone = pool.clone();
            let delay = *delay_ms;

            let rt = tokio::runtime::Runtime::new().unwrap();
            if let Err(e) = rt.block_on(async {
                oplire_reset::watch::start_watch_mode(
                    &upstream_clone,
                    log_file_clone,
                    pool_clone,
                    delay,
                )
                .await
            }) {
                eprintln!("{} Watch error: {}", "[ERROR]".red(), e);
                std::process::exit(1);
            }
        }

        Commands::Daemon { listen, proxy_file } => {
            let app_cfg = load_config();
            let effective_listen = listen.clone().unwrap_or(app_cfg.listen);
            let effective_upstream = app_cfg.upstream.clone();

            print_banner();
            println!("{}", "Daemon Mode".bold().magenta());
            println!();
            println!("{} Listening:  {}", "→".green(), effective_listen.bold());
            println!("{} Upstream:   {}", "→".green(), effective_upstream.bold());

            let pool = if let Some(ref pf) = proxy_file {
                match ProxyPool::load_from_file(pf, "custom") {
                    Ok(p) => Some(p),
                    Err(e) => {
                        eprintln!(
                            "{} Failed to load proxy file {}: {}",
                            "[WARN]".yellow(),
                            pf.display(),
                            e
                        );
                        None
                    }
                }
            } else {
                let p = ProxyPool::load_default();
                if p.is_empty_sync() {
                    None
                } else {
                    Some(p)
                }
            };

            if let Some(ref p) = pool {
                let rt_test = tokio::runtime::Runtime::new().unwrap();
                if let Some(active) = rt_test.block_on(p.get_active()) {
                    println!(
                        "{} Proxy Pool:  {} ({}:{})",
                        "→".green(),
                        active.provider.bold(),
                        active.host,
                        active.port
                    );
                }
            }
            println!();
            println!("{}", "Running as background daemon...".dimmed());
            println!(
                "{} Log watch will rotate proxies automatically",
                "Tip:".cyan()
            );
            println!();

            let rt = tokio::runtime::Runtime::new().unwrap();
            let pool_watcher = pool.clone();
            let upstream_watcher = effective_upstream.clone();

            if let Err(e) = rt.block_on(async {
                tokio::spawn(async move {
                    let _ = oplire_reset::watch::start_watch_mode(
                        &upstream_watcher,
                        None,
                        pool_watcher,
                        oplire_reset::watch::DEFAULT_DEBOUNCE_MS,
                    )
                    .await;
                });

                oplire_reset::proxy::start_proxy_server(effective_listen, pool).await
            }) {
                eprintln!("{} Daemon error: {}", "[ERROR]".red(), e);
                std::process::exit(1);
            }
        }

        Commands::Doctor {} => {
            print_banner();
            println!("{}", "System Diagnostics".bold().cyan());
            println!();

            let mut all_ok = true;
            let mut checks = Vec::new();

            if check_opencode_installed() {
                checks.push(("OpenCode", "installed".to_string(), true));
            } else {
                checks.push(("OpenCode", "NOT FOUND".to_string(), false));
                all_ok = false;
            }

            let config = load_config();
            let upstream = config.upstream.clone();
            if check_opencode_running(&upstream) {
                checks.push(("OpenCode Zen", format!("running ({})", upstream), true));
            } else {
                checks.push((
                    "OpenCode Zen",
                    format!("NOT REACHABLE ({})", upstream),
                    false,
                ));
                all_ok = false;
            }

            let cfg_path = config_path();
            if cfg_path.exists() {
                checks.push((
                    "Config file",
                    format!("found ({})", cfg_path.display()),
                    true,
                ));
            } else {
                checks.push(("Config file", "using defaults".to_string(), true));
            }

            if std::net::TcpListener::bind(config.listen.as_str()).is_ok() {
                checks.push((
                    "Listen address",
                    format!("available ({})", config.listen),
                    true,
                ));
            } else {
                checks.push((
                    "Listen address",
                    format!("IN USE ({})", config.listen),
                    false,
                ));
                all_ok = false;
            }

            for (name, status, ok) in &checks {
                let status_str = if *ok {
                    status.green().bold()
                } else {
                    status.red().bold()
                };
                println!("{} {}: {}", "→".cyan(), name.bold(), status_str);
            }

            println!();
            if all_ok {
                println!("{}", "All checks passed!".green().bold());
            } else {
                println!("{}", "Some checks failed.".yellow().bold());
                println!();
                println!(
                    "{} Run `oplire setup` for guided installation",
                    "Fix:".cyan()
                );
            }
        }

        Commands::Config { action } => match action {
            ConfigAction::Show {} => {
                let config = load_config();
                println!("{}", "Current Configuration:".bold());
                println!();
                println!("{} {}", "Listen:".bold(), config.listen);
                println!("{} {}", "Upstream:".bold(), config.upstream);
                println!("{} {}", "Config file:".bold(), config_path().display());
            }
            ConfigAction::Set { listen, upstream } => {
                let mut config = load_config();

                if let Some(l) = listen {
                    config.listen = l.clone();
                }
                if let Some(u) = upstream {
                    config.upstream = u.clone();
                }

                match save_config(&config) {
                    Ok(()) => {
                        println!("{}", "Configuration saved!".green().bold());
                        println!("{} {}", "File:".bold(), config_path().display());
                    }
                    Err(e) => {
                        eprintln!("{} Failed to save config: {}", "[ERROR]".red(), e);
                        std::process::exit(1);
                    }
                }
            }
            ConfigAction::Reset {} => {
                let path = config_path();
                if path.exists() {
                    fs::remove_file(&path)
                        .map_err(|e| e.to_string())
                        .unwrap_or_else(|e| {
                            eprintln!("{} Failed to remove config: {}", "[ERROR]".red(), e);
                            std::process::exit(1);
                        });
                    println!("{}", "Configuration reset to defaults!".green().bold());
                } else {
                    println!("{} No configuration file found", "[INFO]".green());
                }
            }
        },

        Commands::Proxy { listen, proxy_file } => {
            let app_cfg = load_config();
            let effective_listen = listen.clone().unwrap_or(app_cfg.listen);
            let effective_upstream = app_cfg.upstream.clone();

            let pool = if let Some(ref pf) = proxy_file {
                match ProxyPool::load_from_file(pf, "custom") {
                    Ok(p) => Some(p),
                    Err(e) => {
                        eprintln!(
                            "{} Failed to load proxy file {}: {}",
                            "[WARN]".yellow(),
                            pf.display(),
                            e
                        );
                        None
                    }
                }
            } else {
                let p = ProxyPool::load_default();
                if p.is_empty_sync() {
                    None
                } else {
                    Some(p)
                }
            };

            print_banner();
            println!("{}", "OpenCode Forward Proxy".bold().cyan());
            println!();
            println!("{} Listening on: {}", "→".green(), effective_listen.bold());
            if let Some(ref p) = pool {
                let rt_test = tokio::runtime::Runtime::new().unwrap();
                if let Some(active) = rt_test.block_on(p.get_active()) {
                    println!(
                        "{} Proxy Pool:  {} ({}:{})",
                        "→".green(),
                        active.provider.bold(),
                        active.host,
                        active.port
                    );
                }
            }
            println!();
            println!(
                "{} Configure OpenCode CLI / Desktop: {}",
                "Tip:".cyan().bold(),
                "eval $(oplire env)".bold()
            );
            println!();

            let rt = tokio::runtime::Runtime::new().unwrap();
            let pool_watcher = pool.clone();
            let upstream_watcher = effective_upstream.clone();

            if let Err(e) = rt.block_on(async {
                tokio::spawn(async move {
                    let _ = oplire_reset::watch::start_watch_mode(
                        &upstream_watcher,
                        None,
                        pool_watcher,
                        oplire_reset::watch::DEFAULT_DEBOUNCE_MS,
                    )
                    .await;
                });

                oplire_reset::proxy::start_proxy_server(effective_listen, pool).await
            }) {
                eprintln!("{} Proxy server error: {}", "[ERROR]".red(), e);
                std::process::exit(1);
            }
        }

        Commands::Proxies { action } => {
            print_banner();
            match action {
                ProxiesAction::List {} => {
                    println!("{}", "Residential & ISP Proxy Pool".bold().cyan());
                    println!();
                    let pool = ProxyPool::load_default();
                    let rt = tokio::runtime::Runtime::new().unwrap();
                    rt.block_on(async {
                        let proxies = pool.proxies.read().await;
                        if proxies.is_empty() {
                            println!("{} No proxies configured in pool.", "[WARN]".yellow());
                            println!("{} Import proxies with `oplire proxies import <file>` or place `proxiware-isp.txt` in current dir", "Tip:".cyan());
                            return;
                        }
                        let active_idx = pool.active_index.load(std::sync::atomic::Ordering::SeqCst) % proxies.len();
                        println!("{} Config path:   {}", "→".green(), ProxyPool::config_proxies_path().display().to_string().bold());
                        println!("{} Total proxies: {}", "→".green(), proxies.len().to_string().bold());
                        println!();

                        println!("  {:<4} {:<8} {:<24} {:<15} Auth", "Act", "Index", "Host:Port", "Provider");
                        println!("  {}", "─".repeat(65).dimmed());

                        for (i, p) in proxies.iter().enumerate() {
                            let is_active = i == active_idx;
                            let marker = if is_active { "★".green().bold() } else { " ".dimmed() };
                            let idx_str = format!("[{}/{}]", i + 1, proxies.len());
                            let hp = format!("{}:{}", p.host, p.port);
                            let auth = if p.username.is_some() { "yes".green() } else { "none".dimmed() };
                            if is_active {
                                println!("  {:<4} {:<8} {:<24} {:<15} {}", marker, idx_str.green().bold(), hp.green().bold(), p.provider.green(), auth);
                            } else {
                                println!("  {:<4} {:<8} {:<24} {:<15} {}", marker, idx_str.dimmed(), hp, p.provider, auth);
                            }
                        }
                        println!();
                        println!("{} Rotate to next proxy with: {}", "Tip:".cyan(), "oplire proxies next".bold());
                        println!("{} Test all proxies with:      {}", "Tip:".cyan(), "oplire proxies test".bold());
                    });
                }
                ProxiesAction::Test {} => {
                    println!("{}", "Testing Proxy Pool Connectivity".bold().cyan());
                    println!();
                    let pool = ProxyPool::load_default();
                    let rt = tokio::runtime::Runtime::new().unwrap();
                    rt.block_on(async {
                        let proxies = pool.proxies.read().await;
                        if proxies.is_empty() {
                            println!("{} No proxies to test in pool.", "[WARN]".yellow());
                            return;
                        }
                        for (i, p) in proxies.iter().enumerate() {
                            print!(
                                "  [{}/{}] {}:{} ({})... ",
                                i + 1,
                                proxies.len(),
                                p.host,
                                p.port,
                                p.provider
                            );
                            match pool.test_proxy(p).await {
                                Ok((ip, elapsed)) => {
                                    println!(
                                        "{} Exit IP: {} ({}ms)",
                                        "✓ OK".green().bold(),
                                        ip.bold(),
                                        elapsed.as_millis()
                                    );
                                }
                                Err(e) => {
                                    println!("{} Error: {}", "✗ FAIL".red().bold(), e);
                                }
                            }
                        }
                        println!();
                    });
                }
                ProxiesAction::Next {} => {
                    println!("{}", "Rotating Active Proxy".bold().cyan());
                    println!();
                    let pool = ProxyPool::load_default();
                    let rt = tokio::runtime::Runtime::new().unwrap();
                    rt.block_on(async {
                        if let Some(p) = pool.rotate().await {
                            let total = pool.proxies.read().await.len();
                            let idx =
                                pool.active_index.load(std::sync::atomic::Ordering::SeqCst) % total;
                            println!(
                                "{} Switched to proxy [{}/{}]: {} ({}:{})",
                                "✓".green().bold(),
                                idx + 1,
                                total,
                                p.provider.bold(),
                                p.host,
                                p.port
                            );
                        } else {
                            println!("{} No proxies in pool to rotate", "[WARN]".yellow());
                        }
                    });
                }
                ProxiesAction::Import { file } => {
                    println!("{}", "Importing Proxies".bold().cyan());
                    println!();
                    if !file.exists() {
                        eprintln!("{} File not found: {}", "[ERROR]".red(), file.display());
                        std::process::exit(1);
                    }
                    match ProxyPool::load_from_file(file, "custom") {
                        Ok(pool) => {
                            if let Err(e) = pool.save_to_config() {
                                eprintln!("{} Failed to save proxies: {}", "[ERROR]".red(), e);
                                std::process::exit(1);
                            }
                            let count = futures::executor::block_on(pool.proxies.read()).len();
                            println!(
                                "{} Successfully imported {} proxies to {}",
                                "✓".green().bold(),
                                count.to_string().bold(),
                                ProxyPool::config_proxies_path()
                                    .display()
                                    .to_string()
                                    .bold()
                            );
                        }
                        Err(e) => {
                            eprintln!("{} Failed to read proxy file: {}", "[ERROR]".red(), e);
                            std::process::exit(1);
                        }
                    }
                }
                ProxiesAction::Fetch { api_key } => {
                    println!(
                        "{}",
                        "Fetching Static ISP Proxies from Proxiware".bold().cyan()
                    );
                    println!();
                    let rt = tokio::runtime::Runtime::new().unwrap();
                    match rt.block_on(ProxyPool::fetch_from_proxiware_api(api_key)) {
                        Ok(proxies) => {
                            let pool = ProxyPool::new(proxies);
                            if let Err(e) = pool.save_to_config() {
                                eprintln!("{} Failed to save proxies: {}", "[ERROR]".red(), e);
                                std::process::exit(1);
                            }
                            let count = futures::executor::block_on(pool.proxies.read()).len();
                            println!(
                                "{} Successfully fetched {} ISP proxies and saved to {}",
                                "✓".green().bold(),
                                count.to_string().bold(),
                                ProxyPool::config_proxies_path()
                                    .display()
                                    .to_string()
                                    .bold()
                            );
                        }
                        Err(e) => {
                            eprintln!("{} Failed to fetch proxies: {}", "[ERROR]".red(), e);
                            std::process::exit(1);
                        }
                    }
                }
            }
        }

        Commands::Env { listen } => {
            let proxy_url = format!("http://{}", listen);
            println!("export HTTPS_PROXY=\"{}\"", proxy_url);
            println!("export HTTP_PROXY=\"{}\"", proxy_url);
            println!("export ALL_PROXY=\"{}\"", proxy_url);
            println!("export NO_PROXY=\"localhost,127.0.0.1,::1\"");
            println!();
            println!("# Apply in your current terminal session with:");
            println!("#   eval $(oplire env)");
            println!("#");
            println!("# Then run OpenCode CLI or launch OpenCode Desktop:");
            println!("#   opencode");
        }

        Commands::About {} => {
            print_banner();
            println!();
            println!("{} {}", "Version:".bold(), VERSION);
            println!("{} Rust", "Language:".bold());
            println!("{} OpenCode proxy pool manager", "Purpose:".bold());
            println!("{} Tokio + HTTP CONNECT tunnel", "Infrastructure:".bold());
            println!("{} Berke Oruc", "Author:".bold());
            println!("{} https://github.com/BerkeOruc/oplire", "GitHub:".bold());
            println!();
            println!("{}", "Installation:".bold());
            println!("  oplire install opencode      # Install OpenCode");
            println!("  oplire setup                 # Guided setup wizard");
            println!();
            println!("{}", "Proxy & Pool Management:".bold());
            println!("  oplire proxy               # Start forward proxy");
            println!("  oplire proxies list        # Show active & pool proxies");
            println!("  oplire proxies test        # Test ping & exit IP of all proxies");
            println!("  oplire proxies next        # Rotate to next pool proxy");
            println!("  oplire proxies import <f>  # Import proxy list file");
            println!("  oplire proxies fetch       # Fetch ISP proxies from Proxiware");
            println!("  oplire env                 # Print proxy export commands for shell");
            println!("  oplire daemon              # Background proxy service");
            println!("  oplire watch               # Monitor OpenCode, rotate on limits");
            println!();
            println!("{}", "Configuration:".bold());
            println!("  oplire models              # List available OpenCode models");
            println!("  oplire config show         # Show current config");
            println!("  oplire config set          # Save config");
            println!("  oplire config reset        # Reset to defaults");
            println!("  oplire doctor              # Diagnose system setup");
        }
    }

    if !matches!(
        &cli.command,
        Commands::About {}
            | Commands::Proxy { .. }
            | Commands::Daemon { .. }
            | Commands::Watch { .. }
            | Commands::Doctor {}
            | Commands::Config { .. }
            | Commands::Setup {}
            | Commands::Install { .. }
            | Commands::Models { .. }
            | Commands::Proxies { .. }
            | Commands::Env { .. }
    ) {
        println!("\n{} v{}", "oplire".bold(), VERSION.dimmed());
    }
}
