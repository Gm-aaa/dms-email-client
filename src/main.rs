mod config;
mod daemon;

use clap::{Parser, Subcommand};
use config::Config;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

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
            stream_command("watch");
        }
        Some(Commands::Status) | None => {
            // Default to query status
            send_command("status");
        }
        Some(Commands::Body { account, folder, uid }) => {
            send_command(&format!("body\t{}\t{}\t{}", account, folder, uid));
        }
        Some(Commands::Read { account, folder, uid }) => {
            send_command(&format!("read\t{}\t{}\t{}", account, folder, uid));
        }
        Some(Commands::ReadAll { account }) => {
            send_command(&format!("read_all\t{}", account));
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

/// 连接守护进程 socket，发送一行命令并打印其响应
fn send_command(cmd: &str) {
    match UnixStream::connect(daemon::socket_path()) {
        Ok(mut stream) => {
            let _ = stream.write_all(cmd.as_bytes());
            let _ = stream.write_all(b"\n");
            let _ = stream.flush();
            let mut response = String::new();
            if let Err(e) = stream.read_to_string(&mut response) {
                print_error_json(&format!("Read error: {}", e));
                return;
            }
            // Directly output the JSON received from daemon
            println!("{}", response);
        }
        Err(_) => {
            print_error_json("Daemon not running");
        }
    }
}

/// 连接守护进程 socket，发送订阅命令，然后把服务端持续推送的每一行**流式**转发到
/// stdout（逐块读取并 flush，不等 EOF）。守护进程关闭连接或出错时返回，进程随之退出，
/// 前端据此重连。前端用 SplitParser 逐行消费，实现零延迟的状态更新。
fn stream_command(cmd: &str) {
    match UnixStream::connect(daemon::socket_path()) {
        Ok(mut stream) => {
            let _ = stream.write_all(cmd.as_bytes());
            let _ = stream.write_all(b"\n");
            let _ = stream.flush();
            let mut buf = [0u8; 4096];
            let stdout = std::io::stdout();
            loop {
                match stream.read(&mut buf) {
                    Ok(0) => break, // 守护进程关闭了连接
                    Ok(n) => {
                        let mut lock = stdout.lock();
                        if lock.write_all(&buf[..n]).is_err() {
                            break;
                        }
                        let _ = lock.flush();
                    }
                    Err(_) => break,
                }
            }
        }
        Err(_) => {
            print_error_json("Daemon not running");
        }
    }
}

fn print_error_json(err_msg: &str) {
    let err_json = serde_json::json!({
        "error": err_msg,
        "unread_mails": [],
        "last_update": ""
    });
    println!("{}", err_json.to_string());
}
