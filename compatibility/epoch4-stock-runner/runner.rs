use std::{collections::BTreeSet, fs, path::PathBuf};

use anyhow::{bail, ensure, Context, Result};
use blockstack_lib::{
    chainstate::{
        nakamoto::{NakamotoBlockHeader, NAKAMOTO_BLOCK_VERSION_EPOCH_4},
        stacks::{MAX_BLOCK_LEN, MAX_TRANSACTION_LEN},
    },
    core::{
        BITCOIN_MAINNET_FIRST_BLOCK_HEIGHT, BITCOIN_MAINNET_STACKS_40_BURN_HEIGHT,
        BLOCK_LIMIT_MAINNET_40, STACKS_EPOCHS_MAINNET,
    },
};
use clarity::vm::{ClarityVersion, Value};
use serde::Deserialize;
use serde_json::{json, Value as JsonValue};
use stacks_common::{
    consts::{CHAIN_ID_MAINNET, NETWORK_ID_MAINNET, PEER_VERSION_MAINNET},
    types::{StacksEpochId, MINING_COMMITMENT_WINDOW},
};

const PROFILE_ID: &str = "stacks-mainnet-epoch-4.0-v1";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Suite {
    schema_version: u64,
    profile: String,
    vectors: Vec<Vector>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Vector {
    id: String,
    surface: String,
    evidence: Vec<String>,
    input: JsonValue,
    expected: JsonValue,
}

#[derive(Deserialize)]
struct Profile {
    schema_version: u64,
    id: String,
    domain: Domain,
    network: Network,
    activation: Activation,
    limits: Limits,
    vm: Vm,
    policies: Policies,
    reference_revisions: Vec<String>,
}

#[derive(Deserialize)]
struct Domain {
    network: String,
    first_supported_burn_height: u64,
    last_supported_burn_height: Option<u64>,
}

#[derive(Deserialize)]
struct Network {
    chain_id: u64,
    network_id: u64,
    peer_version: u64,
    first_burn_height: u64,
}

#[derive(Deserialize)]
struct Activation {
    semantic_epoch: String,
    peer_epoch: u64,
    burn_height: u64,
    nakamoto_block_version: u64,
    next_activation: Option<JsonValue>,
}

#[derive(Deserialize)]
struct Limits {
    block_bytes: u64,
    transaction_bytes: u64,
    block_cost: Cost,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
struct Cost {
    read_count: u64,
    read_length: u64,
    runtime: u64,
    write_count: u64,
    write_length: u64,
}

#[derive(Deserialize)]
struct Vm {
    clarity_version: u64,
    semantic_epoch: String,
    cost_schedule: String,
}

#[derive(Deserialize)]
struct Policies {
    unknown_activation: String,
    engine_fallback: bool,
    state_healing: bool,
}

fn repository_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

fn read_json<T: for<'de> Deserialize<'de>>(path: PathBuf) -> Result<T> {
    let contents = fs::read_to_string(&path)
        .with_context(|| format!("read compatibility input {}", path.display()))?;
    serde_json::from_str(&contents)
        .with_context(|| format!("parse compatibility input {}", path.display()))
}

fn unsigned(value: &JsonValue) -> Result<Vec<u64>> {
    value
        .as_array()
        .context("expected an array")?
        .iter()
        .map(|value| value.as_u64().context("expected an unsigned integer"))
        .collect()
}

fn expected_bools(value: &JsonValue) -> Result<Vec<bool>> {
    value
        .as_array()
        .context("expected a boolean array")?
        .iter()
        .map(|value| value.as_bool().context("expected a boolean"))
        .collect()
}

fn stock_cost() -> Cost {
    Cost {
        read_count: BLOCK_LIMIT_MAINNET_40.read_count,
        read_length: BLOCK_LIMIT_MAINNET_40.read_length,
        runtime: BLOCK_LIMIT_MAINNET_40.runtime,
        write_count: BLOCK_LIMIT_MAINNET_40.write_count,
        write_length: BLOCK_LIMIT_MAINNET_40.write_length,
    }
}

fn validate_profile(profile: &Profile) -> Result<()> {
    ensure!(profile.schema_version == 1, "unsupported profile schema");
    ensure!(profile.id == PROFILE_ID, "unexpected profile id");
    ensure!(
        profile
            .reference_revisions
            .iter()
            .any(|item| item == REVISION),
        "runner revision is absent from the profile"
    );
    ensure!(profile.domain.network == "mainnet", "wrong network domain");
    ensure!(
        profile.domain.first_supported_burn_height == BITCOIN_MAINNET_STACKS_40_BURN_HEIGHT,
        "wrong supported-domain start"
    );
    ensure!(
        profile.domain.last_supported_burn_height.is_none(),
        "unexpected finite profile end"
    );
    ensure!(
        profile.network.chain_id == u64::from(CHAIN_ID_MAINNET),
        "wrong chain id"
    );
    ensure!(
        profile.network.network_id == u64::from(NETWORK_ID_MAINNET),
        "wrong network id"
    );
    ensure!(
        profile.network.peer_version == u64::from(PEER_VERSION_MAINNET),
        "wrong peer version"
    );
    ensure!(
        profile.network.first_burn_height == BITCOIN_MAINNET_FIRST_BLOCK_HEIGHT,
        "wrong first burn height"
    );
    ensure!(
        profile.activation.semantic_epoch == "Epoch40",
        "wrong semantic epoch"
    );
    ensure!(profile.activation.peer_epoch == 0x10, "wrong peer epoch");
    ensure!(
        profile.activation.burn_height == BITCOIN_MAINNET_STACKS_40_BURN_HEIGHT,
        "wrong activation height"
    );
    ensure!(
        profile.activation.nakamoto_block_version == u64::from(NAKAMOTO_BLOCK_VERSION_EPOCH_4),
        "wrong Nakamoto block version"
    );
    ensure!(
        profile.activation.next_activation.is_none(),
        "the profile must not invent a future activation"
    );
    ensure!(
        profile.limits.block_bytes == u64::from(MAX_BLOCK_LEN),
        "wrong block byte limit"
    );
    ensure!(
        profile.limits.transaction_bytes == u64::from(MAX_TRANSACTION_LEN),
        "wrong transaction byte limit"
    );
    ensure!(
        profile.limits.block_cost == stock_cost(),
        "wrong Epoch 4 block cost"
    );
    ensure!(profile.vm.clarity_version == 6, "wrong Clarity version");
    ensure!(
        profile.vm.semantic_epoch == "Epoch40",
        "wrong VM semantic epoch"
    );
    ensure!(profile.vm.cost_schedule == "costs-5", "wrong cost schedule");
    ensure!(
        profile.policies.unknown_activation == "reject",
        "unknown activations must be rejected"
    );
    ensure!(
        !profile.policies.engine_fallback,
        "engine fallback must be disabled"
    );
    ensure!(
        !profile.policies.state_healing,
        "state healing must be disabled"
    );
    Ok(())
}

fn check_block(vector: &Vector) -> Result<&'static str> {
    let actual = unsigned(&vector.input["serialized_bytes"])?
        .into_iter()
        .map(|bytes| bytes <= u64::from(MAX_BLOCK_LEN))
        .collect::<Vec<_>>();
    ensure!(
        actual == expected_bools(&vector.expected["admitted"])?,
        "block admission mismatch"
    );
    Ok("stock MAX_BLOCK_LEN")
}

fn check_transaction(vector: &Vector) -> Result<&'static str> {
    let chains = unsigned(&vector.input["chain_ids"])?
        .into_iter()
        .map(|chain| chain == u64::from(CHAIN_ID_MAINNET))
        .collect::<Vec<_>>();
    let bytes = unsigned(&vector.input["serialized_bytes"])?
        .into_iter()
        .map(|length| length <= u64::from(MAX_TRANSACTION_LEN))
        .collect::<Vec<_>>();
    ensure!(
        chains == expected_bools(&vector.expected["chain_admitted"])?,
        "transaction chain-domain mismatch"
    );
    ensure!(
        bytes == expected_bools(&vector.expected["bytes_admitted"])?,
        "transaction byte-domain mismatch"
    );
    Ok("stock chain id and MAX_TRANSACTION_LEN")
}

fn check_sortition(vector: &Vector) -> Result<&'static str> {
    let windows = unsigned(&vector.input["burn_heights"])?
        .into_iter()
        .map(|height| {
            let top = STACKS_EPOCHS_MAINNET.epoch_id_at_height(height);
            let bottom = STACKS_EPOCHS_MAINNET
                .epoch_id_at_height(height.saturating_sub(u64::from(MINING_COMMITMENT_WINDOW) + 1));
            if top == bottom {
                u64::from(MINING_COMMITMENT_WINDOW)
            } else {
                1
            }
        })
        .collect::<Vec<_>>();
    ensure!(
        json!(windows) == vector.expected["mining_windows"],
        "sortition commitment-window mismatch"
    );
    Ok("stock epoch table and mining commitment window")
}

fn check_signer(vector: &Vector) -> Result<&'static str> {
    let weights = unsigned(&vector.input["weights"])?;
    let total = u32::try_from(weights.iter().sum::<u64>()).context("signer total")?;
    let threshold = NakamotoBlockHeader::compute_voting_weight_threshold(total)
        .context("stock signer threshold")?;
    let admitted = vector.input["signed_indexes"]
        .as_array()
        .context("signed signer subsets")?
        .iter()
        .map(|indexes| -> Result<bool> {
            let signed = indexes
                .as_array()
                .context("signer subset")?
                .iter()
                .map(|index| {
                    let index = usize::try_from(index.as_u64().context("signer index")?)
                        .context("signer index range")?;
                    weights
                        .get(index)
                        .copied()
                        .context("signer index outside weights")
                })
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .sum::<u64>();
            Ok(signed >= u64::from(threshold))
        })
        .collect::<Result<Vec<_>>>()?;
    ensure!(
        u64::from(threshold) == vector.expected["approval_threshold"],
        "signer threshold mismatch"
    );
    ensure!(
        admitted == expected_bools(&vector.expected["admitted"])?,
        "signer admission mismatch"
    );
    Ok("stock Nakamoto voting-weight threshold")
}

fn check_vm(vector: &Vector) -> Result<&'static str> {
    let epochs = unsigned(&vector.input["burn_heights"])?
        .into_iter()
        .map(|height| -> Result<String> {
            Ok(format!(
                "{:?}",
                STACKS_EPOCHS_MAINNET
                    .epoch_id_at_height(height)
                    .context("stock epoch at height")?
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    ensure!(
        json!(epochs) == vector.expected["semantic_epochs"],
        "semantic epoch mismatch"
    );
    ensure!(
        format!(
            "{:?}",
            ClarityVersion::default_for_epoch(StacksEpochId::Epoch40)
        ) == "Clarity6",
        "stock Epoch40 does not select Clarity6"
    );
    ensure!(
        vector.expected["epoch40_clarity_version"] == 6,
        "vector expects a different Clarity version"
    );
    Ok("stock epoch table and Clarity default version")
}

fn check_receipt(vector: &Vector) -> Result<&'static str> {
    let oracle: JsonValue = read_json(
        repository_root()
            .join("crates/nano-conformance/fixtures/mainnet/divergence/tx-823f-receipt.json"),
    )?;
    ensure!(
        oracle["txid"] == vector.input["txid"],
        "receipt txid mismatch"
    );
    ensure!(
        oracle["block_height"] == vector.input["block_height"],
        "receipt height mismatch"
    );
    ensure!(
        oracle["status"] == vector.expected["status"],
        "receipt status mismatch"
    );
    ensure!(
        oracle["result"]["hex"] == vector.expected["result_hex"],
        "receipt result mismatch"
    );
    ensure!(
        oracle["cost"] == vector.expected["cost"],
        "receipt cost mismatch"
    );
    ensure!(
        oracle["events"].as_array().context("receipt events")?.len() as u64
            == vector.expected["ordered_event_count"],
        "receipt event count mismatch"
    );

    let bytes = hex::decode(
        vector.expected["result_hex"]
            .as_str()
            .context("result hex")?,
    )
    .context("decode result hex")?;
    let value = Value::deserialize_read(&mut bytes.as_slice(), None, false)
        .context("stock Clarity receipt decode")?;
    ensure!(
        value
            .serialize_to_vec()
            .context("stock Clarity receipt encode")?
            == bytes,
        "stock Clarity receipt did not round-trip"
    );
    ensure!(
        value.to_string() == oracle["result"]["repr"].as_str().context("result repr")?,
        "stock Clarity receipt representation mismatch"
    );
    Ok("external receipt oracle and stock Clarity consensus codec; stateful owner task086")
}

fn check_cost(vector: &Vector) -> Result<&'static str> {
    ensure!(
        vector.input["semantic_epoch"] == "Epoch40",
        "cost vector requests another epoch"
    );
    let expected: Cost = serde_json::from_value(vector.expected.clone()).context("cost vector")?;
    ensure!(stock_cost() == expected, "stock block cost mismatch");
    Ok("stock BLOCK_LIMIT_MAINNET_40")
}

fn check_refusal(vector: &Vector) -> Result<&'static str> {
    ensure!(
        format!("{:?}", StacksEpochId::RELEASE_LATEST_EPOCH) == "Epoch40",
        "stock revision has a later release epoch"
    );
    ensure!(
        vector.input["announced_semantic_epoch"] != "Epoch40",
        "refusal does not announce an unknown epoch"
    );
    ensure!(
        vector.input["nakamoto_block_version"]
            .as_u64()
            .context("announced block version")?
            != u64::from(NAKAMOTO_BLOCK_VERSION_EPOCH_4),
        "refusal uses the admitted block version"
    );
    ensure!(
        vector.expected == json!({"decision": "reject", "fallback": false, "healing": false}),
        "refusal policy mismatch"
    );
    Ok("stock release epoch and Epoch-4 Nakamoto block version")
}

fn run_vector(vector: &Vector) -> Result<&'static str> {
    ensure!(!vector.surface.is_empty(), "{} has no surface", vector.id);
    ensure!(!vector.evidence.is_empty(), "{} has no evidence", vector.id);
    match vector.id.as_str() {
        "block.byte-limit" => check_block(vector),
        "transaction.chain-and-byte-domain" => check_transaction(vector),
        "sortition.epoch-boundary-window" => check_sortition(vector),
        "signer.weight-threshold" => check_signer(vector),
        "vm.semantic-domain" => check_vm(vector),
        "receipt.mainnet-8708126-tx4" => check_receipt(vector),
        "cost.epoch4-block-limit" => check_cost(vector),
        "refusal.unknown-activation" => check_refusal(vector),
        unknown => bail!("unowned profile vector {unknown}"),
    }
}

fn main() -> Result<()> {
    let root = repository_root();
    let profile: Profile =
        read_json(root.join("crates/nano-consensus-profile/profile/mainnet-epoch4-v1.json"))?;
    let suite: Suite = read_json(
        root.join("crates/nano-consensus-profile/profile/mainnet-epoch4-v1-vectors.json"),
    )?;
    validate_profile(&profile)?;
    ensure!(suite.schema_version == 1, "unsupported vector schema");
    ensure!(
        suite.profile == profile.id,
        "vector suite names another profile"
    );

    let mut seen = BTreeSet::new();
    let mut results = Vec::with_capacity(suite.vectors.len());
    for vector in &suite.vectors {
        ensure!(
            seen.insert(vector.id.clone()),
            "duplicate vector {}",
            vector.id
        );
        let method = run_vector(vector).with_context(|| vector.id.clone())?;
        results.push(json!({"id": vector.id, "method": method, "status": "pass"}));
    }
    ensure!(seen.len() == 8, "expected exactly eight owned vectors");

    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "profile": profile.id,
            "revision": REVISION,
            "runner": "stock-stacks-core",
            "vectors": results,
        }))?
    );
    Ok(())
}
