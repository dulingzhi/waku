use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use waku_relay::{Limits, Registry};

fn main() -> Result<()> {
    let arguments = Arguments::parse(std::env::args().skip(1))?;
    let listener = TcpListener::bind(&arguments.bind)
        .with_context(|| format!("could not bind waku-relay to {}", arguments.bind))?;
    let registry = Arc::new(Registry::new(arguments.limits));
    println!(
        "{}",
        serde_json::json!({
            "address": listener.local_addr()?.to_string(),
            "role": "relay",
        })
    );
    waku_relay::serve(listener, registry, waku_relay::Silent)
}

struct Arguments {
    bind: String,
    limits: Limits,
}

impl Arguments {
    fn parse(arguments: impl IntoIterator<Item = String>) -> Result<Self> {
        let mut bind = "127.0.0.1:0".to_owned();
        let mut limits = Limits::default();
        let mut arguments = arguments.into_iter();
        while let Some(argument) = arguments.next() {
            match argument.as_str() {
                "--bind" => {
                    bind = arguments.next().context("--bind requires an address")?;
                }
                "--pair-timeout-seconds" => {
                    limits.pair_timeout = std::time::Duration::from_secs(
                        arguments
                            .next()
                            .context("--pair-timeout-seconds requires a number")?
                            .parse()
                            .context("--pair-timeout-seconds is not a number")?,
                    );
                }
                "--max-session-megabytes" => {
                    let megabytes: u64 = arguments
                        .next()
                        .context("--max-session-megabytes requires a number")?
                        .parse()
                        .context("--max-session-megabytes is not a number")?;
                    limits.session_bytes = megabytes.saturating_mul(1024 * 1024);
                }
                "--help" | "-h" => {
                    println!(
                        "usage: {} [--bind ADDRESS] [--pair-timeout-seconds N] [--max-session-megabytes N]",
                        env!("CARGO_BIN_NAME")
                    );
                    std::process::exit(0);
                }
                unknown => anyhow::bail!("unknown argument {unknown:?}"),
            }
        }
        Ok(Self { bind, limits })
    }
}

/// Kept here so the binary and tests agree on what an address looks like.
pub fn describe(address: SocketAddr) -> String {
    address.to_string()
}
