//! Plain HTTP receiver for X Money sender, amount and memo notifications.
use anyhow::{Context, Result, bail, ensure};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Request, State, rejection::JsonRejection},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    net::SocketAddr,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::Semaphore;

const MAX_BODY: usize = 64 * 1024;
const MAX_STORAGE: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Payment {
    sender: String,
    amount: Value,
    memo: String,
}
impl Payment {
    fn validate(&self) -> Result<()> {
        ensure!(!self.sender.trim().is_empty(), "sender must not be empty");
        // arbitrary_precision preserves decimal JSON numbers without an f64 round trip.
        let amount = match &self.amount {
            Value::Number(n) => n.to_string(),
            Value::String(s) => s.clone(),
            _ => bail!("amount must be a dollar amount, for example 25.50"),
        };
        let (whole, fraction) = amount.split_once('.').unwrap_or((&amount, ""));
        ensure!(
            !whole.is_empty()
                && whole.bytes().all(|b| b.is_ascii_digit())
                && fraction.len() <= 2
                && fraction.bytes().all(|b| b.is_ascii_digit())
                && (!amount.contains('.') || !fraction.is_empty())
                && amount.bytes().any(|b| matches!(b, b'1'..=b'9')),
            "amount must be positive dollars with at most two decimal places"
        );
        Ok(())
    }
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredPayment {
    sequence: u64,
    received_at: String,
    payment: Payment,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StateFile {
    version: u8,
    next_sequence: u64,
    transactions: Vec<StoredPayment>,
}

impl StateFile {
    fn validate(&self) -> Result<()> {
        ensure!(self.version == 1, "unsupported receiver state version");
        ensure!(
            self.next_sequence == self.transactions.len() as u64 + 1,
            "receiver state sequence is inconsistent"
        );
        for (index, record) in self.transactions.iter().enumerate() {
            ensure!(
                record.sequence == index as u64 + 1,
                "receiver state order is inconsistent"
            );
            chrono::DateTime::parse_from_rfc3339(&record.received_at)?;
            record.payment.validate()?;
        }
        Ok(())
    }
}

// The original receiver wrote these flat files. Import them once; retain originals.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyPayment {
    received_at: String,
    sender: String,
    amount: Value,
    memo: String,
}

fn import_legacy(root: &Path) -> Result<StateFile> {
    let mut records = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("payment-") || !name.ends_with(".json") {
            continue;
        }
        ensure!(
            entry.file_type()?.is_file(),
            "legacy payment must be a regular file"
        );
        let record: LegacyPayment = serde_json::from_reader(File::open(entry.path())?)
            .with_context(|| format!("invalid saved payment: {name}"))?;
        let timestamp = chrono::DateTime::parse_from_rfc3339(&record.received_at)?;
        records.push((timestamp, name, record));
    }
    records.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    let transactions: Vec<_> = records
        .into_iter()
        .enumerate()
        .map(|(i, (_, _, r))| StoredPayment {
            sequence: i as u64 + 1,
            received_at: r.received_at,
            payment: Payment {
                sender: r.sender,
                amount: r.amount,
                memo: r.memo,
            },
        })
        .collect();
    let state = StateFile {
        version: 1,
        next_sequence: transactions.len() as u64 + 1,
        transactions,
    };
    state.validate()?;
    Ok(state)
}

struct Store {
    root: PathBuf,
    state: StateFile,
    _owner: File,
}
impl Store {
    fn open(root: PathBuf) -> Result<Self> {
        fs::create_dir_all(&root)?;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        let owner = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join("state.lock"))?;
        owner.set_permissions(fs::Permissions::from_mode(0o600))?;
        owner
            .try_lock()
            .context("receiver state is already in use or cannot be locked")?;
        let (state, initialize) = match File::open(root.join("state.json")) {
            Ok(file) => (
                serde_json::from_reader::<_, StateFile>(file)
                    .context("state.json is invalid; refusing to reset saved payments")?,
                false,
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (import_legacy(&root)?, true),
            Err(e) => return Err(e.into()),
        };
        state.validate()?;
        let mut store = Self {
            root,
            state,
            _owner: owner,
        };
        if initialize {
            store.replace(store.state.clone())?;
        }
        // Persist the storage-directory entry as well as its state file.
        if let Some(parent) = store.root.parent().filter(|p| !p.as_os_str().is_empty()) {
            File::open(parent)?.sync_all()?;
        }
        Ok(store)
    }
    fn replace(&mut self, next: StateFile) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(&next)?;
        ensure!(
            bytes.len() as u64 <= MAX_STORAGE,
            "payment storage limit reached"
        );
        let mut file = tempfile::Builder::new()
            .prefix(".state-")
            .suffix(".tmp")
            .tempfile_in(&self.root)?;
        file.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(&bytes)?;
        file.as_file().sync_all()?;
        file.persist(self.root.join("state.json"))?;
        // Once renamed, memory must track the published file even if directory fsync fails.
        self.state = next;
        File::open(&self.root)?.sync_all()?;
        Ok(())
    }
    fn save(&mut self, payment: Payment) -> Result<()> {
        payment.validate()?;
        let mut next = self.state.clone();
        next.transactions.push(StoredPayment {
            sequence: next.next_sequence,
            received_at: chrono::Utc::now().to_rfc3339(),
            payment,
        });
        next.next_sequence = next
            .next_sequence
            .checked_add(1)
            .context("payment sequence overflow")?;
        self.replace(next)
    }
}
struct AppState {
    store: Mutex<Store>,
    inflight: Semaphore,
}
fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({"ok":false,"error":message}))).into_response()
}
async fn limit_upload(State(state): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let Ok(_permit) = state.inflight.try_acquire() else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "busy; try again shortly");
    };
    match tokio::time::timeout(Duration::from_secs(60), next.run(req)).await {
        Ok(response) => response,
        Err(_) => error(StatusCode::REQUEST_TIMEOUT, "upload timed out"),
    }
}
async fn ingest(
    State(state): State<Arc<AppState>>,
    body: std::result::Result<Json<Payment>, JsonRejection>,
) -> Response {
    let payment = match body {
        Ok(Json(p)) => p,
        Err(e) => return error(e.status(), "send JSON with sender, amount and memo"),
    };
    if let Err(e) = payment.validate() {
        return error(StatusCode::UNPROCESSABLE_ENTITY, &e.to_string());
    }
    let result = tokio::task::spawn_blocking(move || {
        state
            .store
            .lock()
            .map_err(|_| anyhow::anyhow!("storage lock poisoned"))?
            .save(payment)
    })
    .await;
    match result {
        Ok(Ok(())) => Json(json!({"ok":true})).into_response(),
        failure => {
            eprintln!("X Money storage failure: {failure:?}");
            error(StatusCode::INTERNAL_SERVER_ERROR, "could not save payment")
        }
    }
}
#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 3 {
        bail!("usage: capture_ingest <bind-address> <storage-dir>");
    }
    let addr: SocketAddr = args[1].parse()?;
    let store = Store::open(PathBuf::from(&args[2]))?;
    println!(
        "Recovered {} X Money records; next sequence {}",
        store.state.transactions.len(),
        store.state.next_sequence
    );
    let state = Arc::new(AppState {
        store: Mutex::new(store),
        inflight: Semaphore::new(2),
    });
    let app = Router::new()
        .route("/transactions", post(ingest))
        .route_layer(middleware::from_fn_with_state(state.clone(), limit_upload))
        .route("/healthz", get(|| async { Json(json!({"ok":true})) }))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .with_state(state);
    let handle = axum_server::Handle::new();
    let shutdown = handle.clone();
    tokio::spawn(async move {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler");
        tokio::select! { _ = term.recv() => {}, _ = tokio::signal::ctrl_c() => {} }
        shutdown.graceful_shutdown(Some(Duration::from_secs(65)));
    });
    println!("X Money receiver listening at http://{addr}; POST /transactions");
    axum_server::bind(addr)
        .handle(handle)
        .serve(app.into_make_service())
        .await?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preserves_decimal_amount_and_memo_without_rounding() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut store = Store::open(dir.path().into())?;
        let input = r#"{"sender":"@alice","amount":90071992547409.91,"memo":"token wallet\n🪙"}"#;
        store.save(serde_json::from_str(input)?)?;
        let path = dir.path().join("state.json");
        let state: Value = serde_json::from_reader(File::open(&path)?)?;
        let record = &state["transactions"][0]["payment"];
        assert_eq!(record["amount"].to_string(), "90071992547409.91");
        assert_eq!(record["sender"], "@alice");
        assert_eq!(record["memo"], "token wallet\n🪙");
        assert_eq!(fs::metadata(path)?.permissions().mode() & 0o777, 0o600);
        for invalid in ["-1", "0", "0.001", "1.005", "null", "true"] {
            let p: Payment = serde_json::from_str(&format!(
                r#"{{"sender":"@alice","amount":{invalid},"memo":""}}"#
            ))?;
            assert!(p.validate().is_err());
        }
        Ok(())
    }

    #[test]
    fn restart_preserves_order_and_ignores_interrupted_temporary_write() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let payment = || Payment {
            sender: "@alice".into(),
            amount: json!("12.34"),
            memo: "memo".into(),
        };
        let mut store = Store::open(dir.path().into())?;
        store.save(payment())?;
        assert!(
            Store::open(dir.path().into()).is_err(),
            "second writer must be locked out"
        );
        drop(store);
        fs::write(
            dir.path().join(".state-interrupted.tmp"),
            b"{\"transactions\":[",
        )?;
        let mut recovered = Store::open(dir.path().into())?;
        assert_eq!(recovered.state.next_sequence, 2);
        assert_eq!(recovered.state.transactions[0].payment.memo, "memo");
        recovered.save(payment())?;
        drop(recovered);
        let recovered = Store::open(dir.path().into())?;
        assert_eq!(recovered.state.transactions.len(), 2);
        assert_eq!(recovered.state.transactions[1].sequence, 2);
        assert_eq!(recovered.state.next_sequence, 3);
        drop(recovered);
        fs::write(dir.path().join("state.json"), b"{invalid")?;
        assert!(
            Store::open(dir.path().into()).is_err(),
            "invalid committed state must not reset"
        );
        assert_eq!(fs::read(dir.path().join("state.json"))?, b"{invalid");
        Ok(())
    }

    #[test]
    fn imports_existing_payment_files_once_in_timestamp_order() -> Result<()> {
        let dir = tempfile::tempdir()?;
        for (name, timestamp, sender) in [
            ("payment-a.json", "2026-09-22T01:00:02Z", "@later"),
            ("payment-z.json", "2026-09-22T01:00:01Z", "@earlier"),
        ] {
            fs::write(
                dir.path().join(name),
                serde_json::to_vec(&json!({
                    "received_at":timestamp, "sender":sender, "amount":"1.00", "memo":""
                }))?,
            )?;
        }
        let store = Store::open(dir.path().into())?;
        assert_eq!(store.state.transactions[0].payment.sender, "@earlier");
        assert_eq!(store.state.transactions[1].payment.sender, "@later");
        drop(store);
        let store = Store::open(dir.path().into())?;
        assert_eq!(store.state.transactions.len(), 2);
        assert_eq!(store.state.next_sequence, 3);
        assert!(dir.path().join("payment-a.json").exists());
        Ok(())
    }
}
