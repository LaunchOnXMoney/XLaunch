//! Runs the real Helius Sender SDK against a loopback HTTP proxy in a child
//! process. The child-only proxy env prevents any request reaching public Sender.
use anyhow::Result;
use axum::{
    Json, Router,
    extract::State,
    http::{StatusCode, Uri},
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use solana_sdk::{
    hash::Hash,
    signature::{Keypair, Signature, Signer},
    transaction::VersionedTransaction,
};
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
};
use xlaunch::{
    sender::{Journal, sender_regions, submit_once},
    transaction::{
        Envelope, SETTLEMENT_COMPUTE_UNIT_LIMIT, SETTLEMENT_COMPUTE_UNIT_PRICE_MICRO_LAMPORTS,
        SenderTier,
    },
};

struct Fixture {
    mode: &'static str,
    calls: Mutex<Vec<(String, Value)>>,
    all_regions: tokio::sync::Barrier,
}
async fn serve(State(state): State<Arc<Fixture>>, uri: Uri, Json(body): Json<Value>) -> Response {
    state
        .calls
        .lock()
        .unwrap()
        .push((uri.to_string(), body.clone()));
    let id = body["id"].clone();
    if uri.path() == "/rpc" {
        let result = match body["method"].as_str().unwrap() {
            "simulateTransaction" => {
                assert_eq!(body["params"][1]["commitment"], "processed");
                assert_eq!(body["params"][1]["sigVerify"], true);
                assert_eq!(body["params"][1]["replaceRecentBlockhash"], false);
                json!({"context":{"slot":123},"value":{
                    "err":if state.mode == "simulation_failure" { json!({"InstructionError":[0,"ComputationalBudgetExceeded"]}) } else { Value::Null },
                    "logs":[],"unitsConsumed":200_000
                }})
            }
            "getBlockHeight" => json!(100),
            "getSignatureStatuses" => json!({"context":{"slot":123},"value":[{
                "slot":123,"confirmations":1,"err":null,"status":{"Ok":null},"confirmationStatus":"confirmed"
            }]}),
            unexpected => panic!("unexpected SDK RPC: {unexpected}"),
        };
        return Json(json!({"jsonrpc":"2.0","id":id,"result":result})).into_response();
    }
    assert_eq!(uri.path(), "/fast");
    assert_eq!(uri.query(), Some("swqos_only=true"));
    assert_eq!(body["method"], "sendTransaction");
    assert_eq!(
        body["params"][1],
        json!({"encoding":"base64","skipPreflight":true,"maxRetries":0})
    );
    let wire = STANDARD
        .decode(body["params"][0].as_str().unwrap())
        .unwrap();
    let signed: VersionedTransaction = bincode::deserialize(&wire).unwrap();
    signed.verify_and_hash_message().unwrap();
    // Every region must arrive before any can answer. Sequential delivery fails.
    state.all_regions.wait().await;
    if state.mode == "all_failed" || uri.host() == Some("slc-sender.helius-rpc.com") {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "fixture region unavailable",
        )
            .into_response();
    }
    let signature = if uri.host() == Some("tyo-sender.helius-rpc.com") {
        Signature::default() // A region returning a foreign signature is rejected.
    } else {
        signed.signatures[0]
    };
    Json(json!({"jsonrpc":"2.0","id":id,"result":signature.to_string()})).into_response()
}

#[tokio::test]
async fn sdk_fanout_sends_identical_wire_to_all_regions_and_handles_partial_failure() -> Result<()>
{
    for mode in ["partial_failure", "all_failed", "simulation_failure"] {
        let expected: BTreeSet<_> = sender_regions()
            .into_iter()
            .map(|region| {
                format!(
                    "{}?swqos_only=true",
                    helius::optimized_transaction::sender_fast_url(region)
                )
            })
            .collect();
        assert_eq!(expected.len(), 7);
        let fixture = Arc::new(Fixture {
            mode,
            calls: Mutex::new(vec![]),
            all_regions: tokio::sync::Barrier::new(expected.len()),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let router = Router::new().fallback(serve).with_state(fixture.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let output = tokio::task::spawn_blocking(move || {
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "sender_sdk_child", "--nocapture"])
                .env("XLAUNCH_SENDER_FIXTURE", &base)
                .env("XLAUNCH_SENDER_MODE", mode)
                .env("HTTP_PROXY", &base)
                .env("http_proxy", &base)
                .env("HTTPS_PROXY", &base)
                .env("https_proxy", &base)
                .env("ALL_PROXY", &base)
                .env("all_proxy", &base)
                .env("NO_PROXY", "127.0.0.1,localhost")
                .env("no_proxy", "127.0.0.1,localhost")
                .output()
        })
        .await??;
        server.abort();
        assert!(
            output.status.success(),
            "{mode}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let calls = fixture.calls.lock().unwrap();
        let sends: Vec<_> = calls
            .iter()
            .filter(|(_, body)| body["method"] == "sendTransaction")
            .collect();
        if mode == "simulation_failure" {
            assert!(sends.is_empty());
            continue;
        }
        assert_eq!(
            sends.len(),
            7,
            "no dropped regions, duplicate sends or retry broadcasts"
        );
        assert_eq!(
            sends
                .iter()
                .map(|(uri, _)| uri.clone())
                .collect::<BTreeSet<_>>(),
            expected
        );
        let simulated = &calls
            .iter()
            .find(|(_, body)| body["method"] == "simulateTransaction")
            .unwrap()
            .1["params"][0];
        for (_, body) in &sends {
            assert_eq!(&body["params"][0], simulated);
        }
        println!(
            "{mode}: all seven SDK endpoints received the exact simulated wire, one POST each"
        );
    }
    Ok(())
}

#[tokio::test]
async fn sender_sdk_child() -> Result<()> {
    let Ok(base) = std::env::var("XLAUNCH_SENDER_FIXTURE") else {
        return Ok(());
    };
    assert!(base.starts_with("http://127.0.0.1:"));
    let mode = std::env::var("XLAUNCH_SENDER_MODE")?;
    let helius = helius::Helius::new_with_url(&format!("{base}/rpc"))?;
    let payer = Keypair::new();
    let mut envelope = Envelope {
        payer: payer.pubkey(),
        blockhash: Hash::new_unique(),
        last_valid_block_height: 500,
        lookup_tables: vec![],
        compute_unit_limit: SETTLEMENT_COMPUTE_UNIT_LIMIT,
        compute_unit_price: SETTLEMENT_COMPUTE_UNIT_PRICE_MICRO_LAMPORTS,
        tip_lamports: 5_000,
        tier: SenderTier::Swqos,
    };
    let signed = envelope.sign(&[], &[&payer])?;
    let directory = tempfile::tempdir()?;
    let journal = Journal::open(directory.path())?;
    let result = submit_once(&helius, &journal, "test-intent", &signed, 123).await;
    if mode == "simulation_failure" {
        assert!(result.is_err());
        assert_eq!(std::fs::read_dir(directory.path())?.count(), 0);
        return Ok(());
    }
    if mode == "partial_failure" {
        assert_eq!(result?, signed.signature());
    } else {
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("unresolved in every region")
        );
    }
    let entries = std::fs::read_dir(directory.path())?.collect::<Result<Vec<_>, _>>()?;
    assert_eq!(entries.len(), 1, "one journal intent for all regions");
    let intent = entries[0].path();
    let record: Value = serde_json::from_reader(std::fs::File::open(intent.join("signed.json"))?)?;
    assert_eq!(record["wire_base64"], STANDARD.encode(signed.wire()?));
    let report: Value = serde_json::from_reader(std::fs::File::open(intent.join("fanout.json"))?)?;
    assert_eq!(report["signature"], signed.signature().to_string());
    assert_eq!(report["wire_sha256"], record["wire_sha256"]);
    let regions = report["regions"].as_array().unwrap();
    assert_eq!(regions.len(), 7);
    let confirmed = regions.iter().filter(|r| r["confirmed"] == true).count();
    assert_eq!(confirmed, if mode == "partial_failure" { 5 } else { 0 });
    assert_eq!(
        intent.join("confirmed.txt").exists(),
        mode == "partial_failure"
    );
    // Even a newly signed replacement cannot rebroadcast an already reserved intent.
    envelope.blockhash = Hash::new_unique();
    let changed = envelope.sign(&[], &[&payer])?;
    assert_ne!(signed.signature(), changed.signature());
    let retry_error = submit_once(&helius, &journal, "test-intent", &changed, 123)
        .await
        .unwrap_err();
    assert!(
        retry_error.to_string().contains("intent already reserved"),
        "replacement failed for an unexpected reason: {retry_error:#}"
    );
    Ok(())
}
