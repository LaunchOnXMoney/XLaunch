//! V0 envelope construction, measured wire limits, and separate signing.
use crate::{chain_instruction, pump_key};
use anyhow::{Result, ensure};
use helius::optimized_transaction::{
    MAX_COMPUTE_UNIT_LIMIT, MIN_TIP_LAMPORTS_MAX, MIN_TIP_LAMPORTS_SWQOS,
};
use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_sdk::{
    hash::Hash,
    instruction::Instruction,
    message::{AddressLookupTableAccount, VersionedMessage, v0},
    pubkey::Pubkey,
    signature::{Signature, Signer},
    transaction::VersionedTransaction,
};

// Verified against Solana transaction documentation and SDK wire serialization.
pub const MAX_WIRE_BYTES: usize = 1232;
pub const MAX_ACCOUNTS: usize = 64;
/// Settlement defaults: ceil(500_000 * 2_800 / 1_000_000) = 1_400 lamports.
pub const SETTLEMENT_COMPUTE_UNIT_LIMIT: u32 = 500_000;
pub const SETTLEMENT_COMPUTE_UNIT_PRICE_MICRO_LAMPORTS: u64 = 2_800;
pub const TIP_ACCOUNT: Pubkey =
    Pubkey::from_str_const("4ACfpUFoaSD9bfPdeu6DBt89gB6ENTeHBXCAi87NhDEE");

#[derive(Clone, Copy, Debug)]
pub enum SenderTier {
    Max,
    Swqos,
}

impl SenderTier {
    pub fn minimum_tip(self) -> u64 {
        match self {
            Self::Max => MIN_TIP_LAMPORTS_MAX,
            Self::Swqos => MIN_TIP_LAMPORTS_SWQOS,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Envelope {
    pub payer: Pubkey,
    pub blockhash: Hash,
    pub last_valid_block_height: u64,
    pub lookup_tables: Vec<AddressLookupTableAccount>,
    pub compute_unit_limit: u32,
    /// Micro-lamports per CU, not lamports or a total transaction fee.
    pub compute_unit_price: u64,
    pub tip_lamports: u64,
    pub tier: SenderTier,
}

impl Envelope {
    pub fn instructions(&self, body: &[Instruction]) -> Result<Vec<Instruction>> {
        ensure!(
            (1..=MAX_COMPUTE_UNIT_LIMIT).contains(&self.compute_unit_limit),
            "invalid compute limit"
        );
        ensure!(
            self.compute_unit_price > 0 && self.tip_lamports >= self.tier.minimum_tip(),
            "Sender requires a priority fee and sufficient tip"
        );
        ensure!(
            self.payer != Pubkey::default() && self.last_valid_block_height > 0,
            "invalid payer/expiry"
        );
        let mut instructions = vec![
            chain_instruction(ComputeBudgetInstruction::set_compute_unit_limit(
                self.compute_unit_limit,
            )),
            chain_instruction(ComputeBudgetInstruction::set_compute_unit_price(
                self.compute_unit_price,
            )),
        ];
        instructions.extend_from_slice(body);
        instructions.push(chain_instruction(
            solana_system_interface::instruction::transfer(
                &pump_key(self.payer),
                &pump_key(TIP_ACCOUNT),
                self.tip_lamports,
            ),
        ));
        Ok(instructions)
    }

    /// Placeholder signatures have exactly the wire size of real signatures.
    /// Returned transaction is for inspection/simulation, and cannot be submitted.
    pub fn compile(&self, body: &[Instruction]) -> Result<VersionedTransaction> {
        let instructions = self.instructions(body)?;
        let message = v0::Message::try_compile(
            &self.payer,
            &instructions,
            &self.lookup_tables,
            self.blockhash,
        )?;
        let required = message.header.num_required_signatures;
        let transaction = VersionedTransaction {
            signatures: vec![Signature::default(); required.into()],
            message: VersionedMessage::V0(message),
        };
        Ok(transaction)
    }

    pub fn sign(&self, body: &[Instruction], signers: &[&dyn Signer]) -> Result<SignedTransaction> {
        let unsigned = self.compile(body)?;
        ensure!(
            fits(&unsigned)?,
            "transaction exceeds V0 limits; supply verified active lookup tables or reduce the batch"
        );
        let transaction = VersionedTransaction::try_new(unsigned.message, signers)?;
        transaction.verify_and_hash_message()?;
        Ok(SignedTransaction {
            transaction,
            last_valid_block_height: self.last_valid_block_height,
            tier: self.tier,
        })
    }
}

pub struct SignedTransaction {
    pub(crate) transaction: VersionedTransaction,
    pub(crate) last_valid_block_height: u64,
    pub(crate) tier: SenderTier,
}

impl SignedTransaction {
    pub fn transaction(&self) -> &VersionedTransaction {
        &self.transaction
    }
    pub fn signature(&self) -> Signature {
        self.transaction.signatures[0]
    }
    pub fn wire(&self) -> Result<Vec<u8>> {
        Ok(bincode::serialize(&self.transaction)?)
    }
}

pub fn wire_size(tx: &VersionedTransaction) -> Result<usize> {
    Ok(bincode::serialize(tx)?.len())
}

pub fn fits(tx: &VersionedTransaction) -> Result<bool> {
    let VersionedMessage::V0(message) = &tx.message else {
        anyhow::bail!("only V0 supported");
    };
    let accounts = message.account_keys.len()
        + message
            .address_table_lookups
            .iter()
            .map(|l| l.writable_indexes.len() + l.readonly_indexes.len())
            .sum::<usize>();
    Ok(wire_size(tx)? <= MAX_WIRE_BYTES
        && accounts <= MAX_ACCOUNTS
        && message.instructions.len() <= 64)
}

/// Fetch an existing table from a processed bank. Reject deactivated tables and
/// entries still in their extension slot. No hard-coded address list is trusted.
pub fn fetch_lookup_table(
    helius: &helius::Helius,
    address: Pubkey,
) -> Result<AddressLookupTableAccount> {
    use anyhow::Context;
    use solana_address_lookup_table_interface::{program, state::AddressLookupTable};
    use solana_commitment_config::CommitmentConfig;
    let response = helius
        .rpc_client
        .solana_client
        .get_multiple_accounts_with_commitment(&[address], CommitmentConfig::processed())?;
    let account = response
        .value
        .first()
        .and_then(Option::as_ref)
        .context("lookup table missing")?;
    ensure!(
        account.owner.to_bytes() == program::ID.to_bytes() && !account.executable,
        "lookup table owner/type mismatch"
    );
    let table = AddressLookupTable::deserialize(&account.data)?;
    ensure!(
        table.meta.deactivation_slot == u64::MAX
            && table.meta.last_extended_slot < response.context.slot,
        "table deactivated or not yet active"
    );
    Ok(AddressLookupTableAccount {
        key: address,
        addresses: table
            .addresses
            .iter()
            .map(|p| Pubkey::new_from_array(p.to_bytes()))
            .collect(),
    })
}
