//! A chain's API as operations the load performs: `[adapter]` and `[workload]`.
//!
//! An adapter is a declaration, not code per chain: each operation is a JSON-RPC method, its
//! parameters with `{name}` holes, and the paths of the fields the load records. `kind` names a
//! built-in declaration a scenario may override call by call; a third chain is a new declaration.

use std::collections::BTreeMap;

use bohr_sdk_proto::json::Json;
use serde::Deserialize;

use crate::ConfigError;

/// One operation: the method, its parameters, the fields read from the reply, and the field that
/// must hold a value for the call to count.
#[derive(Clone, PartialEq, Debug)]
pub struct Operation {
    /// The JSON-RPC method.
    pub method: String,
    /// The parameters, with `{tx_base64}`, `{hash}` and `{hash_base64}` holes the load fills: a
    /// JSON-RPC body carries bytes as base64, where the submit answer gives the hash in hex.
    pub params: Json,
    /// Field name to its path in the reply, `result.…`.
    pub fields: BTreeMap<String, String>,
    /// `(path, value)` that must hold for the call to count, where there is one.
    pub holds: Option<(String, Json)>,
}

/// The `[adapter]` section: the three operations the load needs.
#[derive(Clone, PartialEq, Debug)]
pub struct Adapter {
    /// The built-in declaration it starts from.
    pub kind: String,
    /// The port every node answers JSON-RPC on.
    pub port: u16,
    /// Submit a transaction: records whether it was accepted and its hash.
    pub submit: Operation,
    /// Ask where a transaction landed: records its height and whether it succeeded.
    pub included: Operation,
    /// Ask a node its head: records its height.
    pub head: Operation,
    /// Ask a node one block, `{h}`: its transactions, compared byte for byte with what was sent
    /// when the node's index of transactions says one is absent.
    pub block: Option<Operation>,
}

/// The `[workload]` section: what the load sends, to whom, and for how long.
#[derive(Clone, PartialEq, Debug)]
pub struct Workload {
    /// The pre-signed transactions, one base64 line each, by their path inside the world.
    pub txs: String,
    /// Transactions sent per virtual second.
    pub rate: u64,
    /// The nodes that receive submissions, in rotation.
    pub targets: Vec<u32>,
    /// Every node of the chain, each asked its head and, at the end, every committed transaction.
    pub nodes: Vec<u32>,
    /// Virtual time between two rounds of `head`.
    pub observe_every_ticks: u64,
    /// Virtual time transactions are sent for; the fault window's close where none is given.
    pub window_ticks: u64,
    /// Virtual time between the window closing and the final check.
    pub settle_ticks: u64,
    /// The chain's API.
    pub adapter: Adapter,
}

impl Workload {
    /// The plan `bohr-load` takes as its one argument.
    #[must_use]
    pub fn plan(&self) -> String {
        let endpoints = |nodes: &[u32]| {
            Json::Array(
                nodes
                    .iter()
                    .map(|node| {
                        Json::Object(BTreeMap::from([
                            ("node".to_owned(), Json::Int(i64::from(*node))),
                            ("port".to_owned(), Json::Int(i64::from(self.adapter.port))),
                        ]))
                    })
                    .collect(),
            )
        };
        let int = |n: u64| Json::Int(i64::try_from(n).unwrap_or(i64::MAX));
        Json::Object(
            [
                ("targets".to_owned(), endpoints(&self.targets)),
                ("nodes".to_owned(), endpoints(&self.nodes)),
                ("txs".to_owned(), Json::Str(self.txs.clone())),
                ("rate".to_owned(), int(self.rate)),
                ("observe_every".to_owned(), int(self.observe_every_ticks)),
                ("window".to_owned(), int(self.window_ticks)),
                ("settle".to_owned(), int(self.settle_ticks)),
                ("submit".to_owned(), operation_json(&self.adapter.submit)),
                ("included".to_owned(), operation_json(&self.adapter.included)),
                ("head".to_owned(), operation_json(&self.adapter.head)),
            ]
            .into_iter()
            .chain(self.adapter.block.as_ref().map(|b| ("block".to_owned(), operation_json(b))))
            .collect::<BTreeMap<_, _>>(),
        )
        .to_json()
    }
}

fn operation_json(op: &Operation) -> Json {
    let mut out = BTreeMap::from([
        ("method".to_owned(), Json::Str(op.method.clone())),
        ("params".to_owned(), op.params.clone()),
        (
            "fields".to_owned(),
            Json::Object(
                op.fields.iter().map(|(k, v)| (k.clone(), Json::Str(v.clone()))).collect(),
            ),
        ),
    ]);
    if let Some((field, equals)) = &op.holds {
        out.insert(
            "holds".to_owned(),
            Json::Object(BTreeMap::from([
                ("field".to_owned(), Json::Str(field.clone())),
                ("equals".to_owned(), equals.clone()),
            ])),
        );
    }
    Json::Object(out)
}

/// The declaration `kind = "cometbft-rpc"` starts from: CometBFT and every Cosmos SDK chain.
const COMETBFT_RPC: &str = r#"
port = 26657
[submit]
rpc = "broadcast_tx_sync"
params = { tx = "{tx_base64}" }
accepted = { field = "result.code", equals = 0 }
hash = "result.hash"
[included]
rpc = "tx"
params = { hash = "{hash_base64}" }
height = "result.height"
succeeded = { field = "result.tx_result.code", equals = 0 }
[head]
rpc = "block"
params = {}
height = "result.block.header.height"
[block]
rpc = "block"
params = { height = "{h}" }
txs = "result.block.data.txs"
"#;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawAdapter {
    kind: Option<String>,
    port: Option<u16>,
    submit: Option<RawOperation>,
    included: Option<RawOperation>,
    head: Option<RawOperation>,
    block: Option<RawOperation>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOperation {
    rpc: String,
    params: Option<toml::Value>,
    accepted: Option<RawHolds>,
    succeeded: Option<RawHolds>,
    hash: Option<String>,
    height: Option<String>,
    block_hash: Option<String>,
    state_hash: Option<String>,
    txs: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHolds {
    field: String,
    equals: toml::Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawWorkload {
    txs: String,
    rate: Option<u64>,
    targets: Vec<u32>,
    observe_every_ticks: Option<u64>,
    window_ticks: Option<u64>,
    settle_ticks: Option<u64>,
}

/// Reads `[workload]` and `[adapter]` together, since neither means anything alone.
///
/// `window_default` is where the fault window closes, which a workload sends until unless it says
/// otherwise.
pub(crate) fn workload(
    raw: Option<&RawWorkload>,
    adapter: Option<&RawAdapter>,
    nodes: u32,
    window_default: Option<u64>,
) -> Result<Option<Workload>, ConfigError> {
    let refused = |m: String| ConfigError::Refused(format!("BOHR-1002: {m}"));
    let (raw, adapter) = match (raw, adapter) {
        (None, None) => return Ok(None),
        (Some(_), None) => {
            return Err(refused(
                "[workload] sends a chain transactions through its API, and no [adapter] says \
                 what the API is"
                    .to_owned(),
            ));
        }
        (None, Some(_)) => {
            return Err(refused(
                "[adapter] declares a chain's API, and no [workload] uses it".to_owned(),
            ));
        }
        (Some(raw), Some(adapter)) => (raw, adapter),
    };
    if raw.targets.is_empty() || raw.targets.iter().any(|t| *t >= nodes) {
        return Err(refused(format!(
            "workload.targets must name nodes of the cluster, 0 to {}",
            nodes.saturating_sub(1)
        )));
    }
    let window_ticks = raw.window_ticks.or(window_default).ok_or_else(|| {
        refused(
            "workload.window_ticks is needed where no faults.disarm_after_ticks closes a window"
                .to_owned(),
        )
    })?;
    Ok(Some(Workload {
        txs: raw.txs.clone(),
        rate: raw.rate.unwrap_or(20).max(1),
        targets: raw.targets.clone(),
        nodes: (0..nodes).collect(),
        observe_every_ticks: raw.observe_every_ticks.unwrap_or(1_000_000_000),
        window_ticks,
        settle_ticks: raw.settle_ticks.unwrap_or(10_000_000_000),
        adapter: adapter_of(adapter)?,
    }))
}

fn adapter_of(raw: &RawAdapter) -> Result<Adapter, ConfigError> {
    let refused = |m: String| ConfigError::Refused(format!("BOHR-1002: {m}"));
    let kind = raw.kind.clone().unwrap_or_else(|| "cometbft-rpc".to_owned());
    let base: RawAdapter = match kind.as_str() {
        "cometbft-rpc" => toml::from_str(COMETBFT_RPC)
            .map_err(|e| refused(format!("the built-in cometbft-rpc declaration: {e}")))?,
        "custom" => RawAdapter::default(),
        other => {
            return Err(refused(format!(
                "adapter.kind has no declaration named {other:?}; this build knows cometbft-rpc, \
                 or custom with every call written out"
            )));
        }
    };
    let pick = |own: &Option<RawOperation>, base: &Option<RawOperation>, name: &str| {
        own.clone()
            .or_else(|| base.clone())
            .ok_or_else(|| refused(format!("[adapter.{name}] is needed under kind {kind:?}")))
    };
    let submit = pick(&raw.submit, &base.submit, "submit")?;
    let included = pick(&raw.included, &base.included, "included")?;
    let head = pick(&raw.head, &base.head, "head")?;
    Ok(Adapter {
        kind: kind.clone(),
        port: raw.port.or(base.port).unwrap_or(26657),
        submit: operation(&submit, &[("hash", submit.hash.as_ref())], submit.accepted.as_ref()),
        included: operation(
            &included,
            &[("height", included.height.as_ref())],
            included.succeeded.as_ref(),
        ),
        head: operation(
            &head,
            &[
                ("height", head.height.as_ref()),
                ("block_hash", head.block_hash.as_ref()),
                ("state_hash", head.state_hash.as_ref()),
                ("txs", head.txs.as_ref()),
            ],
            None,
        ),
        block: raw
            .block
            .clone()
            .or_else(|| base.block.clone())
            .map(|block| operation(&block, &[("txs", block.txs.as_ref())], None)),
    })
}

fn operation(
    raw: &RawOperation,
    fields: &[(&str, Option<&String>)],
    holds: Option<&RawHolds>,
) -> Operation {
    Operation {
        method: raw.rpc.clone(),
        params: raw.params.as_ref().map_or_else(|| Json::Object(BTreeMap::new()), json_of),
        fields: fields
            .iter()
            .filter_map(|(name, path)| Some(((*name).to_owned(), (*path)?.clone())))
            .collect(),
        holds: holds.map(|h| (h.field.clone(), json_of(&h.equals))),
    }
}

/// A TOML value as the JSON a guest is handed.
fn json_of(value: &toml::Value) -> Json {
    match value {
        toml::Value::String(s) => Json::Str(s.clone()),
        toml::Value::Integer(n) => Json::Int(*n),
        toml::Value::Boolean(b) => Json::Bool(*b),
        toml::Value::Float(f) => Json::Str(f.to_string()),
        toml::Value::Datetime(d) => Json::Str(d.to_string()),
        toml::Value::Array(items) => Json::Array(items.iter().map(json_of).collect()),
        toml::Value::Table(map) => {
            Json::Object(map.iter().map(|(k, v)| (k.clone(), json_of(v))).collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_built_in_declaration_fills_every_call_and_a_scenario_overrides_one() {
        let adapter: RawAdapter = toml::from_str(
            r#"kind = "cometbft-rpc"
               [head]
               rpc = "status"
               height = "result.sync_info.latest_block_height""#,
        )
        .expect("toml");
        let workload: RawWorkload = toml::from_str(
            r#"txs = "/workload/txs"
                              targets = [0, 1]"#,
        )
        .expect("toml");
        let built = workload_of(&workload, &adapter);
        assert_eq!(built.adapter.submit.method, "broadcast_tx_sync");
        assert_eq!(built.adapter.head.method, "status");
        assert_eq!(built.adapter.port, 26657);
        let plan = built.plan();
        assert!(plan.contains(r#""holds":{"equals":0,"field":"result.code"}"#), "{plan}");
        assert!(plan.contains(r#""window":30"#), "{plan}");
    }

    #[test]
    fn a_workload_without_an_adapter_or_past_the_cluster_is_refused() {
        let workload: RawWorkload = toml::from_str(
            r#"txs = "t"
                              targets = [4]"#,
        )
        .expect("toml");
        assert!(super::workload(Some(&workload), None, 4, Some(1)).is_err());
        let adapter = RawAdapter::default();
        assert!(super::workload(Some(&workload), Some(&adapter), 4, Some(1)).is_err());
    }

    fn workload_of(workload: &RawWorkload, adapter: &RawAdapter) -> Workload {
        super::workload(Some(workload), Some(adapter), 4, Some(30)).expect("valid").expect("some")
    }
}
