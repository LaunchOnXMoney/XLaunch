//! Signed execution against Surfpool only. Never uses a public Sender endpoint.
//! Run with tools/test_surfpool.sh; evidence contains public data, never keys.
use anchor_lang::{AnchorDeserialize, Discriminator};
use anchor_spl::token_2022::spl_token_2022::{
    extension::StateWithExtensions, state::Account as TokenAccount,
};
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use helius::Helius;
use pump_rust_client::{constants, pda, state::BondingCurve};
use serde_json::{Value, json};
use solana_commitment_config::CommitmentConfig;
use solana_rpc_client_api::{
    config::{RpcSendTransactionConfig, RpcSimulateTransactionConfig, RpcTransactionConfig},
    request::RpcRequest,
};
use solana_sdk::{
    account::Account,
    instruction::Instruction,
    message::AddressLookupTableAccount,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
};
use solana_transaction_status_client_types::{
    EncodedConfirmedTransactionWithStatusMeta, UiTransactionEncoding,
};
use std::{
    fs,
    net::SocketAddr,
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};
use xlaunch::{
    allocation::{self, Contribution},
    distribution::Distribution,
    launch::{self, Launch},
    presale::{Executor, RAISE_CAP_MICRO_USDC, Raise, Settlement, Status, Trigger},
    receipt,
    scheduler::Queue,
    sender::{Journal, RpcSimulator},
    state::Config,
    transaction::{self, Envelope, SenderTier, SignedTransaction},
};

fn key(p: anchor_lang::prelude::Pubkey) -> Pubkey {
    Pubkey::new_from_array(p.to_bytes())
}
fn anchor(p: Pubkey) -> anchor_lang::prelude::Pubkey {
    anchor_lang::prelude::Pubkey::new_from_array(p.to_bytes())
}
fn ata(owner: Pubkey, mint: Pubkey, program: Pubkey) -> Pubkey {
    key(pda::associated_token(&anchor(owner), &anchor(program), &anchor(mint)).0)
}

struct Fork {
    helius: Helius,
    payer: Keypair,
    table: AddressLookupTableAccount,
    evidence: PathBuf,
    journal: Journal,
}
impl Fork {
    fn save(&self, name: &str, value: &impl serde::Serialize) -> Result<()> {
        fs::write(
            self.evidence.join(format!("{name}.json")),
            serde_json::to_vec_pretty(value)?,
        )?;
        Ok(())
    }
    fn account(&self, address: Pubkey) -> Result<Option<Account>> {
        Ok(self
            .helius
            .rpc_client
            .solana_client
            .get_account_with_commitment(&address, CommitmentConfig::processed())?
            .value)
    }
    fn tokens(&self, owner: Pubkey, mint: Pubkey, program: Pubkey) -> Result<u64> {
        let Some(account) = self.account(ata(owner, mint, program))? else {
            return Ok(0);
        };
        ensure!(account.owner == program, "token account program differs");
        let state = StateWithExtensions::<TokenAccount>::unpack(&account.data)?;
        ensure!(
            key(state.base.mint) == mint && key(state.base.owner) == owner,
            "token account identity differs"
        );
        Ok(state.base.amount)
    }
    fn envelope(&self) -> Result<Envelope> {
        let (blockhash, last_valid_block_height) = self
            .helius
            .rpc_client
            .solana_client
            .get_latest_blockhash_with_commitment(CommitmentConfig::processed())?;
        Ok(Envelope {
            payer: self.payer.pubkey(),
            blockhash,
            last_valid_block_height,
            lookup_tables: vec![self.table.clone()],
            compute_unit_limit: transaction::SETTLEMENT_COMPUTE_UNIT_LIMIT,
            compute_unit_price: transaction::SETTLEMENT_COMPUTE_UNIT_PRICE_MICRO_LAMPORTS,
            tip_lamports: 5000,
            tier: SenderTier::Swqos,
        })
    }
    fn transaction(
        &self,
        signed: &SignedTransaction,
    ) -> Result<EncodedConfirmedTransactionWithStatusMeta> {
        // Surfpool 1.5.0 exposes processed transaction metadata. Standard mainnet
        // getTransaction does not; this fork-only transport is never a fallback.
        let until = Instant::now() + Duration::from_secs(15);
        loop {
            let response: Option<EncodedConfirmedTransactionWithStatusMeta> =
                self.helius.rpc_client.solana_client.send(
                    RpcRequest::GetTransaction,
                    json!([
                        signed.signature().to_string(),
                        RpcTransactionConfig {
                            encoding: Some(UiTransactionEncoding::Base64),
                            commitment: Some(CommitmentConfig::processed()),
                            max_supported_transaction_version: Some(0),
                        }
                    ]),
                )?;
            if let Some(response) = response {
                return Ok(response);
            }
            ensure!(
                Instant::now() < until,
                "processed receipt timed out: {}",
                signed.signature()
            );
            thread::sleep(Duration::from_millis(100));
        }
    }
    fn send(
        &self,
        name: &str,
        signed: &SignedTransaction,
        expect_failure: bool,
    ) -> Result<EncodedConfirmedTransactionWithStatusMeta> {
        signed.transaction().verify_and_hash_message()?;
        let rpc = &self.helius.rpc_client.solana_client;
        let sim = rpc.simulate_transaction_with_config(
            signed.transaction(),
            RpcSimulateTransactionConfig {
                sig_verify: true,
                replace_recent_blockhash: false,
                commitment: Some(CommitmentConfig::processed()),
                ..Default::default()
            },
        )?;
        self.save(&format!("{name}.simulation"), &sim)?;
        ensure!(
            sim.value.err.is_some() == expect_failure,
            "unexpected simulation result: {:?}; logs={:?}",
            sim.value.err,
            sim.value.logs
        );
        self.journal.reserve(name, signed)?;
        self.save(
            &format!("{name}.wire"),
            &json!({"signature":signed.signature().to_string(),
            "bytes":signed.wire()?.len(), "base64":STANDARD.encode(signed.wire()?)}),
        )?;
        // Failure tests must actually execute to prove rollback. All other sends
        // retain preflight at processed, in addition to exact-signature simulation.
        let signature = rpc.send_transaction_with_config(
            signed.transaction(),
            RpcSendTransactionConfig {
                skip_preflight: expect_failure,
                preflight_commitment: Some(CommitmentConfig::processed().commitment),
                max_retries: Some(0),
                ..Default::default()
            },
        )?;
        ensure!(signature == signed.signature(), "signature mismatch");
        let response = self.transaction(signed)?;
        self.save(&format!("{name}.transaction"), &response)?;
        ensure!(
            response
                .transaction
                .meta
                .as_ref()
                .context("missing metadata")?
                .err
                .is_some()
                == expect_failure,
            "execution differs from expected outcome"
        );
        println!(
            "{name}: {} bytes; signature {signature}; slot {}",
            signed.wire()?.len(),
            response.slot
        );
        Ok(response)
    }
    fn run_raise(
        &self,
        name: &str,
        budgets: &[u64],
        repeated_wallet: bool,
        sellout: bool,
    ) -> Result<Value> {
        let mint = Keypair::new();
        let config = Config::fetch(&self.helius)?;
        let creator = Keypair::new().pubkey();
        let mut wallets: Vec<_> = budgets.iter().map(|_| Keypair::new().pubkey()).collect();
        if repeated_wallet {
            let last = wallets.len() - 1;
            wallets[last] = wallets[0];
        }
        let contributions: Vec<_> = budgets
            .iter()
            .enumerate()
            .map(|(i, budget)| Contribution {
                transfer_id: format!("{name}-{i}"),
                sequence: i as u64,
                wallet: wallets[i],
                quote_budget: *budget,
            })
            .collect();
        let allocations = allocation::allocate(&config, creator, &contributions)?;
        self.save(&format!("{name}.allocations"), &allocations)?;
        for a in &allocations {
            ensure!(
                a.curve_quote + a.fee_quote + a.unspent_quote == a.contribution.quote_budget,
                "money not conserved"
            );
        }
        if repeated_wallet {
            ensure!(
                allocations[0].weight > allocations[1].weight,
                "arrival ordering did not reward earlier buyer"
            );
        }
        let total_budget: u64 = budgets.iter().sum();
        let unspent: u64 = allocations.iter().map(|a| a.unspent_quote).sum();
        let launch = Launch {
            mint: mint.pubkey(),
            buyer: self.payer.pubkey(),
            creator,
            name: format!("Fork {name}"),
            symbol: "FORK".into(),
            metadata_uri: "https://example.com/fork-test.json".into(),
            quote_asset: xlaunch::state::QuoteAsset::Usdc,
            quote_budget: total_budget - unspent,
        };
        let plan = launch::build(&config, &launch)?;
        let signed = self
            .envelope()?
            .sign(&plan.instructions, &[&self.payer, &mint])?;
        let response = self.send(&format!("{name}.launch"), &signed, false)?;
        let bought = receipt::verify_launch_receipt(&response, &signed, &launch, &plan)?;
        ensure!(
            bought.quote_spent == plan.quote.total_quote,
            "actual USDC debit differs from exact quote"
        );
        let payouts = allocation::distribute(&allocations, bought.tokens_received)?;
        ensure!(
            payouts.iter().map(|p| p.amount).sum::<u64>() == bought.tokens_received,
            "distribution loses tokens"
        );
        self.save(&format!("{name}.payouts"), &payouts)?;
        let mint_account = self
            .account(mint.pubkey())?
            .context("created mint missing")?;
        let distribution = Distribution::new(
            mint.pubkey(),
            &mint_account,
            self.payer.pubkey(),
            self.payer.pubkey(),
            payouts.clone(),
        )?;
        // Pre-existing empty recipient ATAs exercise idempotent creation as well
        // as the absent ATA path used by all remaining recipients.
        let groups = distribution.all_instructions();
        let create_atas: Vec<Instruction> = groups.iter().step_by(2).take(2).cloned().collect();
        let setup = self.envelope()?.sign(&create_atas, &[&self.payer])?;
        self.send(&format!("{name}.existing-atas"), &setup, false)?;
        let simulator = RpcSimulator {
            helius: &self.helius,
            min_context_slot: response.slot,
        };
        let (mut cursor, mut batches, mut delivered) = (0, Vec::new(), 0u64);
        while cursor < distribution.len() {
            let envelope = self.envelope()?;
            let batch = distribution.next_batch(cursor, &envelope, &simulator)?;
            // Prove the next complete recipient cannot fit the SAME envelope.
            // These runs hit wire size before CU; CU-based splitting is covered separately.
            if batch.end < distribution.len() {
                ensure!(
                    !transaction::fits(
                        &envelope.compile(&groups[cursor * 2..(batch.end + 1) * 2])?
                    )?,
                    "batch not maximal by wire/account limits"
                );
            }
            let signed = envelope.sign(&batch.instructions, &[&self.payer])?;
            let intent = format!("{name}.batch-{cursor}");
            let response = self.send(&intent, &signed, false)?;
            let paid = receipt::verify_payout_receipt(
                &response,
                &signed,
                mint.pubkey(),
                self.payer.pubkey(),
                &batch.payouts,
            )?;
            delivered += paid.delivered_tokens;
            if cursor == 0 {
                // Re-signing an already reserved intent must be blocked, even
                // with a new valid signature. Reopening also covers process restart.
                let mut changed = envelope.clone();
                changed.compute_unit_price += 1;
                let changed = changed.sign(&batch.instructions, &[&self.payer])?;
                ensure!(
                    changed.signature() != signed.signature(),
                    "test must produce different bytes"
                );
                ensure!(
                    Journal::open(self.evidence.join("journal"))?
                        .reserve(&intent, &changed)
                        .is_err(),
                    "duplicate intent accepted"
                );
                // Identical signature replay must never pay twice. The transport
                // may return AlreadyProcessed or the same signature; verify state.
                let replay = self
                    .helius
                    .rpc_client
                    .solana_client
                    .send_transaction_with_config(
                        signed.transaction(),
                        RpcSendTransactionConfig {
                            skip_preflight: false,
                            preflight_commitment: Some(CommitmentConfig::processed().commitment),
                            max_retries: Some(0),
                            ..Default::default()
                        },
                    );
                self.save(
                    &format!("{name}.replay"),
                    &json!({"result":format!("{replay:?}"),"changed_intent_rejected":true}),
                )?;
                for p in &batch.payouts {
                    ensure!(
                        self.tokens(
                            p.wallet,
                            mint.pubkey(),
                            key(constants::SPL_TOKEN_2022_PROGRAM_ID)
                        )? == p.amount,
                        "replay credited twice"
                    );
                }
            }
            batches.push(
                json!({"start":cursor,"end":batch.end,"wire_bytes":signed.wire()?.len(),
                "simulated_units":batch.units_consumed,"receipt":paid}),
            );
            cursor = batch.end;
        }
        if repeated_wallet {
            ensure!(
                batches.len() > 1 && payouts.len() == budgets.len() - 1,
                "multi-batch/aggregation case not exercised"
            );
        }
        for p in &payouts {
            ensure!(
                self.tokens(
                    p.wallet,
                    mint.pubkey(),
                    key(constants::SPL_TOKEN_2022_PROGRAM_ID)
                )? == p.amount,
                "final recipient balance mismatch"
            );
        }
        ensure!(
            delivered == bought.tokens_received
                && self.tokens(
                    self.payer.pubkey(),
                    mint.pubkey(),
                    key(constants::SPL_TOKEN_2022_PROGRAM_ID)
                )? == 0,
            "source not fully distributed"
        );
        let curve_key = key(pda::pump::bonding_curve(&anchor(mint.pubkey())).0);
        let curve_account = self.account(curve_key)?.context("curve missing")?;
        ensure!(
            curve_account.owner == key(pump_rust_client::pump::ID)
                && curve_account.data.starts_with(BondingCurve::DISCRIMINATOR),
            "curve owner/discriminator mismatch"
        );
        let curve = BondingCurve::deserialize(
            &mut &curve_account.data[BondingCurve::DISCRIMINATOR.len()..],
        )?;
        ensure!(curve.complete == sellout, "unexpected curve completion");
        ensure!(
            curve.real_token_reserves == config.initial_reserves().2 - bought.tokens_received,
            "sellable reserves differ"
        );
        let remaining = self.tokens(
            curve_key,
            mint.pubkey(),
            key(constants::SPL_TOKEN_2022_PROGRAM_ID),
        )?;
        ensure!(
            remaining + bought.tokens_received == curve.token_total_supply,
            "mint supply not conserved"
        );
        if sellout {
            ensure!(
                unspent > 0 && curve.real_token_reserves == 0,
                "cap/overflow case not exercised"
            );
        }
        Ok(
            json!({"case":name,"mint":mint.pubkey().to_string(),"config_slot":config.slot,
            "contributions":contributions.len(),"recipients":payouts.len(),"launch":bought,"batches":batches,
            "unspent_contribution_quote":unspent,"curve_complete":curve.complete,
            "curve_remaining_base_tokens":remaining,"all_recipient_balances_verified":true,
            "source_balance":0,"journal_restart_duplicate_blocked":true,"same_signature_replay_no_double_credit":true}),
        )
    }
    fn failed_buy_rolls_back(&self) -> Result<Value> {
        let mint = Keypair::new();
        let launch = Launch {
            mint: mint.pubkey(),
            buyer: self.payer.pubkey(),
            creator: Keypair::new().pubkey(),
            name: "Rollback test".into(),
            symbol: "FAIL".into(),
            metadata_uri: "https://example.com/fork-test.json".into(),
            quote_asset: xlaunch::state::QuoteAsset::Usdc,
            quote_budget: 100_000_000,
        };
        let mut plan = launch::build(&Config::fetch(&self.helius)?, &launch)?;
        // Published buy_v2 IDL: discriminator [0..8], amount u64 [8..16],
        // max_sol_cost u64 [16..24]. Force the documented 6002 budget failure.
        let idl: Value = serde_json::from_str(include_str!("../docs/verification/pump-idl.json"))?;
        let buy = idl["instructions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["name"] == "buy_v2")
            .unwrap();
        ensure!(
            buy["args"][0]["name"] == "amount"
                && buy["args"][0]["type"] == "u64"
                && buy["args"][1]["name"] == "max_sol_cost"
                && buy["args"][1]["type"] == "u64",
            "IDL buy args changed"
        );
        plan.instructions.last_mut().unwrap().data[16..24].copy_from_slice(&1u64.to_le_bytes());
        let quote_program = key(constants::SPL_TOKEN_PROGRAM_ID);
        let before = self.tokens(self.payer.pubkey(), Config::usdc_mint(), quote_program)?;
        let signed = self
            .envelope()?
            .sign(&plan.instructions, &[&self.payer, &mint])?;
        let response = self.send("rollback", &signed, true)?;
        let meta = serde_json::to_value(response.transaction.meta.as_ref().unwrap())?;
        ensure!(
            meta["err"]["InstructionError"][1]["Custom"] == 6002,
            "unexpected failed instruction: {}",
            meta["err"]
        );
        ensure!(
            self.account(mint.pubkey())?.is_none(),
            "failed buy left mint created"
        );
        ensure!(
            self.account(key(pda::pump::bonding_curve(&anchor(mint.pubkey())).0))?
                .is_none(),
            "failed buy left curve created"
        );
        ensure!(
            self.tokens(self.payer.pubkey(), Config::usdc_mint(), quote_program)? == before,
            "failed buy debited USDC"
        );
        ensure!(
            receipt::verify_launch_receipt(&response, &signed, &launch, &plan).is_err(),
            "failed transaction accepted as receipt"
        );
        Ok(
            json!({"mint":mint.pubkey().to_string(),"signature":signed.signature().to_string(),"error":meta["err"],
            "mint_absent":true,"curve_absent":true,"usdc_unchanged":true,"failed_receipt_rejected":true}),
        )
    }

    fn sol_settle(
        &self,
        raise: &Raise,
        trigger: Trigger,
        mint: &Keypair,
        funded_lamports: u64,
    ) -> Result<Settlement> {
        let config = Config::fetch(&self.helius)?;
        let execution = xlaunch::settlement::execute(
            xlaunch::settlement::SolInputs {
                helius: &self.helius,
                buyer: &self.payer,
                mint,
                envelope: self.envelope()?,
                funded_lamports,
            },
            raise,
            trigger.clone(),
            |intent, signed, _min_slot| self.send(intent, signed, false),
        )?;
        let intent = raise.mint.to_string();
        self.save(&format!("{intent}.execution"), &execution)?;
        let bought = &execution.launch;
        let payouts = &execution.payouts;
        let batches = &execution.batches;
        let total = execution.settlement.tokens_distributed;
        for payout in payouts {
            ensure!(
                self.tokens(
                    payout.wallet,
                    raise.mint,
                    key(constants::SPL_TOKEN_2022_PROGRAM_ID)
                )? == payout.amount,
                "SOL payout mismatch"
            );
        }
        ensure!(
            self.tokens(
                self.payer.pubkey(),
                raise.mint,
                key(constants::SPL_TOKEN_2022_PROGRAM_ID)
            )? == 0,
            "SOL settlement retained tokens"
        );
        let curve_key = key(pda::pump::bonding_curve(&anchor(raise.mint)).0);
        let curve_account = self.account(curve_key)?.context("SOL curve missing")?;
        ensure!(
            curve_account.owner == key(pump_rust_client::pump::ID)
                && curve_account.data.starts_with(BondingCurve::DISCRIMINATOR),
            "SOL curve identity mismatch"
        );
        let curve = BondingCurve::deserialize(
            &mut &curve_account.data[BondingCurve::DISCRIMINATOR.len()..],
        )?;
        ensure!(
            curve.quote_mint == anchor(Pubkey::default()),
            "SOL curve must store the zero quote key"
        );
        ensure!(
            curve.real_quote_reserves == execution.curve_quote_lamports
                && curve.virtual_quote_reserves
                    == config.initial_quote_reserves(xlaunch::state::QuoteAsset::Sol)
                        + execution.curve_quote_lamports,
            "SOL reserves differ from quote"
        );
        ensure!(
            curve.real_token_reserves == config.initial_reserves().2 - bought.tokens_received,
            "SOL token reserves mismatch"
        );
        ensure!(
            curve.complete == (trigger == Trigger::CapReached),
            "unexpected SOL completion"
        );
        self.save(&format!("{intent}.sol-report"), &json!({"trigger":trigger,"accepted_micro_usdc":raise.accepted_micro_usdc(),
            "quote_mint":xlaunch::state::QuoteAsset::Sol.mint().to_string(),"funded_lamports":funded_lamports,
            "buy":bought,"curve_complete":curve.complete,"curve_net_lamports":curve.real_quote_reserves,
            "remaining_supply":curve.token_total_supply-bought.tokens_received,"recipients":payouts.len(),"batches":batches,
            "tokens_distributed":total,"source_balance":0}))?;
        Ok(execution.settlement)
    }
}

struct ForkExecutor<'a> {
    fork: &'a Fork,
    mints: &'a [Keypair],
    calls: usize,
}
impl Executor for ForkExecutor<'_> {
    fn settle(&mut self, raise: &Raise, trigger: Trigger) -> Result<Settlement> {
        self.calls += 1;
        let mint = self
            .mints
            .iter()
            .find(|m| m.pubkey() == raise.mint)
            .context("reserved signer missing")?;
        // Explicit test funding, not an assumed USDC/SOL conversion rate.
        let budget = if trigger == Trigger::CapReached {
            90_000_000_000
        } else {
            10_000_000_000
        };
        self.fork.sol_settle(raise, trigger, mint, budget)
    }
}

fn timed_sol_settlements(fork: &Fork) -> Result<Value> {
    let config = Config::fetch(&fork.helius)?;
    let mints = [Keypair::new(), Keypair::new(), Keypair::new()];
    let dir = fork.evidence.join("raise-queue");
    let mut queue = Queue::open(&dir)?;
    let opened_at = 1_800_000_000;
    for (i, mint) in mints.iter().enumerate() {
        queue.create(Raise::new(
            &config,
            mint.pubkey(),
            Keypair::new().pubkey(),
            opened_at,
            format!("SOL case {i}"),
            "SOLTEST".into(),
            "https://example.com/sol-test.json".into(),
        )?)?;
    }
    let mut executor = ForkExecutor {
        fork,
        mints: &mints,
        calls: 0,
    };
    for i in 0..24 {
        queue.record(
            mints[0].pubkey(),
            Contribution {
                transfer_id: format!("cap-{i}"),
                sequence: i,
                wallet: Keypair::new().pubkey(),
                quote_budget: 500_000_000,
            },
            opened_at + i,
        )?;
    }
    queue.record(
        mints[0].pubkey(),
        Contribution {
            transfer_id: "above-cap".into(),
            sequence: 24,
            wallet: Keypair::new().pubkey(),
            quote_budget: 100_000_000,
        },
        opened_at + 24,
    )?;
    ensure!(
        queue
            .job(mints[0].pubkey())
            .unwrap()
            .raise
            .accepted_micro_usdc()
            == RAISE_CAP_MICRO_USDC,
        "incorrect cap"
    );
    for i in 0..3 {
        queue.record(
            mints[1].pubkey(),
            Contribution {
                transfer_id: format!("partial-{i}"),
                sequence: i,
                wallet: Keypair::new().pubkey(),
                quote_budget: 100_000_000,
            },
            opened_at + i,
        )?;
    }
    ensure!(
        queue.tick(opened_at + 24, &mut executor)? == 1,
        "cap must settle immediately"
    );
    ensure!(executor.calls == 1, "wrong early invocation count");
    ensure!(
        queue.tick(opened_at + 3599, &mut executor)? == 0,
        "deadline fired early"
    );
    drop(queue);
    let mut queue = Queue::open(&dir)?; // Actual restart; deadline and claimed state survive.
    ensure!(
        queue.tick(opened_at + 3600, &mut executor)? == 1,
        "partial raise did not auto-settle at one hour"
    );
    ensure!(
        queue.tick(opened_at + 7200, &mut executor)? == 0 && executor.calls == 2,
        "already distributed raise relaunched"
    );
    for mint in &mints[..2] {
        match &queue.job(mint.pubkey()).unwrap().status {
            Status::Distributed { receipt } => ensure!(
                receipt.tokens_received == receipt.tokens_distributed,
                "undelivered tokens"
            ),
            status => anyhow::bail!("SOL settlement did not finish: {status:?}"),
        }
    }
    ensure!(
        matches!(queue.job(mints[2].pubkey()).unwrap().status, Status::Empty),
        "empty raise must not buy"
    );
    let report = json!({"cap_micro_usdc":RAISE_CAP_MICRO_USDC,"deadline_seconds":3600,"cap_mint":mints[0].pubkey().to_string(),
        "timeout_mint":mints[1].pubkey().to_string(),"executor_calls":executor.calls,"restart_verified":true,"empty_raise_skipped":true});
    fork.save("sol-scheduler-report", &report)?;
    Ok(report)
}

#[test]
#[ignore = "requires isolated Surfpool mainnet fork; run tools/test_surfpool.sh"]
fn mainnet_fork_settlement_e2e() -> Result<()> {
    let endpoint = std::env::var("XLAUNCH_SURFPOOL_URL")
        .context("set XLAUNCH_SURFPOOL_URL to the isolated loopback fork")?;
    let socket: SocketAddr = endpoint
        .strip_prefix("http://")
        .context("fork must use loopback HTTP")?
        .trim_end_matches('/')
        .parse()?;
    ensure!(
        socket.ip().is_loopback(),
        "refusing writes to a non-loopback RPC"
    );
    let helius = Helius::new_with_url(&endpoint)?;
    let version: Value = helius
        .rpc_client
        .solana_client
        .send(RpcRequest::GetVersion, json!([]))?;
    ensure!(
        version["surfnet-version"] == "1.5.0",
        "unverified Surfpool version: {version}"
    );
    let payer = Keypair::new();
    let base =
        std::env::var("XLAUNCH_E2E_EVIDENCE").unwrap_or_else(|_| "var/surfpool-e2e/runs".into());
    let evidence = PathBuf::from(base).join(payer.pubkey().to_string());
    fs::create_dir_all(&evidence)?;
    println!("Evidence: {}", evidence.display());
    let rpc = &helius.rpc_client.solana_client;
    let _: Value = rpc.send(
        RpcRequest::Custom {
            method: "surfnet_setAccount",
        },
        json!([payer.pubkey().to_string(),{"lamports":150_000_000_000u64}]),
    )?;
    let _: Value = rpc.send(RpcRequest::Custom { method: "surfnet_setTokenAccount" }, json!([
        payer.pubkey().to_string(),Config::usdc_mint().to_string(),{"amount":50_000_000_000u64,"state":"initialized"},constants::SPL_TOKEN_PROGRAM_ID.to_string()]))?;
    // Official Pump SDK published table; verify deployed owner, activation, data.
    let table = transaction::fetch_lookup_table(
        &helius,
        "Hyif6eWb8x88RVrvjPfabsgRYnwkVnyByEXTVTXbUcyP".parse()?,
    )?;
    let journal = Journal::open(evidence.join("journal"))?;
    let fork = Fork {
        helius,
        payer,
        table,
        evidence,
        journal,
    };
    fork.save("environment", &json!({"version":version,"endpoint":endpoint,"commitment":"processed",
        "sender_transport":"Helius embedded RPC, fork-only; public Sender not used",
        "upstream":"https://api.mainnet.solana.com","signature_verification":true,"blockhash_verification":true}))?;
    let rollback = fork.failed_buy_rolls_back().context("atomic rollback")?;
    let partial = fork
        .run_raise("partial", &[100_000_000; 26], true, false)
        .context("partial raise")?;
    let sellout = fork
        .run_raise("sellout", &[20_000_000_000, 100_000_000], false, true)
        .context("sellout raise")?;
    let sol_scheduler = timed_sol_settlements(&fork).context("SOL scheduler")?;
    let report = json!({"passed":true,"commitment":"processed","version":version,"rollback":rollback,"partial":partial,"sellout":sellout,"sol_scheduler":sol_scheduler});
    fork.save("report", &report)?;
    println!(
        "PASS: rollback, partial raise, sellout, maximal batches, exact balances, journal and replay\n{}",
        serde_json::to_string_pretty(&report)?
    );
    Ok(())
}
