//! Wires onboarding, hoprd and the chat core together behind the `Ui` trait, so the TUI and the
//! GUI share everything below this module.

use std::{path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use edgli::blokli::ChainKeypair;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::{
    chat::{self, Chat, Config, Event},
    hoprd, onboard,
    settings::{self, Settings},
};

/// What a frontend has to provide. Calls happen in order: `choose_safe` (first start only),
/// any number of `status`, then one `run` for the lifetime of the chat.
#[allow(async_fn_in_trait)]
pub trait Ui {
    /// First start without a Safe: use an existing one, or have us deploy one.
    async fn choose_safe(&mut self, node_address: &str) -> Result<SafeChoice>;
    /// Onboarding and startup progress.
    fn status(&mut self, status: Status);
    /// Main loop: render `events`, drive `chat` from user input, return when the user quits.
    async fn run(&mut self, chat: Arc<Chat>, events: UnboundedReceiver<Event>) -> Result<()>;
}

#[derive(Deserialize, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SafeChoice {
    Existing { safe: String, module: String },
    Create,
}

#[derive(Serialize, Clone, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Status {
    Identity { address: String, created: bool },
    WaitingForFunds { address: String, xdai: String, wxhopr: String, need_xdai: String, need_wxhopr: String },
    DeployingSafe,
    SafeReady { safe: String, module: String },
    StartingCluster { log: PathBuf },
    StartingHoprd { log: PathBuf },
    Unsigned { reason: String },
    Ready,
}

/// Tunables shared by all modes.
pub struct ChatOpts {
    pub retry_secs: u64,
    pub ack_timeout: i64,
}

pub const DEFAULT_OPTS: ChatOpts = ChatOpts { retry_secs: 15, ack_timeout: 30 };

pub struct Production {
    pub home: PathBuf,
    pub nick: Option<String>,
    pub blokli_url: Option<String>,
    pub hops: Option<u8>,
}

pub struct Dev {
    pub nick: String,
    pub data_dir: PathBuf,
    pub hops: u8,
    pub localcluster_bin: String,
    pub hoprd_bin: String,
    pub chain_image: String,
    pub cluster_dir: PathBuf,
    pub node: Option<u64>,
}

pub struct Attach {
    pub nick: String,
    pub data_dir: PathBuf,
    pub hops: u8,
    pub api: String,
    pub token: Option<String>,
    pub hoprd_config: Option<PathBuf>,
}

/// Own node: onboarding on first start, then hoprd (in-process, or a child without `embedded`).
/// Returns only after hoprd is down, so the process can exit right after.
pub async fn production<U: Ui>(ui: &mut U, p: Production, opts: ChatOpts) -> Result<()> {
    std::fs::create_dir_all(&p.home)?;
    let settings_path = p.home.join("settings.toml");
    let mut s = settings::load(&settings_path)?.unwrap_or_else(|| Settings::new(default_nick()));
    s.nick = p.nick.unwrap_or(s.nick);
    if let Some(url) = p.blokli_url.filter(|url| *url != s.blokli_url) {
        // The chain index belongs to the old network; hoprd rebuilds it from blokli.
        let _ = std::fs::remove_dir_all(p.home.join("hoprd-db"));
        s.blokli_url = url;
    }
    s.hops = p.hops.unwrap_or(s.hops);
    settings::save(&settings_path, &s)?;
    // TUI and GUI share this home and its node, so only one of them may run at a time.
    std::net::TcpListener::bind(("127.0.0.1", s.api_port))
        .with_context(|| format!("port {} is busy: is hopr-chat (TUI or desktop app) already running?", s.api_port))?;

    let (keys, password, created) = onboard::identity(&p.home)?;
    let address = onboard::address_of(&keys);
    ui.status(Status::Identity { address: address.clone(), created });
    let safe = onboard::safe(ui, &keys, &s.blokli_url, &p.home).await?;
    onboard::write_hoprd_config(&p.home, &s, &password, &safe, &chat::chat_target(&address))?;

    let log = p.home.join("hoprd.log");
    ui.status(Status::StartingHoprd { log: log.clone() });
    let config = p.home.join(onboard::HOPRD_CONFIG);
    #[cfg(feature = "embedded")]
    let (api, stop_hoprd) = hoprd::embed(keys.clone(), &config, s.api_port, &log).await?;
    #[cfg(not(feature = "embedded"))]
    let (api, stop_hoprd) = hoprd::launch(&hoprd_bin(&s), &config, s.api_port, &log).await?;
    let cfg = Config {
        nick: s.nick,
        data_dir: p.home,
        hops: s.hops,
        retry_secs: opts.retry_secs,
        ack_timeout: opts.ack_timeout,
        key: Some(keys.chain_key),
    };
    let res = serve(ui, cfg, api).await;
    let _ = tokio::task::spawn_blocking(stop_hoprd).await;
    res
}

/// localcluster node; the node's identity comes from the config localcluster generated for it.
pub async fn dev<U: Ui>(ui: &mut U, d: Dev, opts: ChatOpts) -> Result<()> {
    std::fs::create_dir_all(&d.data_dir)?;
    let log = d.data_dir.join("localcluster.log");
    ui.status(Status::StartingCluster { log: log.clone() });
    // The same nick keeps the same node, and so the same address its peers saved.
    let node_file = d.data_dir.join("dev-node");
    let remembered = std::fs::read_to_string(&node_file).ok().and_then(|id| id.trim().parse().ok());
    let (api, id, lock) =
        hoprd::dev_attach(&d.localcluster_bin, &d.hoprd_bin, &d.chain_image, &d.cluster_dir, &log, d.node, remembered).await?;
    std::fs::write(&node_file, id.to_string())?;
    let key = node_key(ui, Some(d.cluster_dir.join(format!("hoprd_cfg_{id}.yaml"))));
    let cfg = Config { nick: d.nick, data_dir: d.data_dir, hops: d.hops, retry_secs: opts.retry_secs, ack_timeout: opts.ack_timeout, key };
    let res = serve(ui, cfg, api).await;
    let _ = std::fs::remove_file(lock);
    res
}

pub async fn attach<U: Ui>(ui: &mut U, a: Attach, opts: ChatOpts) -> Result<()> {
    let api = hoprd::Api::new(&a.api, a.token);
    api.wait_ready(std::time::Duration::from_secs(30)).await?;
    let key = node_key(ui, a.hoprd_config);
    let cfg = Config { nick: a.nick, data_dir: a.data_dir, hops: a.hops, retry_secs: opts.retry_secs, ack_timeout: opts.ack_timeout, key };
    serve(ui, cfg, api).await
}

async fn serve<U: Ui>(ui: &mut U, cfg: Config, api: hoprd::Api) -> Result<()> {
    std::fs::create_dir_all(&cfg.data_dir)?;
    let (chat, events) = chat::start(cfg, api).await?;
    ui.status(Status::Ready);
    let res = ui.run(chat.clone(), events).await;
    chat.shutdown().await;
    res
}

/// Signing is optional: a missing or unreadable identity degrades to unsigned messages.
fn node_key<U: Ui>(ui: &mut U, hoprd_config: Option<PathBuf>) -> Option<ChainKeypair> {
    let Some(path) = hoprd_config else {
        ui.status(Status::Unsigned { reason: "no hoprd config given".into() });
        return None;
    };
    let loaded = hoprd::read_identity(&path).and_then(|(file, password)| onboard::load_keys(&file, &password));
    match loaded {
        Ok(keys) => Some(keys.chain_key),
        Err(e) => {
            ui.status(Status::Unsigned { reason: format!("{e:#}") });
            None
        }
    }
}

/// Settings, then `HOPR_CHAT_HOPRD_BIN`, then the binary bundled next to ours, then PATH.
#[cfg(not(feature = "embedded"))]
fn hoprd_bin(s: &Settings) -> PathBuf {
    s.hoprd_bin
        .clone()
        .or_else(|| std::env::var_os("HOPR_CHAT_HOPRD_BIN").map(PathBuf::from))
        .or_else(|| {
            let bundled = std::env::current_exe().ok()?.parent()?.join("hoprd");
            bundled.exists().then_some(bundled)
        })
        .unwrap_or_else(|| "hoprd".into())
}

fn default_nick() -> String {
    std::env::var("USER").unwrap_or_else(|_| "anon".into())
}
