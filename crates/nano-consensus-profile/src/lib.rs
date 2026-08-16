//! The executable consensus domain supported by nano-stacks.

use std::{collections::BTreeSet, fmt, str::FromStr};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

pub const PROFILE_JSON: &str = include_str!("../profile/mainnet-epoch4-v1.json");
pub const VECTORS_JSON: &str = include_str!("../profile/mainnet-epoch4-v1-vectors.json");
pub const PROFILE_ID: &str = "stacks-mainnet-epoch-4.0-v1";
pub const SEMANTIC_EPOCH: &str = "Epoch40";
pub const NAKAMOTO_BLOCK_VERSION: u8 = 1;

/// Whether a Nakamoto header belongs to this profile's only supported epoch.
#[must_use]
pub const fn admits_nakamoto_block_version(version: u8) -> bool {
    version & 0x7f == NAKAMOTO_BLOCK_VERSION
}

/// Whether an explicitly announced activation is inside this profile.
#[must_use]
pub fn admits_activation(semantic_epoch: &str, block_version: u8) -> bool {
    semantic_epoch == SEMANTIC_EPOCH && admits_nakamoto_block_version(block_version)
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Fingerprint([u8; 32]);

impl Fingerprint {
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&hex::encode(self.0))
    }
}

impl FromStr for Fingerprint {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let bytes = hex::decode(value).map_err(|error| error.to_string())?;
        Ok(Self(bytes.try_into().map_err(|bytes: Vec<u8>| {
            format!("a profile fingerprint is 32 bytes, not {}", bytes.len())
        })?))
    }
}

impl Serialize for Fingerprint {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Fingerprint {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub schema_version: u32,
    pub id: String,
    pub domain: Domain,
    pub network: Network,
    pub activation: Activation,
    pub pox: Pox,
    pub limits: Limits,
    pub vm: Vm,
    pub system_contracts: Vec<SystemContract>,
    pub policies: Policies,
    pub reference_revisions: Vec<String>,
    pub evidence: Vec<Evidence>,
    pub disagreements: Vec<Disagreement>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Domain {
    pub network: String,
    pub first_supported_burn_height: u64,
    pub last_supported_burn_height: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Network {
    pub chain_id: u32,
    pub network_id: u32,
    pub peer_version: u32,
    pub bitcoin_network: String,
    pub first_burn_height: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Activation {
    pub semantic_epoch: String,
    pub peer_epoch: u8,
    pub burn_height: u64,
    pub nakamoto_block_version: u8,
    pub next_activation: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Pox {
    pub contract: String,
    pub activation_burn_height: u64,
    pub first_burn_height: u64,
    pub reward_cycle_length: u32,
    pub prepare_phase_length: u32,
    pub reward_phase_length: u32,
    pub outputs_per_commit: u8,
    pub mining_commitment_window: u8,
    pub sbtc_registry: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub block_bytes: u32,
    pub transaction_bytes: u32,
    pub block_cost: Cost,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Cost {
    pub read_count: u64,
    pub read_length: u64,
    pub runtime: u64,
    pub write_count: u64,
    pub write_length: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Vm {
    pub clarity_version: u8,
    pub semantic_epoch: String,
    pub cost_schedule: String,
    pub production_engine: String,
    pub compiler_identity: String,
    pub host_runtime_identity: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SystemContract {
    pub contract: String,
    pub role: String,
    pub source_kind: SourceKind,
    pub deployed_source_sha256: Option<String>,
    pub reference_sources: Vec<ReferenceSource>,
    pub reference_path: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourceKind {
    DeployedContract,
    DeploymentTemplate,
    GeneratedContract,
    Native,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReferenceSource {
    pub revision: String,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Policies {
    pub unknown_activation: String,
    pub engine_fallback: bool,
    pub state_healing: bool,
    pub security_upgrade_requires_full_replay: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Evidence {
    pub id: String,
    pub fields: Vec<String>,
    pub sips: Vec<String>,
    pub deployed_chain: Vec<String>,
    pub reference_paths: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Disagreement {
    pub field: String,
    pub sources: Vec<String>,
    pub resolution: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VectorCorpus {
    pub schema_version: u32,
    pub profile: String,
    pub vectors: Vec<Vector>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Vector {
    pub id: String,
    pub surface: Surface,
    pub evidence: Vec<String>,
    pub input: serde_json::Value,
    pub expected: serde_json::Value,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Surface {
    Block,
    Transaction,
    Sortition,
    Signer,
    Vm,
    Receipt,
    Cost,
    Refusal,
}

#[derive(Clone, Copy, Debug)]
pub struct RuntimeIdentities<'a> {
    pub compiler: &'a str,
    pub host_version: &'a str,
    pub host_configuration: &'a str,
}

#[derive(Serialize)]
struct ActiveProfile<'a> {
    profile: Profile,
    profile_sha256: String,
    vectors_sha256: String,
    compiler_identity: &'a str,
    host_runtime_version: &'a str,
    host_runtime_configuration: &'a str,
    fingerprint: String,
}

pub fn profile() -> Result<Profile, String> {
    serde_json::from_str(PROFILE_JSON).map_err(|error| error.to_string())
}

pub fn vectors() -> Result<VectorCorpus, String> {
    serde_json::from_str(VECTORS_JSON).map_err(|error| error.to_string())
}

#[must_use]
pub fn profile_sha256() -> Fingerprint {
    hash(PROFILE_JSON.as_bytes())
}

#[must_use]
pub fn vectors_sha256() -> Fingerprint {
    hash(VECTORS_JSON.as_bytes())
}

#[must_use]
pub fn fingerprint(identities: RuntimeIdentities<'_>) -> Fingerprint {
    let mut hasher = Sha256::new();
    hasher.update(b"nano-stacks/epoch4-profile/v1\0");
    hasher.update(PROFILE_JSON.as_bytes());
    for value in [
        identities.compiler,
        identities.host_version,
        identities.host_configuration,
    ] {
        hasher.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
        hasher.update(value.as_bytes());
    }
    Fingerprint(hasher.finalize().into())
}

pub fn active_profile_json(identities: RuntimeIdentities<'_>) -> Result<String, String> {
    validate_identities(identities)?;
    let active = ActiveProfile {
        profile: profile()?,
        profile_sha256: profile_sha256().to_string(),
        vectors_sha256: vectors_sha256().to_string(),
        compiler_identity: identities.compiler,
        host_runtime_version: identities.host_version,
        host_runtime_configuration: identities.host_configuration,
        fingerprint: fingerprint(identities).to_string(),
    };
    serde_json::to_string_pretty(&active).map_err(|error| error.to_string())
}

pub fn validate_builtin() -> Result<(), String> {
    let profile = profile()?;
    let corpus = vectors()?;
    validate_profile(&profile)?;
    validate_vectors(&profile, &corpus)
}

fn validate_profile(profile: &Profile) -> Result<(), String> {
    if profile.schema_version != 1 || profile.id != PROFILE_ID {
        return Err("unsupported Epoch-4 profile schema or identifier".to_owned());
    }
    if profile.domain.network != "mainnet"
        || profile.domain.first_supported_burn_height != 960_230
        || profile.network.chain_id != 1
        || profile.network.network_id != 0x1700_0000
        || profile.network.peer_version != 0x1800_0010
        || profile.activation.semantic_epoch != SEMANTIC_EPOCH
        || profile.activation.burn_height != 960_230
        || profile.activation.nakamoto_block_version != NAKAMOTO_BLOCK_VERSION
        || profile.pox.activation_burn_height != profile.activation.burn_height
        || profile.pox.reward_cycle_length
            != profile.pox.prepare_phase_length + profile.pox.reward_phase_length
    {
        return Err("the built-in profile contradicts its Epoch-4 mainnet domain".to_owned());
    }
    if profile.activation.next_activation.is_some()
        || profile.policies.unknown_activation != "reject"
        || profile.policies.engine_fallback
        || profile.policies.state_healing
        || !profile.policies.security_upgrade_requires_full_replay
    {
        return Err("the profile permits execution outside its fail-closed domain".to_owned());
    }
    if profile.reference_revisions.len() < 2 {
        return Err("fewer than two stock reference revisions are named".to_owned());
    }
    validate_sources(profile)?;
    let evidence = profile
        .evidence
        .iter()
        .flat_map(|item| item.fields.iter().map(String::as_str))
        .collect::<BTreeSet<_>>();
    for field in [
        "domain.*",
        "network.*",
        "activation.semantic_epoch",
        "activation.peer_epoch",
        "activation.burn_height",
        "activation.nakamoto_block_version",
        "activation.next_activation",
        "pox.contract",
        "pox.activation_burn_height",
        "pox.first_burn_height",
        "pox.reward_cycle_length",
        "pox.prepare_phase_length",
        "pox.reward_phase_length",
        "pox.outputs_per_commit",
        "pox.mining_commitment_window",
        "pox.sbtc_registry",
        "limits.*",
        "vm.*",
        "system_contracts.*",
        "policies.*",
        "reference_revisions.*",
    ] {
        if !evidence.contains(field) {
            return Err(format!("profile field {field} has no evidence binding"));
        }
    }
    for disagreement in &profile.disagreements {
        if disagreement.field.is_empty()
            || disagreement.sources.len() < 2
            || disagreement.resolution.is_empty()
        {
            return Err("a source disagreement is not explicit".to_owned());
        }
    }
    Ok(())
}

fn validate_sources(profile: &Profile) -> Result<(), String> {
    let revisions = profile
        .reference_revisions
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    for contract in &profile.system_contracts {
        let references = contract
            .reference_sources
            .iter()
            .map(|source| source.revision.as_str())
            .collect::<BTreeSet<_>>();
        if references != revisions {
            return Err(format!(
                "{} does not name every reference revision",
                contract.contract
            ));
        }
        if matches!(
            contract.source_kind,
            SourceKind::DeployedContract | SourceKind::GeneratedContract
        ) != contract.deployed_source_sha256.is_some()
        {
            return Err(format!(
                "{} has an inconsistent deployed-source identity",
                contract.contract
            ));
        }
        for (owner, digest) in contract
            .deployed_source_sha256
            .iter()
            .map(|digest| ("deployed", digest))
            .chain(
                contract
                    .reference_sources
                    .iter()
                    .map(|source| (source.revision.as_str(), &source.sha256)),
            )
        {
            validate_sha256(digest)
                .map_err(|error| format!("{} {owner} source: {error}", contract.contract))?;
        }
    }
    Ok(())
}

fn validate_vectors(profile: &Profile, corpus: &VectorCorpus) -> Result<(), String> {
    if corpus.schema_version != 1 || corpus.profile != profile.id {
        return Err("the vector corpus does not name this profile".to_owned());
    }
    let evidence = profile
        .evidence
        .iter()
        .map(|item| item.id.as_str())
        .collect::<BTreeSet<_>>();
    let mut identifiers = BTreeSet::new();
    let mut surfaces = BTreeSet::new();
    for vector in &corpus.vectors {
        if !identifiers.insert(vector.id.as_str()) {
            return Err(format!("duplicate vector {}", vector.id));
        }
        surfaces.insert(vector.surface);
        if vector.evidence.is_empty()
            || vector
                .evidence
                .iter()
                .any(|item| !evidence.contains(item.as_str()))
        {
            return Err(format!("vector {} has no valid evidence owner", vector.id));
        }
    }
    let required = BTreeSet::from([
        Surface::Block,
        Surface::Transaction,
        Surface::Sortition,
        Surface::Signer,
        Surface::Vm,
        Surface::Receipt,
        Surface::Cost,
        Surface::Refusal,
    ]);
    if surfaces != required {
        return Err("the vector corpus does not cover every mandatory surface".to_owned());
    }
    Ok(())
}

fn validate_identities(identities: RuntimeIdentities<'_>) -> Result<(), String> {
    if identities.compiler.is_empty()
        || identities.host_version.is_empty()
        || identities.host_configuration.is_empty()
    {
        return Err("an active profile requires exact compiler and host identities".to_owned());
    }
    Ok(())
}

fn validate_sha256(digest: &str) -> Result<(), String> {
    let bytes = hex::decode(digest).map_err(|error| error.to_string())?;
    if bytes.len() != 32 {
        return Err(format!("expected 32 bytes, got {}", bytes.len()));
    }
    Ok(())
}

fn hash(bytes: &[u8]) -> Fingerprint {
    Fingerprint(Sha256::digest(bytes).into())
}

#[cfg(test)]
mod tests {
    use super::{
        Fingerprint, PROFILE_ID, RuntimeIdentities, active_profile_json, fingerprint, profile,
        validate_builtin,
    };
    use std::str::FromStr as _;

    const IDENTITIES: RuntimeIdentities<'_> = RuntimeIdentities {
        compiler: "sha256:compiler",
        host_version: "36.0.3",
        host_configuration: "fuel=off;epoch=off",
    };

    #[test]
    fn the_builtin_profile_and_every_mandatory_surface_are_owned() {
        validate_builtin().expect("the checked-in profile is complete");
        assert_eq!(profile().expect("profile").id, PROFILE_ID);
    }

    #[test]
    fn the_active_fingerprint_changes_with_every_runtime_identity() {
        let original = fingerprint(IDENTITIES);
        for changed in [
            RuntimeIdentities {
                compiler: "sha256:other",
                ..IDENTITIES
            },
            RuntimeIdentities {
                host_version: "37",
                ..IDENTITIES
            },
            RuntimeIdentities {
                host_configuration: "fuel=on",
                ..IDENTITIES
            },
        ] {
            assert_ne!(fingerprint(changed), original);
        }
        assert_eq!(
            Fingerprint::from_str(&original.to_string()).expect("round trip"),
            original
        );
    }

    #[test]
    fn the_active_document_names_the_exact_fingerprint() {
        let document = active_profile_json(IDENTITIES).expect("active profile");
        let json: serde_json::Value = serde_json::from_str(&document).expect("JSON");
        assert_eq!(json["fingerprint"], fingerprint(IDENTITIES).to_string());
        assert_eq!(json["profile"]["id"], PROFILE_ID);
    }

    #[test]
    fn deployed_sources_are_not_relabelled_as_reference_templates() {
        let profile = profile().expect("profile");
        let pox = profile
            .system_contracts
            .iter()
            .find(|contract| contract.role == "pox")
            .expect("pox-5 identity");
        assert_eq!(
            pox.deployed_source_sha256.as_deref(),
            Some("ffad35ad181d85832ebd7b998f445204c92d5cd19549166e644fb1f3988fa385")
        );
        assert_ne!(
            pox.reference_sources[0].sha256,
            pox.reference_sources[1].sha256
        );
        assert!(
            profile
                .disagreements
                .iter()
                .any(|item| item.field.contains("pox-5"))
        );
    }
}
