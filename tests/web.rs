use anyhow::Result;
use serde_json::json;
use solana_sdk::{
    pubkey::Pubkey,
    signature::{Keypair, Signer},
};
use xlaunch::web::{
    model::{
        CAP_CENTS, Inbox, Incoming, IncomingPayment, LaunchConfig, Processed, Socials, Store,
        digest,
    },
    pinata::Pinned,
};
fn record(sequence: u64, time: &str, amount: &str, memo: String) -> Incoming {
    Incoming {
        sequence,
        received_at: time.into(),
        payment: IncomingPayment {
            sender: "@tester".into(),
            amount: json!(amount),
            memo,
        },
    }
}
fn inbox(records: Vec<Incoming>) -> Inbox {
    Inbox {
        version: 1,
        next_sequence: records.len() as u64 + 1,
        transactions: records,
    }
}
fn note() -> String {
    "Name: Social Token\nSymbol: SOCIAL\nImage Uri: ipfs://test-image\nWebsite: https://example.com\nX: https://x.com/example\nTelegram: https://t.me/example".into()
}
#[test]
fn receiver_replay_preserves_reserved_key_socials_and_conserves_cap_and_late_payments() -> Result<()>
{
    let dir = tempfile::tempdir()?;
    let mut store = Store::open(dir.path().into())?;
    let mut records = vec![record(1, "2026-09-22T00:00:00Z", "1.00", note())];
    assert!(store.ingest(inbox(records.clone()))?);
    let coin = store.data.coins.values().next().unwrap();
    let mint = coin.mint.clone();
    let secret = coin.mint_keypair.clone();
    assert_eq!(
        Keypair::try_from(secret.as_slice())?.pubkey().to_string(),
        mint
    );
    let wallet = Pubkey::new_from_array([7; 32]);
    records.push(record(
        2,
        "2026-09-22T00:00:10Z",
        "11999.99",
        format!("{mint} {wallet}"),
    ));
    records.push(record(
        3,
        "2026-09-22T00:00:11Z",
        "1.00",
        format!("{mint} {wallet}"),
    ));
    records.push(record(
        4,
        "2026-09-22T01:00:00Z",
        "5.00",
        format!("{mint} {wallet}"),
    ));
    store.ingest(inbox(records.clone()))?;
    assert_eq!(store.data.coins[&mint].raised(), CAP_CENTS);
    assert_eq!(store.data.coins[&mint].buys[1].accepted_cents, 1);
    assert_eq!(store.data.coins[&mint].buys[1].unallocated_cents, 99);
    assert_eq!(store.data.coins[&mint].buys[2].unallocated_cents, 500);
    drop(store);
    let mut store = Store::open(dir.path().into())?;
    assert!(!store.ingest(inbox(records.clone()))?);
    assert_eq!(store.data.coins.len(), 1);
    assert_eq!(store.data.coins[&mint].mint_keypair, secret);
    assert_eq!(
        store.data.coins[&mint].config.socials.twitter,
        "https://x.com/example"
    );
    records[0].payment.memo = "changed history".into();
    assert!(store.ingest(inbox(records)).is_err());
    Ok(())
}
#[test]
fn pinned_json_socials_and_uri_reach_the_actual_pump_create_instruction() -> Result<()> {
    let config = LaunchConfig::from_note(&note())?;
    let metadata = config.metadata();
    assert_eq!(metadata["twitter"], "https://x.com/example");
    assert_eq!(metadata["telegram"], "https://t.me/example");
    assert_eq!(metadata["external_url"], "https://example.com");
    let dir = tempfile::tempdir()?;
    let mut store = Store::open(dir.path().into())?;
    store.ingest(inbox(vec![record(1, "2026-09-22T00:00:00Z", "1", note())]))?;
    let coin = store.data.coins.values_mut().next().unwrap();
    let buyer = Keypair::new().pubkey();
    assert!(coin.launch_request(buyer, buyer, 1_000_000_000).is_err());
    let uri = "ipfs://bafkreih5aznjvttude6c3wbvqeebb6rlx5wkbzyppv7garjiubll2ceym4";
    coin.metadata = Some(Pinned {
        cid: uri[7..].into(),
        uri: uri.into(),
        url: format!("https://ipfs.io/ipfs/{}", &uri[7..]),
    });
    let launch = coin.launch_request(buyer, buyer, 1_000_000_000)?;
    let plan = xlaunch::launch::build(&xlaunch::state::Config::saved_evidence()?, &launch)?;
    assert!(
        plan.instructions[0]
            .data
            .windows(uri.len())
            .any(|v| v == uri.as_bytes())
    );
    assert!(
        !plan.instructions[0]
            .data
            .windows("ipfs://test-image".len())
            .any(|v| v == b"ipfs://test-image")
    );
    Ok(())
}
#[test]
fn social_validation_rejects_script_urls_and_duplicate_note_fields() -> Result<()> {
    for value in [
        "javascript:alert(1)",
        "data:text/html,x",
        "https://user:password@example.com",
    ] {
        let socials = Socials {
            website: value.into(),
            ..Default::default()
        };
        assert!(socials.validate().is_err());
    }
    assert!(LaunchConfig::from_note(&(note() + "\nX: https://x.com/different")).is_err());
    assert!(LaunchConfig::from_note(&(note() + "\nUnexpected: x")).is_err());
    Ok(())
}

#[test]
fn three_line_note_restores_image_and_socials_from_durable_metadata_before_launch() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut store = Store::open(dir.path().into())?;
    let config = LaunchConfig::from_note(&note())?;
    let uri = "ipfs://bafkreig6dyl2vjvcpb4ffynemnbohbr22uwo3pj6gnxo2qbjfch2telxby";
    let pin = Pinned {
        cid: uri[7..].into(),
        uri: uri.into(),
        url: format!("https://ipfs.io/ipfs/{}", &uri[7..]),
    };
    store.save_prepared(config.clone(), pin.clone())?;
    // Repeating a prepare is idempotent; changing what an acknowledged URI means is forbidden.
    store.save_prepared(config.clone(), pin.clone())?;
    let mut conflicting = config.clone();
    conflicting.socials.twitter = "https://x.com/changed".into();
    assert!(store.save_prepared(conflicting, pin).is_err());
    drop(store);
    let mut store = Store::open(dir.path().into())?;
    let compact = format!("Name: Social Token\nSymbol: SOCIAL\nMetadata Uri: {uri}");
    assert_eq!(compact.lines().count(), 3);
    assert!(LaunchConfig::from_note(&compact.replace("Metadata Uri", "Image Uri")).is_ok());
    assert!(LaunchConfig::from_note(&format!("{compact}\nImage Uri: {uri}")).is_err());
    let records = vec![
        record(
            1,
            "2026-09-22T00:00:00Z",
            "1",
            compact.replace("Social Token", "Wrong Name"),
        ),
        record(
            2,
            "2026-09-22T00:00:01Z",
            "1",
            format!("{compact}\nX: https://x.com/conflicting"),
        ),
        record(3, "2026-09-22T00:00:02Z", "1", compact),
    ];
    store.ingest(inbox(records.clone()))?;
    assert_eq!(store.data.processed[0].unallocated_cents, 100);
    assert_eq!(store.data.processed[1].unallocated_cents, 100);
    assert_eq!(store.data.coins.len(), 1);
    let coin = store.data.coins.values().next().unwrap();
    assert_eq!(coin.config.socials, config.socials);
    assert_eq!(coin.config.image_uri, "ipfs://test-image");
    assert_eq!(coin.metadata.as_ref().unwrap().uri, uri);
    let buyer = Keypair::new().pubkey();
    let launch = coin.launch_request(buyer, buyer, 1_000_000_000)?;
    let plan = xlaunch::launch::build(&xlaunch::state::Config::saved_evidence()?, &launch)?;
    assert!(
        plan.instructions[0]
            .data
            .windows(uri.len())
            .any(|v| v == uri.as_bytes())
    );
    let mint = coin.mint.clone();
    drop(store);
    let mut store = Store::open(dir.path().into())?;
    assert!(!store.ingest(inbox(records))?);
    assert_eq!(store.data.coins[&mint].config.socials, config.socials);
    assert_eq!(store.data.coins[&mint].metadata.as_ref().unwrap().uri, uri);
    Ok(())
}

#[test]
fn flattened_memos_parse_explicit_labels_without_losing_spaces_in_the_name() -> Result<()> {
    for separator in [" ", "        ", "\t", "\u{a0}", "\n"] {
        for label in ["Metadata Uri", "Image Uri", "metadata uri"] {
            let memo = format!(
                "Name: My Social Token{separator}Symbol: SOCIAL{separator}{label}: ipfs://test-image"
            );
            let config = LaunchConfig::from_note(&memo)?;
            assert_eq!(config.name, "My Social Token");
            assert_eq!(config.symbol, "SOCIAL");
            assert_eq!(config.image_uri, "ipfs://test-image");
            assert!(LaunchConfig::from_note(&format!("{memo} Symbol: OTHER")).is_err());
        }
    }
    let old = LaunchConfig::from_note(&note().replace('\n', "   "))?;
    assert_eq!(old.socials.twitter, "https://x.com/example");
    Ok(())
}

#[test]
fn recovery_reuses_the_received_payment_and_is_idempotent_across_restart() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut store = Store::open(dir.path().into())?;
    let config = LaunchConfig::from_note(&note())?;
    let uri = "ipfs://bafkreig6dyl2vjvcpb4ffynemnbohbr22uwo3pj6gnxo2qbjfch2telxby";
    store.save_prepared(
        config,
        Pinned {
            cid: uri[7..].into(),
            uri: uri.into(),
            url: format!("https://ipfs.io/ipfs/{}", &uri[7..]),
        },
    )?;
    let unrelated = record(1, "2026-09-22T09:00:00Z", "5", "Here buddy".into());
    store.ingest(inbox(vec![unrelated.clone()]))?;
    let source = record(
        2,
        "2026-09-22T09:07:45Z",
        "1",
        format!("Name: Social Token        Symbol: SOCIAL               Image Uri: {uri}"),
    );
    // Model the actual old parser's durable rejection without changing the receipt.
    let mut legacy = store.data.clone();
    legacy.processed.push(Processed {
        sequence: 2,
        hash: digest(&source)?,
        kind: "unmatched".into(),
        mint: None,
        unallocated_cents: 100,
        reason: Some("Symbol is required".into()),
    });
    store.commit(legacy)?;
    let unrelated_before = digest(&store.data.processed[0])?;
    let mut tampered = source.clone();
    tampered.payment.memo.push(' ');
    assert!(store.recover_launch(&tampered).is_err());
    assert!(store.data.coins.is_empty());
    let mint = store.recover_launch(&source)?;
    assert_eq!(store.data.processed[1].hash, digest(&source)?);
    assert_eq!(store.data.processed[1].unallocated_cents, 0);
    assert_eq!(digest(&store.data.processed[0])?, unrelated_before);
    let coin = &store.data.coins[&mint];
    assert_eq!(coin.launch_sequence, 2);
    assert_eq!(
        coin.created_at,
        chrono::DateTime::parse_from_rfc3339(&source.received_at)?.timestamp()
    );
    assert_eq!(coin.deadline, coin.created_at + 3600);
    assert!(coin.buys.is_empty());
    let key = coin.mint_keypair.clone();
    drop(store);
    let mut store = Store::open(dir.path().into())?;
    assert_eq!(store.recover_launch(&source)?, mint);
    assert_eq!(store.data.coins.len(), 1);
    assert_eq!(store.data.coins[&mint].mint_keypair, key);
    assert!(!store.ingest(inbox(vec![unrelated, source]))?);
    Ok(())
}

#[test]
fn curve_market_cap_replays_fees_in_order_and_survives_restart() -> Result<()> {
    use xlaunch::{allocation, state::Config, web::market::curve_market_cap};
    let dir = tempfile::tempdir()?;
    let config = Config::saved_evidence()?; // Tests only; production fetches live processed state.
    let mut store = Store::open(dir.path().into())?;
    store.set_market_config(config.snapshot())?;
    let mut records = vec![record(1, "2026-09-22T00:00:00Z", "1.00", note())];
    store.ingest(inbox(records.clone()))?;
    let coin = store.data.coins.values().next().unwrap();
    let mint = coin.mint.clone();
    let initial_cap = curve_market_cap(coin)?.unwrap();
    let mut curve = allocation::initial_curve(&config, mint.parse()?)?;
    assert_eq!(
        initial_cap,
        (u128::from(curve.virtual_quote_reserves) * u128::from(curve.token_total_supply)
            / u128::from(curve.virtual_token_reserves)) as f64
            / 1_000_000.0
    );
    let wallet = Pubkey::new_from_array([7; 32]);
    for (sequence, amount, budget) in [(2, "100.00", 100_000_000), (3, "900.00", 900_000_000)] {
        records.push(record(
            sequence,
            "2026-09-22T00:00:10Z",
            amount,
            format!("{mint} {wallet}"),
        ));
        let buy = allocation::affordable_buy(&config, &curve, budget)?;
        assert!(buy.total_quote > buy.curve_quote);
        curve.virtual_token_reserves -= buy.tokens;
        curve.real_token_reserves -= buy.tokens;
        curve.virtual_quote_reserves += buy.curve_quote;
    }
    store.ingest(inbox(records.clone()))?;
    let cap = curve_market_cap(&store.data.coins[&mint])?.unwrap();
    let expected = (u128::from(curve.virtual_quote_reserves) * u128::from(curve.token_total_supply)
        / u128::from(curve.virtual_token_reserves)) as f64
        / 1_000_000.0;
    assert_eq!(cap, expected);
    assert!(cap > initial_cap);
    assert_ne!(cap, 1000.0);
    let opening = serde_json::to_value(&store.data.coins[&mint].market_opening)?;
    drop(store);
    let mut store = Store::open(dir.path().into())?;
    let mut newer = serde_json::to_value(config.snapshot())?;
    newer["slot"] = json!(config.slot + 1);
    store.set_market_config(serde_json::from_value(newer)?)?;
    assert_eq!(curve_market_cap(&store.data.coins[&mint])?, Some(cap));
    assert_eq!(
        serde_json::to_value(&store.data.coins[&mint].market_opening)?,
        opening
    );
    // A funded legacy coin with no opening evidence must not be retroactively priced.
    let mut legacy = store.data.clone();
    legacy.coins.get_mut(&mint).unwrap().market_opening = None;
    store.commit(legacy)?;
    store.set_market_config(config.snapshot())?;
    assert_eq!(curve_market_cap(&store.data.coins[&mint])?, None);
    Ok(())
}

#[test]
fn buy_memos_accept_either_address_order_and_whitespace_without_changing_accounting() -> Result<()>
{
    let dir = tempfile::tempdir()?;
    let mut store = Store::open(dir.path().into())?;
    let mut records = vec![record(1, "2026-09-22T00:00:00Z", "1.00", note())];
    store.ingest(inbox(records.clone()))?;
    let mint = store.data.coins.keys().next().unwrap().clone();
    let wallet = Pubkey::new_from_array([7; 32]);
    for memo in [
        format!("{mint} {wallet}"),
        format!("{wallet} {mint}"),
        format!("  {mint}    {wallet}  "),
        format!("\t{wallet}\n\r\t{mint}\n"),
        format!("{wallet}\u{00a0}{mint}"),
    ] {
        records.push(record(
            records.len() as u64 + 1,
            "2026-09-22T00:00:10Z",
            "25.50",
            memo,
        ));
    }
    // Catalog membership, not position or a live-chain lookup, determines the token.
    drop(store);
    let mut store = Store::open(dir.path().into())?;
    store.ingest(inbox(records.clone()))?;
    assert_eq!(store.data.coins[&mint].raised(), 12_750);
    let buys = &store.data.coins[&mint].buys;
    assert_eq!(buys.len(), 5);
    for (i, buy) in buys.iter().enumerate() {
        assert_eq!(buy.sequence, i as u64 + 2);
        assert_eq!(buy.wallet, wallet.to_string());
        assert_eq!(buy.accepted_cents, 2550);
        assert_eq!(buy.unallocated_cents, 0);
    }
    records.push(record(
        7,
        "2026-09-22T00:59:59Z",
        "12000.00",
        format!("{wallet} {mint}"),
    ));
    records.push(record(
        8,
        "2026-09-22T01:00:00Z",
        "3.00",
        format!("{wallet} {mint}"),
    ));
    store.ingest(inbox(records.clone()))?;
    let coin = &store.data.coins[&mint];
    assert_eq!(coin.raised(), CAP_CENTS);
    assert_eq!(coin.buys[5].accepted_cents, CAP_CENTS - 12_750);
    assert_eq!(coin.buys[5].unallocated_cents, 12_750);
    assert_eq!(coin.buys[6].accepted_cents, 0);
    assert_eq!(coin.buys[6].unallocated_cents, 300);
    let durable = serde_json::to_value(&store.data)?;
    drop(store);
    let mut store = Store::open(dir.path().into())?;
    assert!(!store.ingest(inbox(records))?);
    assert_eq!(serde_json::to_value(&store.data)?, durable);
    Ok(())
}

#[test]
fn ambiguous_unknown_and_invalid_buy_memos_stay_unallocated_and_do_not_block_valid_buys()
-> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut store = Store::open(dir.path().into())?;
    let mut records = vec![
        record(1, "2026-09-22T00:00:00Z", "1.00", note()),
        record(2, "2026-09-22T00:00:01Z", "1.00", note()),
    ];
    store.ingest(inbox(records.clone()))?;
    let mints: Vec<_> = store.data.coins.keys().cloned().collect();
    let mint = &mints[0];
    let other = &mints[1];
    let wallet = Pubkey::new_from_array([7; 32]);
    let unknown = Pubkey::new_from_array([8; 32]);
    let zero = Pubkey::default();
    let invalid = [
        format!("{mint} {other}"),
        format!("{other} {mint}"),
        format!("{mint} {mint}"),
        format!("{wallet} {unknown}"),
        format!("{mint} {zero}"),
        format!("{zero} {mint}"),
        format!("{mint} {}", "1".repeat(31)), // Base58 syntax but wrong decoded size.
        format!("{mint} {}", "z".repeat(44)), // Valid alphabet, overflowing 32 bytes.
        format!("{mint} O0Il"),
        format!("{mint}{wallet}"),
        format!("{mint},{wallet}"),
        format!("{mint} {wallet} {unknown}"),
        format!("{wallet} {mint} trailing text"),
    ];
    for memo in &invalid {
        records.push(record(
            records.len() as u64 + 1,
            "2026-09-22T00:00:10Z",
            "2.50",
            memo.clone(),
        ));
    }
    records.push(record(
        records.len() as u64 + 1,
        "2026-09-22T00:00:11Z",
        "5.00",
        format!("{wallet} {mint}"),
    ));
    store.ingest(inbox(records))?;
    for processed in &store.data.processed[2..2 + invalid.len()] {
        assert_eq!(processed.kind, "unmatched");
        assert_eq!(processed.unallocated_cents, 250);
        assert!(processed.mint.is_none());
        assert!(processed.reason.is_some());
    }
    assert_eq!(store.data.coins[mint].raised(), 500);
    assert_eq!(store.data.coins[mint].buys.len(), 1);
    assert_eq!(store.data.coins[mint].buys[0].wallet, wallet.to_string());
    assert_eq!(store.data.coins[other].raised(), 0);
    assert!(store.data.coins[other].buys.is_empty());
    Ok(())
}
