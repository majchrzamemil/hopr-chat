use std::{
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use reqwest::{Method, RequestBuilder};
use serde_json::{json, Value};

pub struct Api {
    http: reqwest::Client,
    base: String,
    token: Option<String>,
}

impl Api {
    pub fn new(base: &str, token: Option<String>) -> Self {
        Self { http: reqwest::Client::new(), base: base.trim_end_matches('/').to_string(), token }
    }

    fn req(&self, method: Method, path: &str) -> RequestBuilder {
        let req = self.http.request(method, format!("{}{path}", self.base));
        match &self.token {
            Some(t) => req.header("X-Auth-Token", t),
            None => req,
        }
    }

    async fn call(&self, req: RequestBuilder) -> Result<Value> {
        let resp = req.send().await?;
        let status = resp.status();
        let body = resp.text().await?;
        if !status.is_success() {
            bail!("hoprd {status}: {body}");
        }
        Ok(serde_json::from_str(&body).unwrap_or(Value::Null))
    }

    pub async fn my_address(&self) -> Result<String> {
        let v = self.call(self.req(Method::GET, "/api/v4/account/addresses")).await?;
        v["native"].as_str().map(str::to_string).context("no native address in hoprd response")
    }

    /// hoprd can only route to peers whose packet key it learned from the chain announcement.
    pub async fn ensure_announced(&self, address: &str) -> Result<()> {
        let resp = self.req(Method::GET, &format!("/api/v4/peers/{address}")).send().await?;
        match resp.status() {
            s if s.is_success() => Ok(()),
            reqwest::StatusCode::NOT_FOUND => bail!("{address} is not an announced HOPR node"),
            s => bail!("hoprd {s}: {}", resp.text().await?),
        }
    }

    pub async fn balances(&self) -> Result<Value> {
        self.call(self.req(Method::GET, "/api/v4/account/balances")).await
    }

    async fn is_ready(&self) -> bool {
        self.call(self.req(Method::GET, "/readyz")).await.is_ok()
    }

    pub async fn wait_ready(&self, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        while !self.is_ready().await {
            if Instant::now() > deadline {
                bail!("hoprd at {} not ready after {timeout:?}", self.base);
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        Ok(())
    }

    /// Returns the local `ip:port` socket that tunnels into the session.
    pub async fn open_session(&self, destination: &str, target: &str, hops: u8) -> Result<String> {
        let body = json!({
            "destination": destination,
            "forwardPath": { "Hops": hops },
            "returnPath": { "Hops": hops },
            "target": { "Plain": target },
            "listenHost": "127.0.0.1:0",
        });
        let v = self.call(self.req(Method::POST, "/api/v4/session/tcp").json(&body)).await?;
        let ip = v["ip"].as_str().context("no ip in session response")?;
        let port = v["port"].as_u64().context("no port in session response")?;
        Ok(format!("{ip}:{port}"))
    }

    pub async fn close_session(&self, sock: &str) {
        let Some((ip, port)) = sock.rsplit_once(':') else { return };
        let _ = self.call(self.req(Method::DELETE, &format!("/api/v4/session/tcp/{ip}/{port}"))).await;
    }
}

fn cluster_status(localcluster_bin: &str, cluster_dir: &Path) -> Result<Value> {
    let out = Command::new(localcluster_bin)
        .arg("status")
        .arg("--data-dir")
        .arg(cluster_dir)
        .output()
        .with_context(|| format!("running `{localcluster_bin} status`"))?;
    serde_json::from_slice(&out.stdout).context("parsing localcluster status")
}

/// localcluster hides a dead docker daemon behind a 2-minute blokli timeout, so fail fast here.
fn check_container_runtime() -> Result<()> {
    let runtime = std::env::var("HOPRD_CONTAINER_RUNTIME").unwrap_or_else(|_| "docker".into());
    let args: &[&str] = if runtime.ends_with("container") { &["system", "status"] } else { &["info"] };
    let ok = Command::new(&runtime)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        bail!(
            "container runtime `{runtime}` is not usable (localcluster needs it for anvil + blokli). \
             Start it first: OrbStack `orbctl start`, Docker Desktop, `container system start`, \
             or `podman machine start`; pick another one with HOPRD_CONTAINER_RUNTIME"
        );
    }
    Ok(())
}

fn lock_held(lock: &Path) -> bool {
    std::fs::read_to_string(lock).is_ok_and(|pid| {
        Command::new("kill")
            .args(["-0", pid.trim()])
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    })
}

/// Starts localcluster if needed and claims a node. Returns the node API, its id and our lock file.
pub async fn dev_attach(
    localcluster_bin: &str,
    hoprd_bin: &str,
    chain_image: &str,
    cluster_dir: &Path,
    log: &Path,
    node: Option<u64>,
) -> Result<(Api, u64, PathBuf)> {
    let mut status = cluster_status(localcluster_bin, cluster_dir)?;
    let mut spawned = None;
    if status["state"] == "not_running" {
        check_container_runtime()?;
        let log_file = std::fs::File::create(log)?;
        // own process group: Ctrl-C in this chat must not take the shared cluster down
        let child = Command::new(localcluster_bin)
            .args(["--size", "3", "--frozen-identities", "--hoprd-bin", hoprd_bin])
            .args(["--chain-image", chain_image, "--data-dir"])
            .arg(cluster_dir)
            .stdin(Stdio::null())
            .stdout(log_file.try_clone()?)
            .stderr(log_file)
            .process_group(0)
            .spawn()
            .with_context(|| format!("spawning {localcluster_bin}"))?;
        spawned = Some(child);
    }

    let deadline = Instant::now() + Duration::from_secs(600);
    while status["state"] != "running" {
        if status["state"] == "failed" {
            bail!("localcluster failed: {}", status["error"]);
        }
        if let Some(exit) = spawned.as_mut().map(|c| c.try_wait()).transpose()?.flatten() {
            bail!("localcluster exited ({exit}), log {}:\n{}", log.display(), tail(log));
        }
        if Instant::now() > deadline {
            bail!("localcluster not running after 10 minutes, state: {}", status["state"]);
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
        status = cluster_status(localcluster_bin, cluster_dir)?;
    }

    for n in status["nodes"].as_array().context("no nodes in localcluster status")? {
        let id = n["id"].as_u64().context("node without id")?;
        let lock = cluster_dir.join(format!("chat-node-{id}.lock"));
        let wanted = match node {
            Some(want) => want == id,
            None => !lock_held(&lock),
        };
        if !wanted {
            continue;
        }
        std::fs::write(&lock, std::process::id().to_string())?;
        let api_url = n["api_url"].as_str().context("node without api_url")?;
        let api = Api::new(api_url, n["api_token"].as_str().map(str::to_string));
        return Ok((api, id, lock));
    }
    bail!("no free localcluster node, pick one with --node")
}

pub fn dev_down(localcluster_bin: &str, cluster_dir: &Path) -> Result<String> {
    let status = cluster_status(localcluster_bin, cluster_dir)?;
    let Some(pid) = status["pid"].as_u64() else {
        return Ok("localcluster not running".into());
    };
    Command::new("kill").args(["-TERM", &pid.to_string()]).status()?;
    Ok(format!("sent SIGTERM to localcluster (pid {pid})"))
}

/// Identity file and password from a hoprd config YAML (`HOPRD_PASSWORD` when the YAML has none).
pub fn read_identity(hoprd_config: &Path) -> Result<(PathBuf, String)> {
    let text = std::fs::read_to_string(hoprd_config).with_context(|| format!("reading {}", hoprd_config.display()))?;
    let cfg: serde_yaml::Value = serde_yaml::from_str(&text)?;
    let file = cfg["identity"]["file"].as_str().context("no identity.file in hoprd config")?;
    let password = match cfg["identity"]["password"].as_str() {
        Some(p) if !p.is_empty() => p.to_string(),
        _ => std::env::var("HOPRD_PASSWORD").context("no identity.password in hoprd config and HOPRD_PASSWORD unset")?,
    };
    Ok((file.into(), password))
}

fn tail(log: &Path) -> String {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let lines: Vec<_> = text.lines().collect();
    lines[lines.len().saturating_sub(10)..].join("\n")
}

/// Spawns hoprd as a child that dies with us and waits until it is ready, failing early if it exits.
/// First start can take long: hoprd registers the Safe and announces the node on chain.
pub async fn launch(hoprd_bin: &Path, config: &Path, api_port: u16, log_path: &Path) -> Result<(Api, tokio::process::Child)> {
    let token = random_hex(16)?;
    let log = std::fs::File::create(log_path)?;
    let child = tokio::process::Command::new(hoprd_bin)
        .arg("--configurationFilePath")
        .arg(config)
        .args(["--api", "--apiHost", "127.0.0.1", "--apiPort", &api_port.to_string()])
        .env("HOPRD_API_TOKEN", &token)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawning {}", hoprd_bin.display()))?;
    let mut child = child;

    let api = Api::new(&format!("http://127.0.0.1:{api_port}"), Some(token));
    while !api.is_ready().await {
        if let Some(exit) = child.try_wait()? {
            bail!("hoprd exited ({exit}), log {}:\n{}", log_path.display(), tail(log_path));
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    Ok((api, child))
}

pub fn random_hex(bytes: usize) -> Result<String> {
    use std::io::Read;
    let mut buf = vec![0u8; bytes];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// Runs hoprd inside this process (mobile has no sidecars), on its own runtime: hoprd wants
/// bigger thread stacks than the host app's. Dropping the returned sender stops the node.
#[cfg(feature = "embedded")]
pub async fn embed(
    keys: edgli::hopr_lib::HoprKeys,
    config: &Path,
    api_port: u16,
    log_path: &Path,
) -> Result<(Api, tokio::sync::oneshot::Sender<()>)> {
    use clap::Parser;

    let token = random_hex(16)?;
    let log = std::fs::File::create(log_path)?;
    let _ = tracing_subscriber::fmt()
        .with_writer(std::sync::Mutex::new(log))
        .with_ansi(false)
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .try_init();

    let config = config.to_str().context("config path")?;
    let port = api_port.to_string();
    let args = hoprd::cli::CliArgs::try_parse_from([
        "hoprd", "--configurationFilePath", config, "--api", "--apiHost", "127.0.0.1", "--apiPort", &port, "--apiToken", &token,
    ])?;
    let cfg = hoprd::config::HoprdConfig::try_from(args)?;

    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let (done, mut exited) = tokio::sync::oneshot::channel::<String>();
    std::thread::Builder::new().name("hoprd".into()).spawn(move || {
        let result = edgli::hopr_lib::prepare_tokio_runtime(None, None, None).and_then(|rt| {
            rt.block_on(async {
                tokio::select! {
                    r = hoprd::main_inner(cfg, keys) => r,
                    _ = stopped => Ok(()),
                }
            })
        });
        let _ = done.send(result.map_or_else(|e| format!("{e:#}"), |_| "stopped".into()));
    })?;

    let api = Api::new(&format!("http://127.0.0.1:{api_port}"), Some(token));
    while !api.is_ready().await {
        if let Ok(why) = exited.try_recv() {
            bail!("hoprd exited: {why}, log {}:\n{}", log_path.display(), tail(log_path));
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    Ok((api, stop))
}
