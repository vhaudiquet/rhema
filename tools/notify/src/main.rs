//! The `notify` client: push one notification to a device. The daemon
//! queues the row; the device's next poll-and-ack carries it. One
//! subcommand, one call, one JSON line out — the thin-client shape
//! every rhema tool follows (spec: rhema-tools §3).

use clap::{Parser, Subcommand};
use rhema_tools_common::{Client, Config, fail, print};
use rhema_claw_tools_contract::NotificationPush;

/// Push a notification to the user's devices.
#[derive(Parser)]
#[command(name = "notify", version, about, long_about = None)]
struct Cli {
    /// Present for readability of examples; output is always JSON.
    #[arg(long)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Push one notification: the daemon queues it, the device's next
    /// poll-and-ack delivers it.
    Push {
        /// The notification's title line.
        #[arg(long)]
        title: String,
        /// The notification's body text.
        #[arg(long)]
        body: String,
        /// The device to deliver to; the daemon's default when unset.
        #[arg(long)]
        device: Option<String>,
    },
}

fn main() {
    let Cli { json: _, command } = Cli::parse();
    let client = Client::new(Config::from_env());
    match command {
        Command::Push {
            title,
            body,
            device,
        } => {
            let push = NotificationPush {
                title,
                body,
                device,
            };
            match client.call("POST", rhema_claw_tools_contract::NOTIFICATIONS, Some(&push)) {
                Ok(record) => print(&record),
                Err(error) => fail(error),
            }
        }
    }
}
