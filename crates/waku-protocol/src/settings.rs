use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use ts_rs::TS;

use crate::computer_use::ComputerAppGrant;
use crate::model::ProviderKind;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, TS)]
#[serde(default)]
#[derive(Default)]
pub struct DaemonSettings {
    pub computer_use_enabled: bool,
    pub computer_use_allowed_apps: Vec<ComputerAppGrant>,
    pub disabled_providers: Vec<ProviderKind>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub provider_binary_overrides: HashMap<ProviderKind, String>,
    /// The relay this daemon dials, when it cannot be reached directly. `None`
    /// means the daemon only listens, which is the default and the only mode
    /// the desktop app uses.
    ///
    /// This lives in settings rather than in a new config file because the
    /// daemon already owns exactly one settings document, and a second source
    /// would mean two places to look when a relay never connects.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_url: Option<String>,
    /// Whether this daemon should also keep listening on its bind address
    /// while dialing the relay. Off by default: a relayed daemon is usually on
    /// a host that cannot be reached inbound anyway, and an extra listener is
    /// just another surface to keep locked down.
    #[serde(default)]
    pub relay_keep_listening: bool,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl DaemonSettings {
    pub fn default_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join(".waku")
            .join("settings.json")
    }

    pub fn discard_legacy_app_keys(&mut self) {
        for key in ["analytics_enabled", "favorite_models", "theme", "language"] {
            self.extra.remove(key);
        }
    }
}
