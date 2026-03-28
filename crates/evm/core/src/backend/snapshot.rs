use super::{FoundryEvmInMemoryDB, JournaledState};
use alloy_evm::EvmEnv;
use alloy_genesis::GenesisAccount;
use alloy_primitives::{
    Address, B256, Bytes, U256,
    map::{AddressHashMap, HashMap},
};
use eyre::ensure;
use revm::{
    context::{BlockEnv, CfgEnv},
    database::DatabaseRef,
};
use revm::state::AccountInfo;
use serde::{Deserialize, Deserializer, Serialize, de::MapAccess};
use serde_json::Value;
use std::{collections::BTreeMap, fmt};

/// A minimal abstraction of a state at a certain point in time
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StateSnapshot {
    pub accounts: AddressHashMap<AccountInfo>,
    pub storage: AddressHashMap<HashMap<U256, U256>>,
    pub block_hashes: HashMap<U256, B256>,
}

/// Represents a state snapshot taken during evm execution
#[derive(Clone, Debug)]
pub struct BackendStateSnapshot<T> {
    pub db: T,
    /// The journaled_state state at a specific point
    pub journaled_state: JournaledState,
    /// Contains the evm env at the time of the snapshot
    pub snap_evm_env: EvmEnv,
}

impl<T> BackendStateSnapshot<T> {
    /// Takes a new state snapshot.
    pub fn new(db: T, journaled_state: JournaledState, evm_env: EvmEnv) -> Self {
        Self { db, journaled_state, snap_evm_env: evm_env }
    }

    /// Called when this state snapshot is reverted.
    ///
    /// Since we want to keep all additional logs that were emitted since the snapshot was taken
    /// we'll merge additional logs into the snapshot's `revm::JournaledState`. Additional logs are
    /// those logs that are missing in the snapshot's journaled_state, since the current
    /// journaled_state includes the same logs, we can simply replace use that See also
    /// `DatabaseExt::revert`.
    pub fn merge(&mut self, current: &JournaledState) {
        self.journaled_state.logs.clone_from(&current.logs);
    }
}

pub const PERSISTED_STATE_SNAPSHOT_VERSION: u64 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompatibleStateSnapshot {
    #[serde(default)]
    pub block: Option<BlockEnv>,
    pub accounts: BTreeMap<Address, SerializableAccountRecord>,
    #[serde(default, deserialize_with = "deserialize_best_block_number_compat")]
    pub best_block_number: Option<u64>,
    #[serde(default)]
    pub blocks: Vec<Value>,
    #[serde(default)]
    pub transactions: Vec<Value>,
    #[serde(default)]
    pub historical_states: Option<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SerializableAccountRecord {
    pub nonce: u64,
    pub balance: U256,
    pub code: Bytes,
    #[serde(deserialize_with = "deserialize_storage_btree")]
    pub storage: BTreeMap<B256, B256>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FoundrySnapshotMetadata {
    pub version: u64,
    pub db: FoundryEvmInMemoryDB,
    pub journaled_state: JournaledState,
    pub cfg_env: CfgEnv,
}

/// Represents a state snapshot that can be persisted to disk.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PersistedStateSnapshot {
    #[serde(flatten)]
    pub state: CompatibleStateSnapshot,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foundry_snapshot: Option<FoundrySnapshotMetadata>,
}

impl PersistedStateSnapshot {
    pub fn from_snapshot(snapshot: BackendStateSnapshot<FoundryEvmInMemoryDB>) -> eyre::Result<Self> {
        let BackendStateSnapshot { db, journaled_state, snap_evm_env } = snapshot;
        let EvmEnv { cfg_env, block_env } = snap_evm_env;

        let mut compat_db = db.clone();
        for (address, account) in &journaled_state.state {
            compat_db.insert_account_info(*address, account.info.clone());
            for (slot, value) in &account.storage {
                compat_db.insert_account_storage(*address, *slot, value.present_value)?;
            }
        }

        let accounts = compat_db
            .cache
            .accounts
            .clone()
            .into_iter()
            .map(|(address, account)| -> eyre::Result<_> {
                let code = if let Some(code) = account.info.code {
                    code
                } else {
                    compat_db.code_by_hash_ref(account.info.code_hash)?
                };
                Ok((
                    address,
                    SerializableAccountRecord {
                        nonce: account.info.nonce,
                        balance: account.info.balance,
                        code: code.original_bytes(),
                        storage: account.storage.into_iter().map(|(k, v)| (k.into(), v.into())).collect(),
                    },
                ))
            })
            .collect::<eyre::Result<_>>()?;

        let _ = cfg_env;
        Ok(Self {
            state: CompatibleStateSnapshot {
                block: Some(block_env.clone()),
                accounts,
                best_block_number: Some(block_env.number.saturating_to()),
                blocks: Vec::new(),
                transactions: Vec::new(),
                historical_states: None,
            },
            foundry_snapshot: None,
        })
    }

    pub fn into_snapshot(self) -> eyre::Result<Option<BackendStateSnapshot<FoundryEvmInMemoryDB>>> {
        let Some(foundry_snapshot) = self.foundry_snapshot else { return Ok(None) };
        ensure!(
            foundry_snapshot.version == PERSISTED_STATE_SNAPSHOT_VERSION,
            "unsupported persisted state snapshot version: {}",
            foundry_snapshot.version
        );
        Ok(Some(BackendStateSnapshot::new(
            foundry_snapshot.db,
            foundry_snapshot.journaled_state,
            EvmEnv {
                cfg_env: foundry_snapshot.cfg_env,
                block_env: self.state.block.unwrap_or_default(),
            },
        )))
    }

    pub fn into_allocs(self) -> BTreeMap<Address, GenesisAccount> {
        self.state
            .accounts
            .into_iter()
            .map(|(address, account)| {
                (
                    address,
                    GenesisAccount {
                        nonce: Some(account.nonce),
                        balance: account.balance,
                        code: Some(account.code),
                        storage: Some(
                            account.storage.into_iter().map(|(slot, value)| (slot, value)).collect(),
                        ),
                        ..Default::default()
                    },
                )
            })
            .collect()
    }

    pub fn block_env(&self) -> Option<BlockEnv> {
        self.state.block.clone()
    }
}

fn deserialize_best_block_number_compat<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    let value: Option<Value> = Option::deserialize(deserializer)?;
    let Some(value) = value else {
        return Ok(None);
    };

    let number = match value {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => {
            if let Some(s) = s.strip_prefix("0x") { u64::from_str_radix(s, 16).ok() } else { s.parse().ok() }
        }
        _ => None,
    };

    Ok(number)
}

fn deserialize_storage_btree<'de, D>(deserializer: D) -> Result<BTreeMap<B256, B256>, D::Error>
where
    D: Deserializer<'de>,
{
    struct BTreeVisitor;

    impl<'de> serde::de::Visitor<'de> for BTreeVisitor {
        type Value = BTreeMap<B256, B256>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a mapping of hex encoded storage slots to hex encoded state data")
        }

        fn visit_map<M>(self, mut mapping: M) -> Result<BTreeMap<B256, B256>, M::Error>
        where
            M: MapAccess<'de>,
        {
            let mut btree = BTreeMap::new();
            while let Some((key, value)) = mapping.next_entry::<U256, U256>()? {
                btree.insert(B256::from(key), B256::from(value));
            }
            Ok(btree)
        }
    }

    deserializer.deserialize_map(BTreeVisitor)
}

/// What to do when reverting a state snapshot.
///
/// Whether to remove the state snapshot or keep it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RevertStateSnapshotAction {
    /// Remove the state snapshot after reverting.
    #[default]
    RevertRemove,
    /// Keep the state snapshot after reverting.
    RevertKeep,
}

impl RevertStateSnapshotAction {
    /// Returns `true` if the action is to keep the state snapshot.
    pub fn is_keep(&self) -> bool {
        matches!(self, Self::RevertKeep)
    }
}
