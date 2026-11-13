//! Gateway configuration, loaded from a single YAML file.
//!
//! There is no database: the whole routing table lives in this file.  A
//! `SIGHUP` (or `POST /admin/reload`) swaps in a freshly parsed copy of it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// Authentication scheme used towards one upstream channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum AuthHeader {
    /// Anthropic native: `x-api-key: <key>`.
    #[default]
    XApiKey,
    /// `Authorization: Bearer <key>`.
    Bearer,
    /// Send no credential header at all (trusted LAN upstream).
    None,
}

/// A model alias understood by one channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelMapping {
    pub from: String,
    pub to: String,
}

/// One upstream endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Channel {
    pub name: String,
    /// Base URL without the trailing `/v1/messages`.  It may contain a path
    /// prefix, e.g. `https://api.deepseek.com/anthropic`.
    pub base_url: String,
    /// Upstream credential.  Never logged.
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub auth: AuthHeader,
    /// Models this channel serves directly, by the name the *client* sends.
    #[serde(default)]
    pub models: Vec<String>,
    /// Alias -> real upstream model, applied before the request is sent.
    #[serde(default)]
    pub mappings: Vec<ModelMapping>,
    /// Hide the real name of a mapped model from direct routing.
    #[serde(default)]
    pub hide_mapped_models: bool,
    /// Higher wins.  Candidates at the highest priority are tried first.
    #[serde(default)]
    pub priority: i64,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Extra headers sent to this upstream (for fingerprint-gated relays).
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Do not replace the inbound `user-agent` with ours.
    #[serde(default = "default_true")]
    pub pass_through_user_agent: bool,
    /// Do not replace the inbound request body except the `model` field.
    #[serde(default = "default_true")]
    pub pass_through_body: bool,
}

fn default_true() -> bool {
    true
}

impl Channel {
    /// Resolve `model` as sent by a client to the model name this channel
    /// expects, together with the routing priority.
    ///
    /// Returns `None` when this channel does not serve the model.
    pub fn resolve(&self, model: &str) -> Option<ResolvedRoute> {
        if !self.enabled {
            return None;
        }
        if let Some(mapping) = self.mappings.iter().find(|m| m.from == model) {
            // A mapping target must itself be listed, mirroring AxonHub.
            if !self.models.iter().any(|m| m == &mapping.to) {
                return None;
            }
            return Some(ResolvedRoute {
                upstream_model: mapping.to.clone(),
                priority: self.priority,
            });
        }
        if !self.models.iter().any(|m| m == model) {
            return None;
        }
        if self.hide_mapped_models
            && self
                .mappings
                .iter()
                .any(|m| m.to == model && m.from != model)
        {
            return None;
        }
        Some(ResolvedRoute {
            upstream_model: model.to_string(),
            priority: self.priority,
        })
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedRoute {
    pub upstream_model: String,
    pub priority: i64,
}

/// Gateway-wide settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Address to bind, e.g. `127.0.0.1:8090`.
    #[serde(default = "default_listen")]
    pub listen: String,
    /// Accepted client credentials.  Empty means "accept any credential"
    /// (only sensible when bound to loopback).
    #[serde(default)]
    pub api_keys: Vec<String>,
    #[serde(default = "default_max_attempts")]
    pub max_attempts: usize,
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout_secs: u64,
    #[serde(default = "default_request_timeout")]
    pub request_timeout_secs: u64,
    /// Write one JSON access-log line per request to stderr.  Off by default:
    /// the point of this gateway is to do no durable per-request work.
    #[serde(default = "default_max_tokens")]
    pub default_max_tokens: u64,
    #[serde(default)]
    pub log_requests: bool,
    /// Optional directory for a rolling plain-text access log (no database).
    #[serde(default)]
    pub access_log: Option<PathBuf>,
    pub channels: Vec<Channel>,
}

fn default_listen() -> String {
    "127.0.0.1:8090".to_string()
}
fn default_max_attempts() -> usize {
    3
}
fn default_connect_timeout() -> u64 {
    10
}
fn default_request_timeout() -> u64 {
    1800
}
fn default_max_tokens() -> u64 {
    4096
}

/// Expand a `${VAR}` value from the environment.  Any other value is returned
/// unchanged, so literal keys still work for local-only deployments.
fn expand_env(value: &str) -> String {
    match value
        .strip_prefix("${")
        .and_then(|rest| rest.strip_suffix('}'))
    {
        Some(name) => std::env::var(name).unwrap_or_default(),
        None => value.to_string(),
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Arc<Config>> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read gateway config {}", path.display()))?;
        let mut config: Config = serde_yaml::from_str(&text)
            .with_context(|| format!("parse gateway config {}", path.display()))?;
        for channel in &mut config.channels {
            channel.api_key = expand_env(&channel.api_key);
        }
        config.validate()?;
        Ok(Arc::new(config))
    }

    pub fn validate(&self) -> Result<()> {
        if self.listen.trim().is_empty() {
            bail!("listen must not be empty");
        }
        if self.channels.is_empty() {
            bail!("at least one channel is required");
        }
        if self.max_attempts == 0 {
            bail!("max_attempts must be at least 1");
        }
        let mut names = std::collections::HashSet::new();
        for channel in &self.channels {
            if channel.name.trim().is_empty() {
                bail!("channel name must not be empty");
            }
            if !names.insert(channel.name.clone()) {
                bail!("duplicate channel name {}", channel.name);
            }
            if !channel.base_url.starts_with("http://") && !channel.base_url.starts_with("https://")
            {
                bail!(
                    "channel {} base_url must start with http:// or https://",
                    channel.name
                );
            }
            for mapping in &channel.mappings {
                if !channel.models.iter().any(|m| m == &mapping.to) {
                    tracing::warn!(
                        channel = %channel.name,
                        from = %mapping.from,
                        to = %mapping.to,
                        "mapping target is not in this channel's model list; the mapping will be ignored"
                    );
                }
            }
        }
        Ok(())
    }

    /// All routes serving `model`, highest priority first.
    pub fn routes(&self, model: &str) -> Vec<(&Channel, ResolvedRoute)> {
        let mut routes: Vec<(&Channel, ResolvedRoute)> = self
            .channels
            .iter()
            .filter_map(|channel| channel.resolve(model).map(|route| (channel, route)))
            .collect();
        routes.sort_by(|a, b| b.1.priority.cmp(&a.1.priority));
        routes
    }

    /// The client-facing model list, with mapped targets hidden when asked.
    pub fn public_models(&self) -> Vec<String> {
        let mut models = Vec::new();
        for channel in &self.channels {
            if !channel.enabled {
                continue;
            }
            for mapping in &channel.mappings {
                if !models.contains(&mapping.from) {
                    models.push(mapping.from.clone());
                }
            }
            for model in &channel.models {
                let hidden = channel.hide_mapped_models
                    && channel
                        .mappings
                        .iter()
                        .any(|m| &m.to == model && m.from != *model);
                if !hidden && !models.contains(model) {
                    models.push(model.clone());
                }
            }
        }
        models
    }
}
