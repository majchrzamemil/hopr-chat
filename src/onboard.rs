//! First-start onboarding for production mode, modelled on GnosisVPN:
//! identity file, then a Safe (existing or deployed by us), then the hoprd config.

use std::{io::Write, os::unix::fs::OpenOptionsExt, path::Path, time::Duration};

use anyhow::{Context, Result};
use edgli::{
    blokli::{make_incentive_operations, IncentiveOperations},
    hopr_lib::{builder::Keypair, HoprKeys, IdentityRetrievalModes},
    strategy::{minimum_balance_recommendation, IncentiveConfiguration},
    BlokliEndpoint,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    app::{SafeChoice, Status, Ui},
    settings::Settings,
};

pub const ID_FILE: &str = "hopr.id";
const PASS_FILE: &str = "hopr.pass";
const SAFE_FILE: &str = "hopr.safe";
pub const HOPRD_CONFIG: &str = "hoprd.yaml";

#[derive(Serialize, Deserialize)]
pub struct Safe {
    pub safe_address: String,
    pub module_address: String,
}

/// Fails instead of creating a new identity when `file` is missing.
pub fn load_keys(file: &Path, password: &str) -> Result<HoprKeys> {
    anyhow::ensure!(file.exists(), "identity file {} does not exist", file.display());
    let id_path = file.to_string_lossy();
    HoprKeys::try_from(IdentityRetrievalModes::FromFile { password, id_path: &id_path })
        .map_err(|e| anyhow::anyhow!("loading identity {}: {e}", file.display()))
}

pub fn address_of(keys: &HoprKeys) -> String {
    keys.chain_key.public().to_address().to_string()
}

/// Loads the node identity from the app home, creating it (and its password) on first start.
pub fn identity(home: &Path) -> Result<(HoprKeys, String, bool)> {
    let pass_file = home.join(PASS_FILE);
    if !pass_file.exists() {
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&pass_file)?;
        f.write_all(crate::hoprd::random_hex(24)?.as_bytes())?;
    }
    let password = std::fs::read_to_string(&pass_file)?.trim().to_string();
    let id_file = home.join(ID_FILE);
    let created = !id_file.exists();
    let id_path = id_file.to_string_lossy();
    let keys = HoprKeys::try_from(IdentityRetrievalModes::FromFile { password: &password, id_path: &id_path })
        .map_err(|e| anyhow::anyhow!("identity {}: {e}", id_file.display()))?;
    Ok((keys, password, created))
}

/// Returns the node's Safe: saved one, one already owned on chain, or one the user picks / we deploy.
pub async fn safe<U: Ui>(ui: &mut U, keys: &HoprKeys, blokli_url: &str, home: &Path) -> Result<Safe> {
    let file = home.join(SAFE_FILE);
    if let Ok(text) = std::fs::read_to_string(&file) {
        return Ok(serde_yaml::from_str(&text)?);
    }

    let endpoint = BlokliEndpoint::new(blokli_url.parse().context("blokli url")?);
    let ops = make_incentive_operations(endpoint, &keys.chain_key, None).await?;
    let safe = match ops.retrieve_safe().await? {
        Some(found) => Safe { safe_address: found.safe_address.to_string(), module_address: found.module_address.to_string() },
        None => match ui.choose_safe(&address_of(keys)).await? {
            SafeChoice::Existing { safe, module } => Safe { safe_address: safe, module_address: module },
            SafeChoice::Create => deploy(ui, &*ops, &address_of(keys)).await?,
        },
    };
    std::fs::write(&file, serde_yaml::to_string(&safe)?)?;
    ui.status(Status::SafeReady { safe: safe.safe_address.clone(), module: safe.module_address.clone() });
    Ok(safe)
}

/// Waits until the node wallet holds the recommended minimum, then moves all its wxHOPR into a new Safe.
async fn deploy<U: Ui>(ui: &mut U, ops: &dyn IncentiveOperations, address: &str) -> Result<Safe> {
    let need = minimum_balance_recommendation(ops, &IncentiveConfiguration::default()).await?;
    let wxhopr = loop {
        let (wxhopr, xdai) = ops.balances().await?;
        if wxhopr >= need.total_wxhopr() && xdai >= need.xdai_fund_amount {
            break wxhopr;
        }
        ui.status(Status::WaitingForFunds {
            address: address.to_string(),
            xdai: xdai.to_string(),
            wxhopr: wxhopr.to_string(),
            need_xdai: need.xdai_fund_amount.to_string(),
            need_wxhopr: need.total_wxhopr().to_string(),
        });
        tokio::time::sleep(Duration::from_secs(10)).await;
    };
    ui.status(Status::DeployingSafe);
    let deployed = ops.deploy_safe(wxhopr).await?;
    Ok(Safe { safe_address: deployed.safe_address.to_string(), module_address: deployed.module_address.to_string() })
}

/// Rewritten on every start so new defaults reach existing installs. Unset sections keep hoprd defaults
/// (including its channel lifecycle strategy).
pub fn write_hoprd_config(home: &Path, s: &Settings, password: &str, safe: &Safe, chat_target: &str) -> Result<()> {
    let cfg = json!({
        "hopr": {
            "announce": true,
            "host": { "address": { "IPv4": "0.0.0.0" }, "port": s.p2p_port },
            "safe_module": { "safe_address": safe.safe_address, "module_address": safe.module_address },
        },
        "identity": { "file": home.join(ID_FILE), "password": password },
        "db": { "data": home.join("hoprd-db"), "initialize": true },
        "api": { "enable": true, "host": { "address": { "IPv4": "127.0.0.1" }, "port": s.api_port } },
        "session_ip_forwarding": { "use_target_allow_list": true, "target_allow_list": [chat_target] },
        "blokli_url": s.blokli_url,
    });
    let path = home.join(HOPRD_CONFIG);
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&path)?;
    f.write_all(serde_yaml::to_string(&cfg)?.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hoprd_config_allows_only_our_target() {
        let home = std::env::temp_dir().join(format!("hopr-chat-onboard-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        let safe = Safe { safe_address: "0xsafe".into(), module_address: "0xmodule".into() };
        write_hoprd_config(&home, &Settings::new("a".into()), "pw", &safe, "127.0.0.1:25000").unwrap();
        let yaml: serde_yaml::Value = serde_yaml::from_str(&std::fs::read_to_string(home.join(HOPRD_CONFIG)).unwrap()).unwrap();
        assert_eq!(yaml["session_ip_forwarding"]["target_allow_list"][0], "127.0.0.1:25000");
        assert_eq!(yaml["session_ip_forwarding"]["use_target_allow_list"], true);
        assert_eq!(yaml["hopr"]["safe_module"]["module_address"], "0xmodule");
        assert_eq!(yaml["identity"]["password"], "pw");
        std::fs::remove_dir_all(home).unwrap();
    }
}

#[cfg(test)]
mod keystore_compat {
    /// `HOPR_CHAT_TEST_HOPRD_CFG=/tmp/hopr-nodes/hoprd_cfg_0.yaml cargo test -- --ignored`
    #[test]
    #[ignore]
    fn decrypts_hoprd_identity() {
        let cfg = std::env::var("HOPR_CHAT_TEST_HOPRD_CFG").unwrap();
        let (file, password) = crate::hoprd::read_identity(cfg.as_ref()).unwrap();
        let keys = super::load_keys(&file, &password).unwrap();
        println!("address {}", super::address_of(&keys));
    }
}
