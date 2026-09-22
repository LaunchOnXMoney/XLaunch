use anchor_lang::solana_program::program_pack::Pack;
use anchor_spl::token_2022::spl_token_2022::state::Mint;
use anyhow::Result;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use solana_sdk::{
    account::Account,
    hash::Hash,
    instruction::Instruction,
    message::{AddressLookupTableAccount, VersionedMessage},
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    transaction::VersionedTransaction,
};
use std::cell::Cell;
use xlaunch::{
    allocation::{self, Contribution, Payout},
    distribution::{Distribution, Simulation, Simulator},
    launch::{self, Launch},
    sender::{Journal, JournalRecord},
    state::Config,
    transaction::{self, Envelope, SenderTier},
};

fn key(n: u8) -> Pubkey {
    Pubkey::new_from_array([n; 32])
}
fn creator() -> Pubkey {
    key(19)
}
fn input(sequence: u64, wallet: u8, budget: u64) -> Contribution {
    Contribution {
        transfer_id: format!("payment-{sequence}"),
        sequence,
        wallet: key(wallet),
        quote_budget: budget,
    }
}
fn envelope(payer: Pubkey) -> Envelope {
    Envelope {
        payer,
        blockhash: Hash::default(),
        last_valid_block_height: 500,
        lookup_tables: vec![],
        compute_unit_limit: transaction::SETTLEMENT_COMPUTE_UNIT_LIMIT,
        compute_unit_price: transaction::SETTLEMENT_COMPUTE_UNIT_PRICE_MICRO_LAMPORTS,
        tip_lamports: 5000,
        tier: SenderTier::Swqos,
    }
}
fn launch() -> Launch {
    Launch {
        mint: key(17),
        buyer: key(18),
        creator: creator(),
        name: "Verification".into(),
        symbol: "VERIFY".into(),
        metadata_uri: "https://example.com/token.json".into(),
        quote_asset: xlaunch::state::QuoteAsset::Usdc,
        quote_budget: 100_000_000,
    }
}
fn normalize(ix: &Instruction) -> Value {
    json!({"program_id":ix.program_id.to_string(),"accounts":ix.accounts.iter().map(|a|
        json!({"pubkey":a.pubkey.to_string(),"is_signer":a.is_signer,"is_writable":a.is_writable})).collect::<Vec<_>>(),
        "data_hex":ix.data.iter().map(|b|format!("{b:02x}")).collect::<String>()})
}
fn mint_account() -> Account {
    let state = Mint {
        decimals: 6,
        supply: 1_000_000_000_000_000,
        is_initialized: true,
        ..Default::default()
    };
    let mut data = vec![0; Mint::LEN];
    Mint::pack(state, &mut data).unwrap();
    Account {
        lamports: 1,
        data,
        owner: Pubkey::new_from_array(
            pump_rust_client::constants::SPL_TOKEN_2022_PROGRAM_ID.to_bytes(),
        ),
        executable: false,
        rent_epoch: 0,
    }
}

#[test]
fn saved_mainnet_execution_matches_quote_and_validates_real_created_mint() -> Result<()> {
    let review: Value =
        serde_json::from_str(include_str!("../docs/verification/create-buy.review.json"))?;
    let execution: Value = serde_json::from_str(include_str!(
        "../docs/verification/create-buy.simulation.json"
    ))?;
    let transfer_review: Value =
        serde_json::from_str(include_str!("../docs/verification/airdrop.review.json"))?;
    let transfer: Value =
        serde_json::from_str(include_str!("../docs/verification/airdrop.simulation.json"))?;
    assert!(execution["value"]["err"].is_null() && transfer["value"]["err"].is_null());
    let tokens = review["base_tokens"].as_u64().unwrap();
    let account: solana_rpc_client_api::response::UiAccount =
        serde_json::from_value(execution["value"]["accounts"][0].clone())?;
    let actual_mint = account.to_account().unwrap();
    let mint = review["mint"].as_str().unwrap().parse()?;
    let recipient: Pubkey = transfer_review["recipient"].as_str().unwrap().parse()?;
    Distribution::new(
        mint,
        &actual_mint,
        key(50),
        key(50),
        vec![Payout {
            wallet: recipient,
            amount: tokens,
        }],
    )?;
    let receipt = transfer["value"]["postTokenBalances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["owner"] == recipient.to_string() && b["mint"] == review["mint"])
        .unwrap();
    assert_eq!(
        receipt["uiTokenAmount"]["amount"]
            .as_str()
            .unwrap()
            .parse::<u64>()?,
        tokens
    );
    assert_eq!(receipt["uiTokenAmount"]["decimals"], 6);
    // Recorded RPC evidence is not re-executed by this offline regression test.
    assert_eq!(
        review["wire_bytes"],
        STANDARD
            .decode(review["unsigned_transaction_base64"].as_str().unwrap())?
            .len()
    );
    Ok(())
}

#[test]
fn fresh_curve_matches_independently_generated_official_sdk_quotes() -> Result<()> {
    let config = Config::saved_evidence()?;
    let reference: Value =
        serde_json::from_str(include_str!("../docs/verification/sdk-reference.json"))?;
    let curve = allocation::initial_curve(&config, creator())?;
    for example in reference["quotes"].as_array().unwrap() {
        let amount = example["tokens"].as_str().unwrap().parse::<u64>()?;
        let expected = example["gross_quote"].as_str().unwrap().parse::<u64>()?;
        let actual = pump_rust_client::math::bonding_curve::buy_sol_amount_from_token_amount(
            // Compare via public allocation: a budget just below the independent
            // cost must not afford that target; the exact cost must afford it.
            &decoded_global(),
            Some(&decoded_fees()),
            &curve,
            1_000_000_000_000_000,
            amount,
        )
        .unwrap();
        assert_eq!(actual, expected);
        assert!(allocation::affordable_buy(&config, &curve, expected - 1)?.tokens < amount);
        assert!(allocation::affordable_buy(&config, &curve, expected)?.tokens >= amount);
    }
    Ok(())
}

fn decoded_global() -> pump_rust_client::state::Global {
    let v: Value =
        serde_json::from_str(include_str!("../docs/verification/pump-global.rpc.json")).unwrap();
    pump_rust_client::accounts::decode_global(
        &STANDARD
            .decode(v["result"]["value"]["data"][0].as_str().unwrap())
            .unwrap(),
    )
    .unwrap()
}
fn decoded_fees() -> pump_rust_client::state::FeeConfig {
    let v: Value =
        serde_json::from_str(include_str!("../docs/verification/pump-fees.rpc.json")).unwrap();
    pump_rust_client::accounts::decode_fee_config(
        &STANDARD
            .decode(v["result"]["value"]["data"][0].as_str().unwrap())
            .unwrap(),
    )
    .unwrap()
}

#[test]
fn ordered_purchases_conserve_money_and_reward_earlier_arrival() -> Result<()> {
    let config = Config::saved_evidence()?;
    let rows = allocation::allocate(
        &config,
        creator(),
        &[
            input(1, 1, 100_000_000),
            input(2, 2, 100_000_000),
            input(3, 1, 100_000_000),
        ],
    )?;
    assert!(rows[0].weight > rows[1].weight && rows[1].weight > rows[2].weight);
    for row in &rows {
        assert_eq!(
            row.curve_quote + row.fee_quote + row.unspent_quote,
            row.contribution.quote_budget
        );
    }
    for tokens in [0, 1, 2, 100, 987_654_321, 793_100_000_000_000] {
        let payouts = allocation::distribute(&rows, tokens)?;
        assert_eq!(payouts.iter().map(|p| p.amount).sum::<u64>(), tokens);
        assert!(payouts.len() <= 2);
        assert_eq!(payouts, allocation::distribute(&rows, tokens)?);
    }
    Ok(())
}

#[test]
fn rejects_ambiguous_order_and_duplicate_transfer_identity() -> Result<()> {
    let config = Config::saved_evidence()?;
    assert!(
        allocation::allocate(&config, creator(), &[input(2, 1, 100), input(1, 2, 100)]).is_err()
    );
    assert!(
        allocation::allocate(&config, creator(), &[input(1, 1, 100), input(1, 2, 100)]).is_err()
    );
    let mut duplicate = input(2, 2, 100);
    duplicate.transfer_id = "payment-1".into();
    assert!(allocation::allocate(&config, creator(), &[input(1, 1, 100), duplicate]).is_err());
    Ok(())
}

#[test]
fn cap_overflow_and_dust_are_explicit_unspent_liabilities() -> Result<()> {
    let rows = allocation::allocate(
        &Config::saved_evidence()?,
        creator(),
        &[
            input(1, 1, 1),
            input(2, 2, 20_000_000_000),
            input(3, 3, 100_000_000),
        ],
    )?;
    assert_eq!((rows[0].weight, rows[0].unspent_quote), (0, 1));
    assert_eq!(rows[1].weight, 793_100_000_000_000);
    assert_eq!(rows[1].curve_quote + rows[1].fee_quote, 12_313_451_289);
    assert_eq!((rows[2].weight, rows[2].unspent_quote), (0, 100_000_000));
    Ok(())
}

#[test]
fn create_and_buy_match_current_idl_and_separate_sdk_instruction_bytes() -> Result<()> {
    let plan = launch::build(&Config::saved_evidence()?, &launch())?;
    let reference: Value =
        serde_json::from_str(include_str!("../docs/verification/sdk-reference.json"))?;
    let idl: Value = serde_json::from_str(include_str!("../docs/verification/pump-idl.json"))?;
    let mut create = plan.instructions[0].clone();
    // New TS SDK explicitly writes false for the documented optional trailing
    // holder reward flag; Rust SDK uses documented omission (=false).
    create.data.push(0);
    assert_eq!(normalize(&create), reference["create"]);
    let mut buy = plan.instructions.last().unwrap().clone();
    assert_eq!(
        u64::from_le_bytes(buy.data[8..16].try_into()?),
        plan.quote.tokens
    );
    assert_eq!(
        u64::from_le_bytes(buy.data[16..24].try_into()?),
        launch().quote_budget
    );
    // Reference includes every live authorized recipient pair, not a guessed PDA.
    buy.data[8..16].copy_from_slice(&1_000_000_000_000u64.to_le_bytes());
    assert!(
        reference["buys"]
            .as_array()
            .unwrap()
            .contains(&normalize(&buy))
    );
    for (name, ix) in [
        ("create_v2", &plan.instructions[0]),
        ("buy_v2", plan.instructions.last().unwrap()),
    ] {
        let schema = idl["instructions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["name"] == name)
            .unwrap();
        assert_eq!(
            ix.data[..8],
            schema["discriminator"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_u64().unwrap() as u8)
                .collect::<Vec<_>>()
        );
        for (expected, actual) in schema["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .zip(&ix.accounts)
        {
            assert_eq!(
                actual.is_signer,
                expected["signer"].as_bool().unwrap_or(false)
            );
            assert_eq!(
                actual.is_writable,
                expected["writable"].as_bool().unwrap_or(false)
            );
        }
    }
    Ok(())
}

struct SizeOnly;
impl Simulator for SizeOnly {
    fn simulate(&self, _: &VersionedTransaction) -> Result<Simulation> {
        Ok(Simulation::Success {
            units_consumed: 100,
        })
    }
}
struct Limited {
    max_groups: usize,
    calls: Cell<usize>,
}
impl Simulator for Limited {
    fn simulate(&self, tx: &VersionedTransaction) -> Result<Simulation> {
        self.calls.set(self.calls.get() + 1);
        let VersionedMessage::V0(m) = &tx.message else {
            unreachable!()
        };
        if (m.instructions.len() - 3) / 2 > self.max_groups {
            Ok(Simulation::ResourceLimit {
                detail: "synthetic resource limit, not an on-chain simulation".into(),
            })
        } else {
            Ok(Simulation::Success {
                units_consumed: 100,
            })
        }
    }
}

fn distribution(count: u8) -> Result<Distribution> {
    Distribution::new(
        key(17),
        &mint_account(),
        key(18),
        key(18),
        (1..=count)
            .filter(|i| *i != 18)
            .map(|i| Payout {
                wallet: key(i),
                amount: u64::from(i),
            })
            .collect(),
    )
}

#[test]
fn packs_all_small_payouts_and_uses_measured_packet_boundary_for_large_list() -> Result<()> {
    let e = envelope(key(18));
    let small = distribution(2)?;
    assert_eq!(small.next_batch(0, &e, &SizeOnly)?.end, 2);
    let d = distribution(50)?;
    let first = d.next_batch(0, &e, &SizeOnly)?;
    assert!(first.end > 2 && first.end < d.len());
    assert!(
        transaction::wire_size(&e.compile(&first.instructions)?)? <= transaction::MAX_WIRE_BYTES
    );
    let next = d.next_batch(first.end, &e, &SizeOnly)?;
    let mut larger = first.instructions.clone();
    larger.extend_from_slice(&next.instructions[..2]);
    assert!(!transaction::fits(&e.compile(&larger)?)?);
    let mut cursor = 0;
    let mut delivered = 0;
    while cursor < d.len() {
        let b = d.next_batch(cursor, &e, &SizeOnly)?;
        delivered += b.payouts.len();
        cursor = b.end;
    }
    assert_eq!(delivered, d.len());
    Ok(())
}

#[test]
fn simulation_reduces_batch_and_non_resource_failure_stops() -> Result<()> {
    let d = distribution(12)?;
    let simulator = Limited {
        max_groups: 3,
        calls: Cell::new(0),
    };
    assert_eq!(d.next_batch(0, &envelope(key(18)), &simulator)?.end, 3);
    assert!(simulator.calls.get() > 1);
    struct Broken;
    impl Simulator for Broken {
        fn simulate(&self, _: &VersionedTransaction) -> Result<Simulation> {
            anyhow::bail!("insufficient tokens")
        }
    }
    assert!(
        d.next_batch(0, &envelope(key(18)), &Broken)
            .unwrap_err()
            .to_string()
            .contains("insufficient tokens")
    );
    Ok(())
}

#[test]
fn signing_measures_real_wire_and_journal_blocks_duplicate_intent() -> Result<()> {
    let payer = Keypair::new();
    let e = envelope(payer.pubkey());
    let tx = e.sign(&[], &[&payer])?;
    assert_eq!(tx.wire()?.len(), transaction::wire_size(&e.compile(&[])?)?);
    tx.transaction().verify_and_hash_message()?;
    let temp = tempfile::tempdir()?;
    let journal = Journal::open(temp.path())?;
    let dir = journal.reserve("mint/distribution/0", &tx)?;
    assert!(journal.reserve("mint/distribution/0", &tx).is_err());
    let record: JournalRecord = serde_json::from_slice(&std::fs::read(dir.join("signed.json"))?)?;
    assert_eq!(record.signature, tx.signature().to_string());
    assert_eq!(STANDARD.decode(record.wire_base64)?, tx.wire()?);
    Ok(())
}

#[test]
fn atomic_launch_refuses_oversized_message_and_can_use_lookup_tables() -> Result<()> {
    let payer = Keypair::new();
    let mint = Keypair::new();
    let mut params = launch();
    params.buyer = payer.pubkey();
    params.mint = mint.pubkey();
    let plan = launch::build(&Config::saved_evidence()?, &params)?;
    let mut e = envelope(payer.pubkey());
    assert!(!transaction::fits(&e.compile(&plan.instructions)?)?);
    assert!(e.sign(&plan.instructions, &[&payer, &mint]).is_err());
    // Synthetic ALT checks compiler/serialization only; never used for submission.
    let mut addresses: Vec<_> = plan
        .instructions
        .iter()
        .flat_map(|ix| ix.accounts.iter())
        .filter(|a| !a.is_signer)
        .map(|a| a.pubkey)
        .collect();
    addresses.sort();
    addresses.dedup();
    e.lookup_tables.push(AddressLookupTableAccount {
        key: key(99),
        addresses,
    });
    let signed = e.sign(&plan.instructions, &[&payer, &mint])?;
    assert!(signed.wire()?.len() <= 1232);
    Ok(())
}

#[test]
fn settlement_compute_instructions_keep_priority_fee_at_1400_lamports() -> Result<()> {
    let payer = Keypair::new();
    let e = envelope(payer.pubkey());
    let instructions = e.instructions(&[])?;
    // Compare actual program IDs and bytes across the SDK's two Solana generations.
    let expected = [
        solana_compute_budget_interface::ComputeBudgetInstruction::set_compute_unit_limit(500_000),
        solana_compute_budget_interface::ComputeBudgetInstruction::set_compute_unit_price(2_800),
    ];
    for (actual, expected) in instructions.iter().zip(expected) {
        assert_eq!(actual.program_id.to_bytes(), expected.program_id.to_bytes());
        assert_eq!(actual.data, expected.data);
        assert!(actual.accounts.is_empty());
    }
    assert_eq!(
        (u128::from(e.compute_unit_limit) * u128::from(e.compute_unit_price)).div_ceil(1_000_000),
        1_400
    );
    e.sign(&[], &[&payer])?
        .transaction()
        .verify_and_hash_message()?;
    Ok(())
}
