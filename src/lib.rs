//! Ordered presale accounting and independently reviewable Solana settlement.

pub mod allocation;
pub mod distribution;
pub mod launch;
pub mod presale;
pub mod receipt;
pub mod scheduler;
pub mod sender;
pub mod settlement;
pub mod state;
pub mod transaction;
pub mod web;

// Pump/Anchor and Helius currently use different generations of Solana types.
// Conversion is explicit and preserves every instruction byte and account flag.
pub(crate) fn pump_key(key: solana_sdk::pubkey::Pubkey) -> anchor_lang::prelude::Pubkey {
    anchor_lang::prelude::Pubkey::new_from_array(key.to_bytes())
}

pub(crate) fn chain_key(key: anchor_lang::prelude::Pubkey) -> solana_sdk::pubkey::Pubkey {
    solana_sdk::pubkey::Pubkey::new_from_array(key.to_bytes())
}

pub(crate) fn chain_instruction(
    ix: anchor_lang::solana_program::instruction::Instruction,
) -> solana_sdk::instruction::Instruction {
    solana_sdk::instruction::Instruction {
        program_id: chain_key(ix.program_id),
        accounts: ix
            .accounts
            .into_iter()
            .map(|a| solana_sdk::instruction::AccountMeta {
                pubkey: chain_key(a.pubkey),
                is_signer: a.is_signer,
                is_writable: a.is_writable,
            })
            .collect(),
        data: ix.data,
    }
}
