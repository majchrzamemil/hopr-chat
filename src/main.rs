mod term;
mod tui;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use hopr_chat::{app, hoprd, settings};

#[derive(Parser)]
#[command(about = "Anonymous P2P chat over HOPR", args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    #[command(flatten)]
    prod: ProdArgs,
    #[command(flatten)]
    opts: OptsArgs,
}

/// Default mode: own node, onboarding on first start, bundled hoprd.
#[derive(Args)]
struct ProdArgs {
    /// Default: the per-OS app data dir (or HOPR_CHAT_HOME)
    #[arg(long)]
    home: Option<PathBuf>,
    /// Saved to settings; defaults to $USER on first start
    #[arg(long)]
    nick: Option<String>,
    /// Saved to settings
    #[arg(long, value_parser = settings::NETWORKS.map(|(name, _)| name))]
    network: Option<String>,
    /// Saved to settings; overrides --network
    #[arg(long)]
    blokli_url: Option<String>,
    /// Saved to settings. Relay hops both ways; 0 is direct and NOT anonymous
    #[arg(long)]
    hops: Option<u8>,
}

#[derive(Args)]
struct OptsArgs {
    #[arg(long, global = true, default_value_t = app::DEFAULT_OPTS.retry_secs)]
    retry_secs: u64,
    #[arg(long, global = true, default_value_t = app::DEFAULT_OPTS.ack_timeout)]
    ack_timeout: i64,
}

#[derive(Subcommand)]
enum Cmd {
    /// Chat on a localcluster node, starting the cluster if it is not running
    Dev {
        #[command(flatten)]
        chat: ChatArgs,
        #[command(flatten)]
        cluster: ClusterArgs,
        /// Cluster node to attach to (default: first one not used by another chat)
        #[arg(long)]
        node: Option<u64>,
    },
    /// Stop the localcluster started by `dev`
    DevDown {
        #[command(flatten)]
        cluster: ClusterArgs,
    },
    /// Chat through an already running hoprd
    Attach {
        #[command(flatten)]
        chat: ChatArgs,
        #[arg(long)]
        api: String,
        #[arg(long, env = "HOPRD_API_TOKEN")]
        token: Option<String>,
        /// hoprd config with the node identity, needed to sign messages
        #[arg(long)]
        hoprd_config: Option<PathBuf>,
    },
}

#[derive(Args)]
struct ChatArgs {
    #[arg(long)]
    nick: String,
    /// Default: ~/.hopr-chat/<nick>
    #[arg(long)]
    data: Option<PathBuf>,
    #[arg(long, env = "HOPR_CHAT_HOPS", default_value_t = 1)]
    hops: u8,
}

impl ChatArgs {
    fn data_dir(&self) -> Result<PathBuf> {
        match &self.data {
            Some(d) => Ok(d.clone()),
            None => settings::nick_dir(&self.nick),
        }
    }
}

#[derive(Args)]
struct ClusterArgs {
    #[arg(long, env = "HOPR_CHAT_LOCALCLUSTER_BIN", default_value = "hoprd-localcluster")]
    localcluster_bin: String,
    #[arg(long, env = "HOPR_CHAT_HOPRD_BIN", default_value = "hoprd")]
    hoprd_bin: String,
    #[arg(long, env = "HOPR_CHAT_CHAIN_IMAGE", default_value = hoprd::CHAIN_IMAGE)]
    chain_image: String,
    #[arg(long, default_value = "/tmp/hopr-nodes")]
    cluster_dir: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let ui = &mut tui::TuiUi::default();
    let opts = app::ChatOpts { retry_secs: cli.opts.retry_secs, ack_timeout: cli.opts.ack_timeout };
    match cli.cmd {
        None => {
            let p = cli.prod;
            let blokli_url = p.blokli_url.or(p.network.and_then(|n| settings::blokli_url(&n)));
            let home = match p.home {
                Some(h) => h,
                None => settings::home()?,
            };
            app::production(ui, app::Production { home, nick: p.nick, blokli_url, hops: p.hops }, opts).await
        }
        Some(Cmd::Dev { chat, cluster, node }) => {
            let dev = app::Dev {
                data_dir: chat.data_dir()?,
                nick: chat.nick,
                hops: chat.hops,
                localcluster_bin: cluster.localcluster_bin,
                hoprd_bin: cluster.hoprd_bin,
                chain_image: cluster.chain_image,
                cluster_dir: cluster.cluster_dir,
                node,
            };
            app::dev(ui, dev, opts).await
        }
        Some(Cmd::DevDown { cluster }) => {
            println!("{}", hoprd::dev_down(&cluster.localcluster_bin, &cluster.cluster_dir)?);
            Ok(())
        }
        Some(Cmd::Attach { chat, api, token, hoprd_config }) => {
            let attach = app::Attach { data_dir: chat.data_dir()?, nick: chat.nick, hops: chat.hops, api, token, hoprd_config };
            app::attach(ui, attach, opts).await
        }
    }
}
