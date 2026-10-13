//! Terminal helpers shared by the TUI and the CLI: plain-stdout onboarding before either takes over.

use anyhow::{bail, Result};
use hopr_chat::{
    app::{SafeChoice, Status},
    db,
};
use tokio::io::{AsyncBufReadExt, BufReader};

pub const GREEN: u8 = 32;
pub const YELLOW: u8 = 33;
pub const DIM: u8 = 2;

pub fn paint(color: u8, s: &str) -> String {
    format!("\x1b[{color}m{s}\x1b[0m")
}

pub fn ago(ts: i64) -> String {
    match db::now() - ts {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86400),
    }
}

pub async fn choose_safe(node_address: &str) -> Result<SafeChoice> {
    println!("\nThis node ({node_address}) has no Safe yet.");
    println!("  1) use an existing Safe (this node must already be added to its module, e.g. via the HOPR staking hub)");
    println!("  2) create a new Safe (you fund this node with xDai + wxHOPR, the app deploys the Safe)");
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut ask = async |q: &str| -> Result<String> {
        println!("{q}");
        match lines.next_line().await? {
            Some(l) => Ok(l.trim().to_string()),
            None => bail!("stdin closed during onboarding"),
        }
    };
    loop {
        match ask("choice [1/2]:").await?.as_str() {
            "1" => {
                let safe = ask("Safe address:").await?;
                let module = ask("Module address:").await?;
                return Ok(SafeChoice::Existing { safe, module });
            }
            "2" => return Ok(SafeChoice::Create),
            _ => println!("type 1 or 2"),
        }
    }
}

/// `funding_shown` makes the funding instructions appear once, followed by one line per poll.
pub fn print_status(status: Status, funding_shown: &mut bool) {
    match status {
        Status::Identity { address, created } => {
            let verb = if created { "created" } else { "loaded" };
            println!("{} node identity {address}", paint(DIM, verb));
        }
        Status::WaitingForFunds { address, xdai, wxhopr, need_xdai, need_wxhopr } => {
            if !*funding_shown {
                *funding_shown = true;
                println!("\n{}", paint(YELLOW, "Fund this node address on Gnosis Chain to deploy its Safe:"));
                println!("  {}", paint(GREEN, &address));
                println!("  needs at least {need_xdai} and {need_wxhopr}\n");
            }
            println!("{}", paint(DIM, &format!("waiting for funds: {xdai} / {need_xdai}, {wxhopr} / {need_wxhopr}")));
        }
        Status::DeployingSafe => println!("{}", paint(YELLOW, "funds arrived, deploying Safe...")),
        Status::SafeReady { safe, module } => println!("{} {safe} (module {module})", paint(GREEN, "Safe")),
        Status::StartingCluster { log } => {
            println!("{}", paint(DIM, &format!("attaching to localcluster, log {}", log.display())))
        }
        Status::StartingHoprd { log } => println!(
            "{}",
            paint(DIM, &format!("starting hoprd (first start registers + announces on chain, can take minutes), log {}", log.display()))
        ),
        Status::Unsigned { reason } => println!("{}", paint(YELLOW, &format!("messages will be UNSIGNED: {reason}"))),
        Status::Ready => {}
    }
}
