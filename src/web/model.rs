use super::pinata::Pinned;
use anyhow::{Context, Result, bail, ensure};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use solana_sdk::{
    pubkey::Pubkey,
    signature::{Keypair, Signer},
};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    str::FromStr,
    sync::LazyLock,
};

pub const CAP_CENTS: u64 = 1_200_000;
// Temporary $1 creation fee, until the operator requests restoring $10.
pub const FEE_CENTS: u64 = 100;
pub const DURATION: i64 = 3600;

pub fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().context("state parent")?;
    fs::create_dir_all(parent)?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".state-")
        .tempfile_in(parent)?;
    tmp.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    tmp.write_all(&serde_json::to_vec_pretty(value)?)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}
pub fn digest(value: &impl Serialize) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(value)?)))
}
pub fn cents(amount: &Value) -> Result<u64> {
    let text = match amount {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => bail!("invalid amount"),
    };
    let (whole, fraction) = text.split_once('.').unwrap_or((&text, ""));
    ensure!(
        !whole.is_empty()
            && whole.bytes().all(|c| c.is_ascii_digit())
            && fraction.len() <= 2
            && fraction.bytes().all(|c| c.is_ascii_digit()),
        "invalid dollar amount"
    );
    let fraction = match fraction.len() {
        0 => 0,
        1 => fraction.parse::<u64>()? * 10,
        _ => fraction.parse()?,
    };
    let total = whole
        .parse::<u64>()?
        .checked_mul(100)
        .and_then(|v| v.checked_add(fraction))
        .context("amount too large")?;
    ensure!(total > 0, "amount must be positive");
    Ok(total)
}
#[derive(Clone, Default, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Socials {
    pub website: String,
    pub twitter: String,
    pub telegram: String,
}
impl Socials {
    pub fn validate(&self) -> Result<()> {
        for (kind, value) in [
            ("website", &self.website),
            ("twitter", &self.twitter),
            ("telegram", &self.telegram),
        ] {
            if value.is_empty() {
                continue;
            }
            ensure!(value.len() <= 512, "social URL too long");
            let url = url::Url::parse(value).context("social link must be a full URL")?;
            ensure!(
                matches!(url.scheme(), "https" | "http")
                    && url.host_str().is_some()
                    && url.username().is_empty()
                    && url.password().is_none(),
                "social link must be HTTP(S)"
            );
            let host = url.host_str().unwrap();
            if kind == "twitter" {
                ensure!(
                    matches!(
                        host,
                        "x.com" | "www.x.com" | "twitter.com" | "www.twitter.com"
                    ),
                    "X link must use x.com or twitter.com"
                );
            }
            if kind == "telegram" {
                ensure!(
                    matches!(host, "t.me" | "telegram.me" | "www.t.me"),
                    "Telegram link must use t.me or telegram.me"
                );
            }
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchConfig {
    pub name: String,
    pub symbol: String,
    pub image_uri: String,
    #[serde(default)]
    pub socials: Socials,
}
impl LaunchConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.name.trim().is_empty()
                && self.name.len() <= 32
                && !self.name.chars().any(char::is_control),
            "name must be 1–32 UTF-8 bytes"
        );
        ensure!(
            !self.symbol.is_empty()
                && self.symbol.len() <= 10
                && self.symbol.bytes().all(|v| v.is_ascii_alphanumeric()),
            "symbol must be 1–10 letters or digits"
        );
        ensure!(self.image_uri.len() <= 512, "image URI too long");
        let uri = url::Url::parse(&self.image_uri).context("invalid image URI")?;
        ensure!(
            matches!(uri.scheme(), "ipfs" | "https" | "http") && uri.host_str().is_some(),
            "image URI must use ipfs or HTTP(S)"
        );
        self.socials.validate()
    }
    pub fn from_note(note: &str) -> Result<Self> {
        if note.trim_start().starts_with('{') {
            let config: Self = serde_json::from_str(note)?;
            config.validate()?;
            return Ok(config);
        }
        let mut fields = BTreeMap::new();
        for line in note.lines().filter(|line| !line.trim().is_empty()) {
            // The received X Money memo may flatten line breaks into spaces.
            // Recognize explicit field labels at whitespace boundaries, retaining
            // spaces inside values and rejecting duplicate/ambiguous labels.
            let mut starts = Vec::new();
            let mut boundary = true;
            for (offset, ch) in line.char_indices() {
                if boundary && !ch.is_whitespace() {
                    for key in [
                        "name",
                        "symbol",
                        "metadata uri",
                        "image uri",
                        "website",
                        "x",
                        "twitter",
                        "telegram",
                    ] {
                        let rest = &line[offset..];
                        if rest
                            .get(..key.len())
                            .is_some_and(|s| s.eq_ignore_ascii_case(key))
                            && let Some(value) = rest[key.len()..].trim_start().strip_prefix(':')
                        {
                            starts.push((offset, line.len() - value.len(), key));
                            break;
                        }
                    }
                }
                boundary = ch.is_whitespace();
            }
            let first = starts
                .first()
                .context("launch note must contain Name, Symbol and Metadata Uri")?;
            ensure!(
                line[..first.0].trim().is_empty(),
                "unknown launch note field"
            );
            for (index, &(_, value_start, key)) in starts.iter().enumerate() {
                let end = starts.get(index + 1).map_or(line.len(), |next| next.0);
                let value = line[value_start..end].trim().to_string();
                ensure!(
                    fields.insert(key.to_string(), value).is_none(),
                    "duplicate launch note field"
                );
            }
        }
        ensure!(
            !(fields.contains_key("x") && fields.contains_key("twitter")),
            "duplicate X link"
        );
        ensure!(
            !(fields.contains_key("metadata uri") && fields.contains_key("image uri")),
            "provide Metadata Uri only, not both Metadata Uri and Image Uri"
        );
        let config = Self {
            name: fields.remove("name").context("Name is required")?,
            symbol: fields
                .remove("symbol")
                .context("Symbol is required")?
                .to_ascii_uppercase(),
            image_uri: fields
                .remove("metadata uri")
                .or_else(|| fields.remove("image uri"))
                .context("Metadata Uri is required")?,
            socials: Socials {
                website: fields.remove("website").unwrap_or_default(),
                twitter: fields
                    .remove("x")
                    .or_else(|| fields.remove("twitter"))
                    .unwrap_or_default(),
                telegram: fields.remove("telegram").unwrap_or_default(),
            },
        };
        config.validate()?;
        Ok(config)
    }
    pub fn metadata(&self) -> Value {
        let mut metadata = json!({"name":self.name,"symbol":self.symbol,"description":"","image":self.image_uri,"showName":true});
        for (key, value) in [
            ("website", &self.socials.website),
            ("twitter", &self.socials.twitter),
            ("telegram", &self.socials.telegram),
        ] {
            if !value.is_empty() {
                metadata[key] = json!(value);
            }
        }
        if !self.socials.website.is_empty() {
            metadata["external_url"] = json!(self.socials.website);
        }
        metadata
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Incoming {
    pub sequence: u64,
    pub received_at: String,
    pub payment: IncomingPayment,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IncomingPayment {
    pub sender: String,
    pub amount: Value,
    pub memo: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Inbox {
    pub version: u8,
    pub next_sequence: u64,
    pub transactions: Vec<Incoming>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Buy {
    pub sequence: u64,
    pub received_at: i64,
    pub wallet: String,
    pub accepted_cents: u64,
    pub unallocated_cents: u64,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Coin {
    pub mint: String,
    // Only private durable state contains the signing key. Public views are explicit projections.
    pub mint_keypair: Vec<u8>,
    pub creator_sender: String,
    pub launch_sequence: u64,
    pub created_at: i64,
    pub deadline: i64,
    pub config: LaunchConfig,
    pub metadata: Option<Pinned>,
    pub buys: Vec<Buy>,
    /// Frozen display curve parameters; never replace these after accepting buys.
    #[serde(default)]
    pub market_opening: Option<crate::state::ConfigSnapshot>,
}
impl Coin {
    fn reserve(record: &Incoming, config: LaunchConfig, metadata: Option<Pinned>) -> Result<Self> {
        let created_at = chrono::DateTime::parse_from_rfc3339(&record.received_at)?.timestamp();
        let deadline = created_at
            .checked_add(DURATION)
            .context("deadline overflow")?;
        let key = Keypair::new();
        Ok(Self {
            mint: key.pubkey().to_string(),
            mint_keypair: key.to_bytes().to_vec(),
            creator_sender: record.payment.sender.clone(),
            launch_sequence: record.sequence,
            created_at,
            deadline,
            config,
            metadata,
            buys: vec![],
            market_opening: None,
        })
    }
    pub fn raised(&self) -> u64 {
        self.buys.iter().map(|b| b.accepted_cents).sum()
    }
    pub fn launch_request(
        &self,
        buyer: Pubkey,
        creator: Pubkey,
        funded_lamports: u64,
    ) -> Result<crate::launch::Launch> {
        let metadata = self
            .metadata
            .as_ref()
            .context("token metadata has not been pinned")?;
        Ok(crate::launch::Launch {
            mint: Pubkey::from_str(&self.mint)?,
            buyer,
            creator,
            name: self.config.name.clone(),
            symbol: self.config.symbol.clone(),
            metadata_uri: metadata.uri.clone(),
            quote_asset: crate::state::QuoteAsset::Sol,
            quote_budget: funded_lamports,
        })
    }
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Processed {
    pub sequence: u64,
    pub hash: String,
    pub kind: String,
    pub mint: Option<String>,
    pub unallocated_cents: u64,
    pub reason: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct PreparedLaunch {
    pub config: LaunchConfig,
    pub metadata: Pinned,
}
impl PreparedLaunch {
    fn validate(&self, uri: &str) -> Result<()> {
        self.config.validate()?;
        ensure!(
            uri == self.metadata.uri
                && uri == format!("ipfs://{}", self.metadata.cid)
                && !self.metadata.cid.is_empty()
                && self.metadata.cid.len() <= 120
                && self.metadata.cid.bytes().all(|c| c.is_ascii_alphanumeric())
                && self.config.image_uri != uri,
            "invalid prepared metadata reference"
        );
        Ok(())
    }
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Catalog {
    pub version: u8,
    pub coins: BTreeMap<String, Coin>,
    pub processed: Vec<Processed>,
    #[serde(default)]
    pub prepared: BTreeMap<String, PreparedLaunch>,
}
impl Catalog {
    /// Builds the note a buyer must send, and proves it parses back to exactly
    /// the requested token and wallet using the same parser the indexer runs on
    /// arrival. A note this returns therefore cannot be misattributed, and an
    /// address that would be ambiguous is refused here rather than on payment.
    pub fn buy_note(&self, mint: &str, wallet: &str) -> Result<String> {
        let note = format!("{mint} {wallet}");
        let (resolved_mint, resolved_wallet) = self.buy_target(&note)?;
        ensure!(
            resolved_mint == mint && resolved_wallet.to_string() == wallet,
            "note does not resolve to the requested token and wallet"
        );
        Ok(note)
    }

    /// The memo contains exactly two whitespace-separated addresses. Identify
    /// the token by catalog membership, never by its position in the note.
    fn buy_target(&self, note: &str) -> Result<(String, Pubkey)> {
        static PAIR: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"\A\s*([1-9A-HJ-NP-Za-km-z]+)\s+([1-9A-HJ-NP-Za-km-z]+)\s*\z")
                .expect("static buy memo regex")
        });
        let pair = PAIR
            .captures(note)
            .context("buy memo must contain exactly two whitespace-separated Solana addresses")?;
        // Regex checks the textual shape; the SDK checks exact 32-byte decoding.
        let first = Pubkey::from_str(&pair[1]).context("invalid Solana address in buy memo")?;
        let second = Pubkey::from_str(&pair[2]).context("invalid Solana address in buy memo")?;
        let (mint, wallet) = match (
            self.coins.contains_key(&pair[1]),
            self.coins.contains_key(&pair[2]),
        ) {
            (true, false) => (&pair[1], second),
            (false, true) => (&pair[2], first),
            (false, false) => bail!("neither address is a registered token"),
            (true, true) => bail!("ambiguous buy memo: both addresses are registered tokens"),
        };
        ensure!(wallet != Pubkey::default(), "invalid receiving wallet");
        Ok((mint.to_owned(), wallet))
    }
}
pub struct Store {
    pub root: PathBuf,
    pub data: Catalog,
    _owner: File,
    market_config: Option<(crate::state::ConfigSnapshot, i64)>,
}
impl Store {
    pub fn open(root: PathBuf) -> Result<Self> {
        fs::create_dir_all(&root)?;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        let owner = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join("catalog.lock"))?;
        owner.try_lock().context("catalog already owned")?;
        let data = match File::open(root.join("catalog.json")) {
            Ok(f) => serde_json::from_reader(f).context("invalid catalog; refusing reset")?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Catalog {
                version: 1,
                coins: BTreeMap::new(),
                processed: vec![],
                prepared: BTreeMap::new(),
            },
            Err(e) => return Err(e.into()),
        };
        let store = Self {
            root,
            data,
            _owner: owner,
            market_config: None,
        };
        store.validate()?;
        Ok(store)
    }
    fn validate(&self) -> Result<()> {
        ensure!(self.data.version == 1, "unsupported catalog version");
        for (uri, prepared) in &self.data.prepared {
            prepared.validate(uri)?;
        }
        for (i, p) in self.data.processed.iter().enumerate() {
            ensure!(p.sequence == i as u64 + 1, "catalog cursor inconsistent");
        }
        for (mint, c) in &self.data.coins {
            ensure!(
                mint == &c.mint
                    && Keypair::try_from(c.mint_keypair.as_slice())?
                        .pubkey()
                        .to_string()
                        == *mint,
                "reserved mint key mismatch"
            );
            c.config.validate()?;
            if let Some(opening) = &c.market_opening {
                crate::state::Config::from_snapshot(opening)?;
            }
            ensure!(
                c.deadline == c.created_at + DURATION && c.raised() <= CAP_CENTS,
                "invalid raise state"
            );
        }
        Ok(())
    }
    pub fn save_prepared(&mut self, config: LaunchConfig, metadata: Pinned) -> Result<()> {
        let prepared = PreparedLaunch { config, metadata };
        prepared.validate(&prepared.metadata.uri)?;
        if let Some(previous) = self.data.prepared.get(&prepared.metadata.uri) {
            ensure!(
                previous.config.metadata() == prepared.config.metadata(),
                "metadata URI already belongs to a different configuration"
            );
            return Ok(());
        }
        ensure!(
            !self.data.prepared.contains_key(&prepared.config.image_uri),
            "image must reference the uploaded image, not another metadata document"
        );
        let mut next = self.data.clone();
        next.prepared
            .insert(prepared.metadata.uri.clone(), prepared);
        self.commit(next)
    }
    fn resolve_config(&self, config: LaunchConfig) -> Result<(LaunchConfig, Option<Pinned>)> {
        if let Some(prepared) = self.data.prepared.get(&config.image_uri) {
            ensure!(
                config.name == prepared.config.name && config.symbol == prepared.config.symbol,
                "Name and Symbol must match the configuration saved in Metadata Uri"
            );
            ensure!(
                config.socials == Socials::default() || config.socials == prepared.config.socials,
                "social links conflict with the saved metadata"
            );
            return Ok((prepared.config.clone(), Some(prepared.metadata.clone())));
        }
        // Existing notes used a raw image URI and optional inline socials.
        Ok((config, None))
    }
    pub fn commit(&mut self, next: Catalog) -> Result<()> {
        let path = self.root.join("catalog.json");
        let result = atomic_json(&path, &next);
        if result.is_ok() {
            self.data = next;
        } else {
            // A rename may have succeeded before directory sync failed. Reload the
            // published file so a retry cannot replace its already reserved mint.
            match File::open(&path) {
                Ok(file) => {
                    self.data = serde_json::from_reader(file)?;
                    self.validate()?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        result
    }
    /// Attach verified parameters to new/empty raises. A funded legacy raise
    /// without a snapshot has unknown historical parameters and stays unpriced.
    pub fn set_market_config(&mut self, config: crate::state::ConfigSnapshot) -> Result<bool> {
        crate::state::Config::from_snapshot(&config)?;
        let mut next = self.data.clone();
        let mut changed = false;
        for coin in next.coins.values_mut() {
            if coin.market_opening.is_none() && coin.raised() == 0 {
                coin.market_opening = Some(config.clone());
                changed = true;
            }
        }
        if changed {
            self.commit(next)?;
        }
        self.market_config = Some((config, chrono::Utc::now().timestamp() + 120));
        Ok(changed)
    }
    fn current_market_config(&self) -> Option<crate::state::ConfigSnapshot> {
        self.market_config
            .as_ref()
            .filter(|(_, expires)| chrono::Utc::now().timestamp() < *expires)
            .map(|(config, _)| config.clone())
    }
    pub fn ingest(&mut self, inbox: Inbox) -> Result<bool> {
        ensure!(
            inbox.version == 1 && inbox.next_sequence == inbox.transactions.len() as u64 + 1,
            "invalid inbox state"
        );
        ensure!(
            inbox.transactions.len() >= self.data.processed.len(),
            "inbox history was truncated"
        );
        for (index, record) in inbox.transactions.iter().enumerate() {
            ensure!(record.sequence == index as u64 + 1, "inbox sequence gap");
            if let Some(p) = self.data.processed.get(index) {
                ensure!(p.hash == digest(record)?, "processed inbox record changed");
            }
        }
        if inbox.transactions.len() == self.data.processed.len() {
            return Ok(false);
        }
        let mut next = self.data.clone();
        for record in inbox.transactions.into_iter().skip(next.processed.len()) {
            let amount = cents(&record.payment.amount)?;
            let mut result = Processed {
                sequence: record.sequence,
                hash: digest(&record)?,
                kind: "unmatched".into(),
                mint: None,
                unallocated_cents: amount,
                reason: None,
            };
            let timestamp = chrono::DateTime::parse_from_rfc3339(&record.received_at)?.timestamp();
            let note = record.payment.memo.trim();
            if note.starts_with('{') || note.to_ascii_lowercase().starts_with("name:") {
                match LaunchConfig::from_note(note).and_then(|c| self.resolve_config(c)) {
                    Ok((config, metadata)) if amount == FEE_CENTS => {
                        let mut coin = Coin::reserve(&record, config, metadata)?;
                        coin.market_opening = self.current_market_config();
                        let mint = coin.mint.clone();
                        next.coins.insert(mint.clone(), coin);
                        result.kind = "launch".into();
                        result.mint = Some(mint);
                        result.unallocated_cents = 0;
                    }
                    Ok(_) => {
                        result.reason = Some(format!(
                            "launch payment must be exactly ${}.{:02}",
                            FEE_CENTS / 100,
                            FEE_CENTS % 100
                        ));
                    }
                    Err(e) => result.reason = Some(e.to_string()),
                }
            } else {
                match next.buy_target(note) {
                    Ok((mint, wallet)) => {
                        let coin = next
                            .coins
                            .get_mut(&mint)
                            .context("registered token disappeared")?;
                        let accepted = if timestamp >= coin.created_at && timestamp < coin.deadline
                        {
                            amount.min(CAP_CENTS - coin.raised())
                        } else {
                            0
                        };
                        coin.buys.push(Buy {
                            sequence: record.sequence,
                            received_at: timestamp,
                            wallet: wallet.to_string(),
                            accepted_cents: accepted,
                            unallocated_cents: amount - accepted,
                        });
                        result.kind = "buy".into();
                        result.mint = Some(mint);
                        result.unallocated_cents = amount - accepted;
                        if accepted == 0 {
                            result.reason =
                                Some("raise closed or payment outside its time window".into());
                        }
                    }
                    Err(error) => result.reason = Some(error.to_string()),
                }
            }
            next.processed.push(result);
        }
        self.commit(next)?;
        Ok(true)
    }
    /// Explicit offline recovery of one rejected launch, never a new payment.
    pub fn recover_launch(&mut self, record: &Incoming) -> Result<String> {
        let index = usize::try_from(record.sequence.checked_sub(1).context("invalid sequence")?)?;
        let prior = self
            .data
            .processed
            .get(index)
            .context("payment has not been processed")?;
        ensure!(
            prior.sequence == record.sequence && prior.hash == digest(record)?,
            "source payment changed"
        );
        if prior.kind == "launch" {
            let mint = prior.mint.as_ref().context("launch missing mint")?;
            ensure!(
                self.data
                    .coins
                    .get(mint)
                    .is_some_and(|c| c.launch_sequence == record.sequence),
                "launch record mismatch"
            );
            return Ok(mint.clone());
        }
        let amount = cents(&record.payment.amount)?;
        ensure!(
            prior.kind == "unmatched" && prior.mint.is_none() && prior.unallocated_cents == amount,
            "payment is already allocated"
        );
        ensure!(
            amount == FEE_CENTS,
            "payment does not match the current launch fee"
        );
        let (config, metadata) =
            self.resolve_config(LaunchConfig::from_note(&record.payment.memo)?)?;
        ensure!(
            metadata.is_some(),
            "recovery requires a known prepared metadata URI"
        );
        ensure!(
            !self
                .data
                .coins
                .values()
                .any(|c| c.launch_sequence == record.sequence),
            "payment already has a reserved mint"
        );
        let mut coin = Coin::reserve(record, config, metadata)?;
        coin.market_opening = self.current_market_config();
        let mint = coin.mint.clone();
        let mut next = self.data.clone();
        next.coins.insert(mint.clone(), coin);
        let processed = &mut next.processed[index];
        processed.kind = "launch".into();
        processed.mint = Some(mint.clone());
        processed.unallocated_cents = 0;
        processed.reason = None;
        self.commit(next)?;
        Ok(mint)
    }
}
