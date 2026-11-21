mod network;
mod ui;

use std::{
    error::Error,
    fs::{self, OpenOptions},
    io::{self, ErrorKind},
    path::{Path, PathBuf},
    time::Duration,
};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use libp2p::identity::Keypair;
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

const DATA_DIR: &str = "data";
const IDENTITY_FILE: &str = "identity.key";
const LOG_FILE: &str = "chat.log";

/// Load the ed25519 key from `path`, creating it on first run.
/// The file is authoritative for the PeerId; the database row comes in phase 3.
fn load_or_create_identity(path: &Path) -> io::Result<Keypair> {
    match fs::read(path) {
        Ok(bytes) => Keypair::from_protobuf_encoding(&bytes)
            .map_err(|error| io::Error::new(ErrorKind::InvalidData, error.to_string())),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            let keypair = Keypair::generate_ed25519();
            let bytes = keypair
                .to_protobuf_encoding()
                .map_err(|error| io::Error::new(ErrorKind::InvalidData, error.to_string()))?;
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(path, bytes)?;
            #[cfg(unix)]
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
            Ok(keypair)
        }
        Err(error) => Err(error),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Cli {
    Run { data_dir: PathBuf, simple: bool },
    Help,
    Error(String),
}

fn parse_args_from<I>(args: I) -> Cli
where
    I: IntoIterator<Item = String>,
{
    let mut args = args.into_iter().skip(1);
    let mut data_dir = PathBuf::from(DATA_DIR);
    let mut simple = false;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--data-dir" => match args.next() {
                Some(path) => data_dir = PathBuf::from(path),
                None => return Cli::Error("--data-dir requires a path".to_string()),
            },
            "--simple" => simple = true,
            "-h" | "--help" => return Cli::Help,
            other => return Cli::Error(format!("unknown argument: {other}")),
        }
    }
    Cli::Run { data_dir, simple }
}

fn print_usage() {
    println!("Usage: lan-chat [--data-dir DIR] [--simple]");
    println!();
    println!("  --data-dir DIR  where identity.key (and later chat.db) live; default: data");
    println!("  --simple        plain line-by-line CLI instead of the full-screen TUI");
    println!("  -h, --help      show this help");
    println!();
    println!("Run two instances on one laptop with separate data dirs, e.g.");
    println!("  cargo run -- --data-dir data/alice");
    println!("  cargo run -- --data-dir data/bob");
}

fn init_logging(path: &Path) -> io::Result<()> {
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    tracing_subscriber::fmt()
        .with_writer(file)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_ansi(false)
        .try_init()
        .map_err(|error| io::Error::other(error.to_string()))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let (data_dir, simple) = match parse_args_from(std::env::args()) {
        Cli::Run { data_dir, simple } => (data_dir, simple),
        Cli::Help => {
            print_usage();
            return Ok(());
        }
        Cli::Error(message) => {
            eprintln!("{message}\n");
            print_usage();
            std::process::exit(2);
        }
    };
    fs::create_dir_all(&data_dir)?;
    init_logging(&data_dir.join(LOG_FILE))?;

    let username = ui::cli::prompt_username()?;
    let identity_path = data_dir.join(IDENTITY_FILE);
    let keypair = load_or_create_identity(&identity_path)?;

    let (command_tx, command_rx) = mpsc::channel(64);
    let (event_tx, event_rx) = mpsc::channel(128);
    let network = network::spawn(keypair, username.clone(), command_rx, event_tx)?;

    tracing::info!(
        peer_id = %network.peer_id,
        identity = %identity_path.display(),
        "identity loaded"
    );
    if simple {
        println!(
            "Identity: {} (saved in {})",
            network.peer_id,
            identity_path.display()
        );
        ui::cli::run(network.peer_id, username, command_tx, event_rx).await?;
    } else {
        ui::run(network.peer_id, username, command_tx, event_rx).await?;
    }

    if tokio::time::timeout(Duration::from_secs(2), network.task)
        .await
        .is_err()
    {
        eprintln!("warning: network task did not stop within 2 seconds");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_file_is_created_then_reloaded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join(IDENTITY_FILE);

        let first = load_or_create_identity(&path).unwrap();
        assert!(path.exists());
        let second = load_or_create_identity(&path).unwrap();
        assert_eq!(first.public().to_peer_id(), second.public().to_peer_id());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn parses_data_dir_argument() {
        assert_eq!(
            parse_args_from(["lan-chat".to_string()]),
            Cli::Run {
                data_dir: PathBuf::from(DATA_DIR),
                simple: false,
            }
        );
        assert_eq!(
            parse_args_from([
                "lan-chat".to_string(),
                "--data-dir".to_string(),
                "data/alice".to_string(),
                "--simple".to_string(),
            ]),
            Cli::Run {
                data_dir: PathBuf::from("data/alice"),
                simple: true,
            }
        );
        assert_eq!(
            parse_args_from(["lan-chat".to_string(), "--help".to_string()]),
            Cli::Help
        );
        assert!(matches!(
            parse_args_from(["lan-chat".to_string(), "--data-dir".to_string()]),
            Cli::Error(_)
        ));
    }

    #[test]
    fn corrupt_identity_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(IDENTITY_FILE);
        fs::write(&path, b"definitely not a protobuf keypair").unwrap();
        assert!(load_or_create_identity(&path).is_err());
    }
}
