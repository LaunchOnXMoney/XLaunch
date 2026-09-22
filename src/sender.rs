//! The only broadcast entry point. Transport is the official Helius Sender SDK.
use crate::{
    distribution::{Simulation, Simulator},
    transaction::{SenderTier, SignedTransaction},
};
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::future::join_all;
use helius::{Helius, types::SenderSendOptions};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use solana_commitment_config::CommitmentConfig;
use solana_rpc_client_api::config::RpcSimulateTransactionConfig;
use solana_sdk::{
    instruction::InstructionError,
    signature::Signature,
    transaction::{TransactionError, VersionedTransaction},
};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

pub struct RpcSimulator<'a> {
    pub helius: &'a Helius,
    pub min_context_slot: u64,
}

impl Simulator for RpcSimulator<'_> {
    fn simulate(&self, transaction: &VersionedTransaction) -> Result<Simulation> {
        classify_simulation(self.simulate_raw(transaction)?.value)
    }
}

impl RpcSimulator<'_> {
    /// Unsigned planning simulation, with the RPC node's recent blockhash.
    /// Final submission separately simulates the exact signed bytes without replacement.
    pub fn simulate_raw(
        &self,
        transaction: &VersionedTransaction,
    ) -> Result<
        solana_rpc_client_api::response::Response<
            solana_rpc_client_api::response::RpcSimulateTransactionResult,
        >,
    > {
        self.simulate_raw_with_accounts(transaction, &[])
    }

    pub fn simulate_raw_with_accounts(
        &self,
        transaction: &VersionedTransaction,
        addresses: &[solana_sdk::pubkey::Pubkey],
    ) -> Result<
        solana_rpc_client_api::response::Response<
            solana_rpc_client_api::response::RpcSimulateTransactionResult,
        >,
    > {
        let response = self
            .helius
            .rpc_client
            .solana_client
            .simulate_transaction_with_config(
                transaction,
                RpcSimulateTransactionConfig {
                    sig_verify: false,
                    replace_recent_blockhash: true,
                    commitment: Some(CommitmentConfig::processed()),
                    min_context_slot: Some(self.min_context_slot),
                    accounts: if addresses.is_empty() {
                        None
                    } else {
                        Some(
                            solana_rpc_client_api::config::RpcSimulateTransactionAccountsConfig {
                                encoding: Some(
                                    solana_rpc_client_api::response::UiAccountEncoding::Base64,
                                ),
                                addresses: addresses.iter().map(ToString::to_string).collect(),
                            },
                        )
                    },
                    ..Default::default()
                },
            )?;
        Ok(response)
    }
}

fn classify_simulation(
    value: solana_rpc_client_api::response::RpcSimulateTransactionResult,
) -> Result<Simulation> {
    if let Some(error) = value.err {
        let error: TransactionError = error.into();
        let detail = format!("{error:?}; logs={:?}", value.logs);
        return match error {
            TransactionError::TooManyAccountLocks
            | TransactionError::InstructionError(
                _,
                InstructionError::ComputationalBudgetExceeded
                | InstructionError::MaxInstructionTraceLengthExceeded,
            ) => Ok(Simulation::ResourceLimit { detail }),
            _ => bail!("simulation failed: {detail}"),
        };
    }
    Ok(Simulation::Success {
        units_consumed: value
            .units_consumed
            .context("simulation omitted consumed units")?,
    })
}

#[derive(Debug, Serialize, Deserialize)]
pub struct JournalRecord {
    pub intent: String,
    pub signature: String,
    pub wire_base64: String,
    pub wire_sha256: String,
    pub last_valid_block_height: u64,
}

/// Local durable journal, for a single shared filesystem. An existing intent
/// always blocks a new attempt, including after crashes, failures, or timeouts.
/// Production DB workers must also uniquely reserve (mint, phase, batch index).
pub struct Journal {
    root: PathBuf,
}

impl Journal {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    pub fn reserve(&self, intent: &str, signed: &SignedTransaction) -> Result<PathBuf> {
        ensure!(!intent.is_empty(), "empty settlement intent");
        let dir = self
            .root
            .join(format!("{:x}", Sha256::digest(intent.as_bytes())));
        fs::create_dir(&dir).context("intent already reserved or journal unavailable; reconcile it, do not create a new payment")?;
        File::open(&self.root)?.sync_all()?;
        let wire = signed.wire()?;
        let record = JournalRecord {
            intent: intent.into(),
            signature: signed.signature().to_string(),
            wire_base64: STANDARD.encode(&wire),
            wire_sha256: format!("{:x}", Sha256::digest(&wire)),
            last_valid_block_height: signed.last_valid_block_height,
        };
        write_new(
            &dir.join("signed.json"),
            &serde_json::to_vec_pretty(&record)?,
        )?;
        File::open(&dir)?.sync_all()?;
        Ok(dir)
    }
}

fn write_new(path: &Path, contents: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(contents)?;
    file.sync_all()?;
    Ok(())
}

/// All physical regions from the pinned SDK. `Default` is an automatic router,
/// not another location. Keep endpoint selection in the official SDK.
pub fn sender_regions() -> Vec<&'static str> {
    let mut regions: Vec<_> = helius::optimized_transaction::SENDER_ENDPOINTS
        .keys()
        .copied()
        .filter(|region| *region != "Default")
        .collect();
    regions.sort_unstable();
    regions
}

#[derive(Debug, Serialize)]
struct RegionOutcome {
    region: &'static str,
    confirmed: bool,
    error: Option<String>,
}

/// Simulate once, durably record once, then send identical signed bytes to every
/// Sender region concurrently. Each region has a bounded 30-second deadline;
/// drain every attempt so a fast result cannot cancel another region's send.
/// The pinned SDK internally waits for confirmed; it has no processed option.
/// Our RPC simulation uses processed. Verify transaction-local receipts before
/// allocating or sending payouts. On any error, reconcile the recorded signature.
/// There is no per-region rebuild/re-sign, fallback transport or automatic retry.
pub async fn submit_once(
    helius: &Helius,
    journal: &Journal,
    intent: &str,
    signed: &SignedTransaction,
    min_context_slot: u64,
) -> Result<Signature> {
    let regions = sender_regions();
    ensure!(!regions.is_empty(), "SDK has no Sender regions");
    signed.transaction.verify_and_hash_message()?;
    // Sender confirmation uses this blocking client's own runtime. Keep
    // simulation there too: using get_inner_client() on a different runtime can
    // strand pooled HTTP connections after a preceding regional confirmation.
    let rpc = helius.rpc_client.solana_client.clone();
    let transaction = signed.transaction.clone();
    let simulation = tokio::task::spawn_blocking(move || {
        rpc.simulate_transaction_with_config(
            &transaction,
            RpcSimulateTransactionConfig {
                sig_verify: true,
                replace_recent_blockhash: false,
                commitment: Some(CommitmentConfig::processed()),
                min_context_slot: Some(min_context_slot),
                ..Default::default()
            },
        )
    })
    .await
    .context("signed simulation worker failed")??;
    match classify_simulation(simulation.value)? {
        Simulation::Success { .. } => {}
        Simulation::ResourceLimit { detail } => {
            bail!("signed transaction exceeds runtime limits: {detail}")
        }
    }
    let path = journal.reserve(intent, signed)?;
    let signature = signed.signature();
    let outcomes = join_all(regions.into_iter().map(|region| async move {
        let options = SenderSendOptions::new()
            .with_region(region)
            .with_swqos_only(matches!(signed.tier, SenderTier::Swqos))
            // Exact signed bytes were already simulated at processed above.
            .with_skip_preflight(true)
            .with_poll_timeout_ms(25_000);
        let result = tokio::time::timeout(
            Duration::from_secs(30),
            helius.send_and_confirm_via_sender(
                &signed.transaction,
                signed.last_valid_block_height,
                options,
            ),
        )
        .await;
        let error = match result {
            Ok(Ok(returned)) if returned == signature => None,
            Ok(Ok(_)) => Some("Sender returned a different signature".into()),
            Ok(Err(error)) => Some(error.to_string()),
            Err(_) => Some("regional submission/confirmation deadline elapsed".into()),
        };
        RegionOutcome {
            region,
            confirmed: error.is_none(),
            error,
        }
    }))
    .await;
    write_new(
        &path.join("fanout.json"),
        &serde_json::to_vec_pretty(&serde_json::json!({
            "signature":signature.to_string(),
            "wire_sha256":format!("{:x}", Sha256::digest(signed.wire()?)),
            "regions":outcomes,
        }))?,
    )?;
    File::open(&path)?.sync_all()?;
    ensure!(
        outcomes.iter().any(|outcome| outcome.confirmed),
        "Sender outcome unresolved in every region; reconcile {} using {} before any new attempt",
        signature,
        path.display()
    );
    write_new(
        &path.join("confirmed.txt"),
        signature.to_string().as_bytes(),
    )?;
    File::open(&path)?.sync_all()?;
    Ok(signature)
}
