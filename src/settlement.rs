//! Shared SOL create/buy/distribution sequence. Signing and delivery stay isolated.
//! Production delivery must use sender::submit_once (official Helius Sender) and
//! a processed transaction-metadata feed. The loopback fork supplies a test adapter.
use crate::{
    allocation::{self, Allocation, Payout},
    distribution::Distribution,
    launch,
    presale::{Raise, Settlement, Trigger},
    receipt::{self, LaunchReceipt},
    sender::RpcSimulator,
    state::Config,
    transaction::{
        Envelope, SETTLEMENT_COMPUTE_UNIT_LIMIT, SETTLEMENT_COMPUTE_UNIT_PRICE_MICRO_LAMPORTS,
        SignedTransaction,
    },
};
use anyhow::{Context, Result, ensure};
use helius::Helius;
use solana_commitment_config::CommitmentConfig;
use solana_sdk::signature::Signer;
use solana_transaction_status_client_types::EncodedConfirmedTransactionWithStatusMeta;

pub struct SolInputs<'a> {
    pub helius: &'a Helius,
    pub buyer: &'a dyn Signer,
    pub mint: &'a dyn Signer,
    /// Supplies lookup tables and tip; must use the settlement compute/fee policy.
    /// The blockhash is refreshed for each distinct transaction, never per region.
    pub envelope: Envelope,
    /// Already converted/funded lamports attributable to accepted contributions.
    pub funded_lamports: u64,
}
#[derive(Debug, serde::Serialize)]
pub struct BatchReceipt {
    pub recipients: usize,
    pub wire_bytes: usize,
    pub units: u64,
}
#[derive(Debug, serde::Serialize)]
pub struct Report {
    pub settlement: Settlement,
    pub launch: LaunchReceipt,
    pub curve_quote_lamports: u64,
    pub allocations: Vec<Allocation>,
    pub payouts: Vec<Payout>,
    pub batches: Vec<BatchReceipt>,
}

/// A delivery error leaves the journal and queue claim for reconciliation. Never
/// re-sign here. The adapter must durably journal before broadcast, and return
/// metadata for the exact submitted bytes; all receipts are checked below.
pub fn execute(
    context: SolInputs<'_>,
    raise: &Raise,
    trigger: Trigger,
    mut deliver: impl FnMut(
        &str,
        &SignedTransaction,
        u64,
    ) -> Result<EncodedConfirmedTransactionWithStatusMeta>,
) -> Result<Report> {
    ensure!(
        context.mint.pubkey() == raise.mint && context.envelope.payer == context.buyer.pubkey(),
        "settlement signer/payer mismatch"
    );
    ensure!(
        context.envelope.compute_unit_limit == SETTLEMENT_COMPUTE_UNIT_LIMIT
            && context.envelope.compute_unit_price == SETTLEMENT_COMPUTE_UNIT_PRICE_MICRO_LAMPORTS,
        "settlement requires 500,000 CU and 2,800 micro-lamports per CU"
    );
    let rpc = &context.helius.rpc_client.solana_client;
    let config = Config::fetch(context.helius)?;
    let (launch, plan) = launch::for_raise(
        &config,
        raise,
        trigger,
        context.buyer.pubkey(),
        context.funded_lamports,
    )?;
    let next_envelope = || -> Result<Envelope> {
        let mut e = context.envelope.clone();
        (e.blockhash, e.last_valid_block_height) =
            rpc.get_latest_blockhash_with_commitment(CommitmentConfig::processed())?;
        Ok(e)
    };
    let signed = next_envelope()?.sign(&plan.instructions, &[context.buyer, context.mint])?;
    let response = deliver(&format!("{}.sol-launch", raise.mint), &signed, config.slot)?;
    let bought = receipt::verify_launch_receipt(&response, &signed, &launch, &plan)?;
    let allocations = raise.allocations()?;
    let payouts = allocation::distribute(&allocations, bought.tokens_received)?;
    let account = rpc.get_account_with_commitment(&raise.mint, CommitmentConfig::processed())?;
    ensure!(
        account.context.slot >= bought.slot,
        "mint read precedes launch receipt"
    );
    let distribution = Distribution::new(
        raise.mint,
        &account.value.context("created mint missing")?,
        context.buyer.pubkey(),
        context.buyer.pubkey(),
        payouts.clone(),
    )?;
    let (mut cursor, mut min_slot, mut distributed) = (0, bought.slot, 0u64);
    let mut signatures = vec![];
    let mut batches = vec![];
    while cursor < distribution.len() {
        let envelope = next_envelope()?;
        let simulator = RpcSimulator {
            helius: context.helius,
            min_context_slot: min_slot,
        };
        let batch = distribution.next_batch(cursor, &envelope, &simulator)?;
        let signed = envelope.sign(&batch.instructions, &[context.buyer])?;
        let response = deliver(
            &format!("{}.sol-payout-{cursor}", raise.mint),
            &signed,
            min_slot,
        )?;
        let paid = receipt::verify_payout_receipt(
            &response,
            &signed,
            raise.mint,
            context.buyer.pubkey(),
            &batch.payouts,
        )?;
        distributed = distributed
            .checked_add(paid.delivered_tokens)
            .context("distribution total overflow")?;
        signatures.push(paid.signature);
        min_slot = paid.slot;
        batches.push(BatchReceipt {
            recipients: batch.end - cursor,
            wire_bytes: signed.wire()?.len(),
            units: batch.units_consumed,
        });
        cursor = batch.end;
    }
    ensure!(
        distributed == bought.tokens_received,
        "distribution incomplete"
    );
    Ok(Report {
        settlement: Settlement {
            launch_signature: bought.signature.clone(),
            payout_signatures: signatures,
            tokens_received: bought.tokens_received,
            tokens_distributed: distributed,
        },
        launch: bought,
        curve_quote_lamports: plan.quote.curve_quote,
        allocations,
        payouts,
        batches,
    })
}
