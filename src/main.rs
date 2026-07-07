mod config;
mod daemon;
mod ipc;
mod mailhtml;
mod segment;
mod sysmem;
mod translate;

use clap::{Parser, Subcommand};
use config::Config;
use ipc::Request;
use std::io::Read;

#[derive(Parser)]
#[command(name = "dms-email-client")]
#[command(about = "A high-performance email checker daemon and client for DankMaterialShell (DMS)", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the background mail checker daemon
    Daemon,
    /// Query the current state from the daemon (one-shot)
    Status,
    /// Subscribe to daemon state changes; streams one JSON line per update.
    /// Runs until the daemon closes the connection. Used by the frontend instead
    /// of polling, so new mail / read changes reach the UI with zero delay.
    Watch,
    /// Fetch the body of a specific email via the daemon
    Body {
        account: String,
        folder: String,
        uid: String,
    },
    /// Mark a specific email as read via the daemon
    Read {
        account: String,
        folder: String,
        uid: String,
    },
    /// Mark all unread emails read (optionally restricted to one account)
    ReadAll {
        /// Account name to restrict to; empty = all accounts
        #[arg(default_value = "")]
        account: String,
    },
    /// Translate an email body via the daemon
    Translate {
        account: String,
        folder: String,
        uid: String,
        /// NLLB source lang code, or "auto"
        src: String,
        /// NLLB target lang code (e.g. zho_Hans)
        tgt: String,
        /// Engine: nllb (offline) | google | deeplx
        #[arg(default_value = "nllb")]
        engine: String,
        /// DeepLX endpoint URL (only for engine=deeplx)
        #[arg(default_value = "")]
        deeplx_url: String,
    },
    /// Manage configuration
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Show current configuration as JSON
    Show,
    /// Save configuration from JSON (reads from stdin)
    Save,
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Daemon) => {
            println!("Starting DMS Email Client Daemon...");
            // Load configuration
            let config = match Config::load() {
                Ok(cfg) => cfg,
                Err(e) => {
                    eprintln!("Error loading configuration: {:?}", e);
                    std::process::exit(1);
                }
            };

            if let Err(e) = daemon::run_daemon(config) {
                eprintln!("Daemon error: {:?}", e);
                std::process::exit(1);
            }
        }
        Some(Commands::Watch) => {
            ipc::stream(&Request::Watch);
        }
        Some(Commands::Status) | None => {
            // Default to query status
            ipc::send(&Request::Status);
        }
        Some(Commands::Body { account, folder, uid }) => {
            ipc::send(&Request::Body { account, folder, uid });
        }
        Some(Commands::Read { account, folder, uid }) => {
            ipc::send(&Request::Read { account, folder, uid });
        }
        Some(Commands::ReadAll { account }) => {
            ipc::send(&Request::ReadAll { account });
        }
        Some(Commands::Translate { account, folder, uid, src, tgt, engine, deeplx_url }) => {
            ipc::send(&Request::Translate { account, folder, uid, src, tgt, engine, deeplx_url });
        }
        Some(Commands::Config { action }) => {
            match action {
                ConfigAction::Show => {
                    let config = match Config::load() {
                        Ok(cfg) => cfg,
                        Err(e) => {
                            eprintln!("Error loading configuration: {:?}", e);
                            std::process::exit(1);
                        }
                    };
                    match config.to_json() {
                        Ok(json) => println!("{}", json),
                        Err(e) => {
                            eprintln!("Error serializing configuration: {:?}", e);
                            std::process::exit(1);
                        }
                    }
                }
                ConfigAction::Save => {
                    let mut input = String::new();
                    if let Err(e) = std::io::stdin().read_to_string(&mut input) {
                        eprintln!("Error reading from stdin: {:?}", e);
                        std::process::exit(1);
                    }
                    let config = match Config::from_json(&input) {
                        Ok(cfg) => cfg,
                        Err(e) => {
                            eprintln!("Error parsing JSON: {:?}", e);
                            std::process::exit(1);
                        }
                    };
                    if let Err(e) = config.save() {
                        eprintln!("Error saving configuration: {:?}", e);
                        std::process::exit(1);
                    }
                    println!("{{\"success\": true}}");
                }
            }
        }
    }
}
