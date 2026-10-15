//! Tauri frontend: `TauriUi` implements the same `Ui` trait as the TUI, so onboarding, the node and
//! the chat core run unchanged; the webview only renders and calls commands.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::Result;
use hopr_chat::{
    app::{self, SafeChoice, Status, Ui, DEFAULT_OPTS},
    chat::{compact, Chat, Event},
    db, hoprd, settings,
};
use serde::Serialize;
use tauri::{async_runtime::JoinHandle, AppHandle, Emitter, RunEvent, State};
use tokio::sync::{mpsc::UnboundedReceiver, oneshot, Notify};

/// What the webview needs to render after a reload: events emitted before it listened are lost.
#[derive(Serialize, Clone, Default)]
struct Snapshot {
    statuses: Vec<Status>,
    /// Node address waiting for a Safe choice.
    choose_safe: Option<String>,
    info: Option<Info>,
    fatal: Option<String>,
}

#[derive(Serialize, Clone)]
struct Info {
    nick: String,
    me: String,
    hops: u8,
    signing: bool,
}

#[derive(Serialize)]
struct WalletView {
    gas: String,
    node_wxhopr: String,
    funds: String,
    allowance: String,
    safe_xdai: String,
    /// `(critical, message)`
    warnings: Vec<(bool, String)>,
}

#[derive(Default)]
struct Shared {
    snapshot: Mutex<Snapshot>,
    chat: Mutex<Option<Arc<Chat>>>,
    safe_answer: Mutex<Option<oneshot::Sender<SafeChoice>>>,
    shutdown: Notify,
    core: Mutex<Option<JoinHandle<()>>>,
}

struct TauriUi {
    app: AppHandle,
    shared: Arc<Shared>,
}

impl Ui for TauriUi {
    async fn choose_safe(&mut self, node_address: &str) -> Result<SafeChoice> {
        let (tx, rx) = oneshot::channel();
        *self.shared.safe_answer.lock().unwrap() = Some(tx);
        self.shared.snapshot.lock().unwrap().choose_safe = Some(node_address.to_string());
        let _ = self.app.emit("choose_safe", node_address);
        let choice = rx.await?;
        self.shared.snapshot.lock().unwrap().choose_safe = None;
        Ok(choice)
    }

    fn status(&mut self, status: Status) {
        self.shared.snapshot.lock().unwrap().statuses.push(status.clone());
        let _ = self.app.emit("status", status);
    }

    async fn run(&mut self, chat: Arc<Chat>, mut events: UnboundedReceiver<Event>) -> Result<()> {
        let info = Info { nick: chat.nick().into(), me: chat.me().into(), hops: chat.hops(), signing: chat.signing() };
        *self.shared.chat.lock().unwrap() = Some(chat);
        self.shared.snapshot.lock().unwrap().info = Some(info.clone());
        let _ = self.app.emit("ready", info);

        let app = self.app.clone();
        let forward = tauri::async_runtime::spawn(async move {
            while let Some(ev) = events.recv().await {
                let _ = app.emit("chat_event", ev);
            }
        });
        self.shared.shutdown.notified().await;
        forward.abort();
        Ok(())
    }
}

/// Which node to chat through, picked from the environment so the same build covers all modes:
/// `HOPR_CHAT_DEV_NICK` (localcluster), `HOPR_CHAT_ATTACH_API` (running hoprd), else own node.
async fn run_core(ui: &mut TauriUi) -> Result<()> {
    let env = |k: &str| std::env::var(k).ok();
    let opts = DEFAULT_OPTS;
    let hops = env("HOPR_CHAT_HOPS").and_then(|h| h.parse().ok());

    if let Some(nick) = env("HOPR_CHAT_DEV_NICK") {
        let dev = app::Dev {
            data_dir: settings::nick_dir(&nick)?,
            nick,
            hops: hops.unwrap_or(1),
            localcluster_bin: env("HOPR_CHAT_LOCALCLUSTER_BIN").unwrap_or_else(|| "hoprd-localcluster".into()),
            hoprd_bin: env("HOPR_CHAT_HOPRD_BIN").unwrap_or_else(|| "hoprd".into()),
            chain_image: env("HOPR_CHAT_CHAIN_IMAGE").unwrap_or_else(|| hoprd::CHAIN_IMAGE.into()),
            cluster_dir: env("HOPR_CHAT_CLUSTER_DIR").map_or("/tmp/hopr-nodes".into(), PathBuf::from),
            node: env("HOPR_CHAT_DEV_NODE").and_then(|n| n.parse().ok()),
        };
        return app::dev(ui, dev, opts).await;
    }
    if let Some(api) = env("HOPR_CHAT_ATTACH_API") {
        let nick = env("HOPR_CHAT_NICK").unwrap_or_else(|| "me".into());
        let attach = app::Attach {
            data_dir: settings::nick_dir(&nick)?,
            nick,
            hops: hops.unwrap_or(1),
            api,
            token: env("HOPRD_API_TOKEN"),
            hoprd_config: env("HOPR_CHAT_HOPRD_CONFIG").map(PathBuf::from),
        };
        return app::attach(ui, attach, opts).await;
    }
    // No network picker in the GUI: always the build's default network.
    let blokli_url = Some(settings::NETWORKS[0].1.to_string());
    let production = app::Production { home: settings::home()?, nick: env("HOPR_CHAT_NICK"), blokli_url, hops };
    app::production(ui, production, opts).await
}

type CmdResult<T> = Result<T, String>;

fn chat(shared: &Shared) -> CmdResult<Arc<Chat>> {
    shared.chat.lock().unwrap().clone().ok_or_else(|| "chat is not running yet".to_string())
}

fn err(e: anyhow::Error) -> String {
    format!("{e:#}")
}

#[tauri::command]
fn snapshot(shared: State<'_, Arc<Shared>>) -> Snapshot {
    shared.snapshot.lock().unwrap().clone()
}

#[tauri::command]
fn answer_safe(shared: State<'_, Arc<Shared>>, choice: SafeChoice) -> CmdResult<()> {
    let tx = shared.safe_answer.lock().unwrap().take().ok_or("no Safe question pending")?;
    tx.send(choice).map_err(|_| "onboarding is no longer waiting".to_string())
}

#[tauri::command]
fn conversations(shared: State<'_, Arc<Shared>>) -> CmdResult<Vec<db::Conversation>> {
    chat(&shared)?.conversations().map_err(err)
}

#[tauri::command]
fn peer_names(shared: State<'_, Arc<Shared>>) -> CmdResult<Vec<String>> {
    Ok(chat(&shared)?.peer_names())
}

/// Also marks the conversation read.
#[tauri::command]
fn history(shared: State<'_, Arc<Shared>>, peer: String, limit: Option<i64>) -> CmdResult<Vec<db::Msg>> {
    chat(&shared)?.history(&peer, limit.unwrap_or(500)).map_err(err)
}

#[tauri::command]
async fn send(shared: State<'_, Arc<Shared>>, peer: String, text: String) -> CmdResult<()> {
    chat(&shared)?.send(&peer, &text).await.map_err(err)
}

#[tauri::command]
async fn add_peer(shared: State<'_, Arc<Shared>>, nick: String, address: String) -> CmdResult<()> {
    chat(&shared)?.add_peer(nick.trim(), address.trim()).await.map_err(err)
}

#[tauri::command]
fn peers(shared: State<'_, Arc<Shared>>) -> CmdResult<Vec<db::Peer>> {
    chat(&shared)?.peers().map_err(err)
}

/// Deletes the contact and its messages; returns how many messages went.
#[tauri::command]
async fn remove_peer(shared: State<'_, Arc<Shared>>, nick: String) -> CmdResult<usize> {
    chat(&shared)?.remove_peer(&nick).await.map_err(err)
}

/// `peer: None` deletes every message.
#[tauri::command]
fn purge(shared: State<'_, Arc<Shared>>, peer: Option<String>) -> CmdResult<usize> {
    chat(&shared)?.purge(peer.as_deref()).map_err(err)
}

#[tauri::command]
async fn wallet(shared: State<'_, Arc<Shared>>) -> CmdResult<WalletView> {
    let w = chat(&shared)?.wallet().await.map_err(err)?;
    Ok(WalletView {
        gas: compact(&w.native),
        node_wxhopr: compact(&w.hopr),
        funds: compact(&w.safe_hopr),
        allowance: compact(&w.safe_allowance),
        safe_xdai: compact(&w.safe_native),
        warnings: w.warnings(),
    })
}

/// Stops the chat, then waits for the core to tear hoprd down before the process exits. Onboarding
/// cannot be interrupted cleanly, so it is aborted.
fn shut_down(app: &AppHandle, shared: &Arc<Shared>) -> bool {
    let Some(core) = shared.core.lock().unwrap().take() else { return false };
    let running = shared.chat.lock().unwrap().is_some();
    let app = app.clone();
    let shared = shared.clone();
    tauri::async_runtime::spawn(async move {
        if running {
            shared.shutdown.notify_one();
            let abort = core.inner().abort_handle();
            if tokio::time::timeout(Duration::from_secs(15), core).await.is_err() {
                abort.abort();
            }
        } else {
            core.abort();
        }
        app.exit(0);
    });
    true
}

/// Points the core at the app sandbox and hands rustls the JVM so blokli's TLS can use
/// Android's trust store. Runs in setup: tao has initialised `ndk_context`, nothing reads env yet.
#[cfg(target_os = "android")]
fn android_init(app: &tauri::App) -> anyhow::Result<()> {
    use tauri::Manager;
    let cache = app.path().app_cache_dir()?;
    std::fs::create_dir_all(&cache)?;
    std::env::set_var("HOPR_CHAT_HOME", app.path().app_data_dir()?);
    std::env::set_var("TMPDIR", &cache);

    // Tauri forwards stderr to logcat; hoprd's own file log then stays off (one global subscriber).
    let _ = tracing_subscriber::fmt().with_writer(std::io::stderr).with_ansi(false).with_env_filter("info").try_init();

    let ctx = ndk_context::android_context();
    let vm = unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) };
    vm.attach_current_thread(|env| {
        let context = unsafe { jni::objects::JObject::from_raw(env, ctx.context().cast()) };
        rustls_platform_verifier::android::init_with_env(env, context)
    })?;
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let shared = Arc::new(Shared::default());
    let app = tauri::Builder::default()
        .manage(shared.clone())
        .invoke_handler(tauri::generate_handler![
            snapshot,
            answer_safe,
            conversations,
            peer_names,
            history,
            send,
            add_peer,
            peers,
            wallet,
            purge,
            remove_peer
        ])
        .setup({
            let shared = shared.clone();
            move |app| {
                #[cfg(target_os = "android")]
                android_init(app)?;
                let mut ui = TauriUi { app: app.handle().clone(), shared: shared.clone() };
                let core_shared = shared.clone();
                let handle = tauri::async_runtime::spawn(async move {
                    if let Err(e) = run_core(&mut ui).await {
                        let message = format!("{e:#}");
                        core_shared.snapshot.lock().unwrap().fatal = Some(message.clone());
                        let _ = ui.app.emit("fatal", message);
                    }
                });
                *shared.core.lock().unwrap() = Some(handle);
                Ok(())
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building hopr-chat");

    app.run(move |app, event| {
        if let RunEvent::ExitRequested { api, .. } = event {
            if shut_down(app, &shared) {
                api.prevent_exit();
            }
        }
    });
}
