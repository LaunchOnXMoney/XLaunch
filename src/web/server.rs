use super::{
    market,
    model::{self, CAP_CENTS, Catalog, DURATION, FEE_CENTS, Inbox, LaunchConfig, Store},
    pinata::Pinata,
};
use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Query, Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
};
use lru::LruCache;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::Cursor,
    num::NonZeroUsize,
    path::{Path as FsPath, PathBuf},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime},
};

#[derive(Clone)]
pub struct Cached {
    body: Bytes,
    etag: String,
    mime: &'static str,
    policy: &'static str,
}
impl Cached {
    fn bytes(bytes: Vec<u8>, mime: &'static str, policy: &'static str) -> Self {
        let etag = format!("\"{:x}\"", Sha256::digest(&bytes));
        Self {
            body: bytes.into(),
            etag,
            mime,
            policy,
        }
    }
    fn json(value: &impl Serialize) -> Result<Self> {
        Ok(Self::bytes(
            serde_json::to_vec(value)?,
            "application/json",
            "public, max-age=1, must-revalidate",
        ))
    }
    fn response(&self, headers: &HeaderMap) -> Response {
        let unchanged = headers
            .get(header::IF_NONE_MATCH)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| {
                v.split(',')
                    .any(|t| t.trim() == self.etag || t.trim() == "*")
            });
        let mut response = if unchanged {
            StatusCode::NOT_MODIFIED.into_response()
        } else {
            self.body.clone().into_response()
        };
        for (name, value) in [
            (header::CONTENT_TYPE, self.mime),
            (header::CACHE_CONTROL, self.policy),
            (header::ETAG, &self.etag),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ] {
            response.headers_mut().insert(name, value.parse().unwrap());
        }
        response
    }
}
#[derive(Clone, Deserialize, Serialize)]
pub struct Config {
    pub receiver_handle: Option<String>,
    pub x_money_url: Option<String>,
    pub github_url: Option<String>,
    /// Origin of the separately hosted site, when the pages are not served here.
    /// Only this exact origin is granted cross-origin access to the API.
    #[serde(skip_serializing)]
    pub site_origin: Option<String>,
    pub launch_fee_cents: u64,
    pub cap_cents: u64,
    pub duration_seconds: i64,
}
/// Reads an optional public HTTPS link from the environment. Embedded
/// credentials are rejected because these links are published to every visitor.
fn optional_public_https_url(name: &str) -> Result<Option<String>> {
    let Ok(value) = std::env::var(name) else {
        return Ok(None);
    };
    let parsed = url::Url::parse(&value)?;
    ensure!(
        parsed.scheme() == "https"
            && parsed.host_str().is_some()
            && parsed.username().is_empty()
            && parsed.password().is_none(),
        "invalid {name}"
    );
    Ok(Some(value))
}
impl Config {
    pub fn from_env() -> Result<Self> {
        let receiver_handle = std::env::var("X_MONEY_HANDLE").ok();
        if let Some(handle) = &receiver_handle {
            ensure!(
                !handle.is_empty()
                    && handle
                        .trim_start_matches('@')
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || c == b'_'),
                "invalid X_MONEY_HANDLE"
            );
        }
        let x_money_url = optional_public_https_url("X_MONEY_URL")?;
        let github_url = optional_public_https_url("GITHUB_URL")?;
        let site_origin = match std::env::var("SITE_ORIGIN") {
            Err(_) => None,
            Ok(value) => {
                let parsed = url::Url::parse(&value).context("SITE_ORIGIN must be a URL")?;
                let loopback =
                    matches!(parsed.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
                ensure!(
                    (parsed.scheme() == "https" || (parsed.scheme() == "http" && loopback))
                        && parsed.host_str().is_some()
                        && parsed.username().is_empty()
                        && parsed.password().is_none()
                        && parsed.path() == "/"
                        && parsed.query().is_none()
                        && parsed.fragment().is_none(),
                    "SITE_ORIGIN must be a bare HTTPS origin, or http on loopback for local runs"
                );
                Some(parsed.origin().ascii_serialization())
            }
        };
        Ok(Self {
            receiver_handle,
            x_money_url,
            github_url,
            site_origin,
            launch_fee_cents: FEE_CENTS,
            cap_cents: CAP_CENTS,
            duration_seconds: DURATION,
        })
    }
}
#[derive(Clone, Serialize)]
pub struct TokenView {
    pub mint: String,
    pub name: String,
    pub ticker: String,
    pub image_uri: String,
    pub image_url: String,
    pub socials: model::Socials,
    pub metadata_uri: Option<String>,
    pub creator: String,
    pub created_at: i64,
    pub deadline: i64,
    pub status: String,
    pub raised_cents: u64,
    pub cap_cents: u64,
    pub backers: usize,
    pub fdv_usd: Option<f64>,
    pub market_cap_usd: Option<f64>,
    pub market_cap_source: Option<&'static str>,
    pub price_updated_at: Option<i64>,
    pub price_stale: bool,
    pub price_block: Option<u64>,
    pub volume_24h_usd: Option<f64>,
    pub launch_signature: Option<String>,
}
struct Snapshot {
    rows: BTreeMap<String, TokenView>,
    sorted: BTreeMap<String, Vec<String>>,
    details: BTreeMap<String, Cached>,
    metadata: BTreeMap<String, Cached>,
    stats: Cached,
    pages: Mutex<LruCache<String, Cached>>,
    next_deadline: Option<i64>,
}
impl Snapshot {
    fn new(
        catalog: &Catalog,
        jobs: &BTreeMap<String, crate::presale::Job>,
        now: i64,
        image_urls: &BTreeMap<String, String>,
        pinata: &Pinata,
        market: &market::Cache,
    ) -> Result<Self> {
        let mut rows = BTreeMap::new();
        let mut details = BTreeMap::new();
        let mut metadata = BTreeMap::new();
        let mut next_deadline = None;
        for (mint, coin) in &catalog.coins {
            let settlement = jobs.get(mint).and_then(|job| match &job.status {
                crate::presale::Status::Distributed { receipt } => Some(receipt),
                _ => None,
            });
            let status = if settlement.is_some() {
                "graduated"
            } else if coin.raised() >= CAP_CENTS || now >= coin.deadline {
                "awaiting_deployment"
            } else {
                "raising"
            };
            if status == "raising" {
                next_deadline =
                    Some(next_deadline.map_or(coin.deadline, |n: i64| n.min(coin.deadline)));
            }
            let price = market.quotes.get(mint).filter(|p| p.valid(now));
            let market_cap = if settlement.is_some() {
                price.map(|p| p.market_cap_usd)
            } else {
                market::curve_market_cap(coin)?
            };
            if settlement.is_some()
                && let Some(price) = price
            {
                let next = if price.stale(now) {
                    price.updated_at + market::EXPIRE_SECONDS
                } else {
                    price.updated_at + market::REFRESH_SECONDS
                };
                next_deadline = Some(next_deadline.map_or(next, |n| n.min(next)));
            }
            let row = TokenView {
                mint: mint.clone(),
                name: coin.config.name.clone(),
                ticker: coin.config.symbol.clone(),
                image_uri: coin.config.image_uri.clone(),
                image_url: image_urls
                    .get(&coin.config.image_uri)
                    .cloned()
                    .unwrap_or_else(|| pinata.browser_url(&coin.config.image_uri)),
                socials: coin.config.socials.clone(),
                metadata_uri: coin.metadata.as_ref().map(|p| p.uri.clone()),
                creator: coin.creator_sender.clone(),
                created_at: coin.created_at,
                deadline: coin.deadline,
                status: status.into(),
                raised_cents: coin.raised(),
                cap_cents: CAP_CENTS,
                backers: coin
                    .buys
                    .iter()
                    .filter(|b| b.accepted_cents > 0)
                    .map(|b| &b.wallet)
                    .collect::<BTreeSet<_>>()
                    .len(),
                fdv_usd: market_cap,
                market_cap_usd: market_cap,
                market_cap_source: market_cap.map(|_| {
                    if settlement.is_some() {
                        "jupiter"
                    } else {
                        "presale_curve"
                    }
                }),
                price_updated_at: if settlement.is_some() {
                    price.map(|p| p.updated_at)
                } else {
                    None
                },
                price_stale: settlement.is_some() && price.is_some_and(|p| p.stale(now)),
                price_block: if settlement.is_some() {
                    price.map(|p| p.price_block)
                } else {
                    None
                },
                volume_24h_usd: None,
                launch_signature: settlement.map(|s| s.launch_signature.clone()),
            };
            details.insert(
                mint.clone(),
                Cached::json(&json!({"token":row,"contributions":coin.buys}))?,
            );
            metadata.insert(mint.clone(), Cached::json(&coin.config.metadata())?);
            rows.insert(mint.clone(), row);
        }
        Self::assemble(rows, details, metadata, next_deadline)
    }
    fn assemble(
        rows: BTreeMap<String, TokenView>,
        details: BTreeMap<String, Cached>,
        metadata: BTreeMap<String, Cached>,
        next_deadline: Option<i64>,
    ) -> Result<Self> {
        let mut sorted = BTreeMap::new();
        for status in ["raising", "graduated"] {
            for sort in [
                "raised",
                "recent",
                "backers",
                "fdv",
                "marketcap",
                "volume24h",
            ] {
                let mut list: Vec<_> = rows
                    .values()
                    .filter(|r| (r.status == "graduated") == (status == "graduated"))
                    .collect();
                list.sort_by(|a, b| {
                    match sort {
                        "raised" => b.raised_cents.cmp(&a.raised_cents),
                        "backers" => b.backers.cmp(&a.backers),
                        "fdv" | "marketcap" => b
                            .fdv_usd
                            .partial_cmp(&a.fdv_usd)
                            .unwrap_or(std::cmp::Ordering::Equal),
                        "volume24h" => b
                            .volume_24h_usd
                            .partial_cmp(&a.volume_24h_usd)
                            .unwrap_or(std::cmp::Ordering::Equal),
                        _ => b.created_at.cmp(&a.created_at),
                    }
                    .then(b.created_at.cmp(&a.created_at))
                    .then(a.mint.cmp(&b.mint))
                });
                sorted.insert(
                    format!("{status}:{sort}"),
                    list.into_iter().map(|r| r.mint.clone()).collect(),
                );
            }
        }
        let graduated = rows.values().filter(|r| r.status == "graduated").count();
        let raised = rows
            .values()
            .try_fold(0_u64, |sum, row| sum.checked_add(row.raised_cents))
            .context("display total overflow")?;
        let stats = Cached::json(
            &json!({"raising":rows.len()-graduated,"graduated":graduated,"total":rows.len(),"raised_cents":raised}),
        )?;
        Ok(Self {
            rows,
            sorted,
            details,
            metadata,
            stats,
            pages: Mutex::new(LruCache::new(NonZeroUsize::new(256).unwrap())),
            next_deadline,
        })
    }
    fn page(&self, q: &ListQuery) -> Result<(Cached, bool)> {
        let key = serde_json::to_string(q)?;
        let mut pages = self.pages.lock().unwrap();
        if let Some(hit) = pages.get(&key) {
            return Ok((hit.clone(), true));
        }
        let sorted = &self.sorted[&format!("{}:{}", q.status, q.sort)];
        let search = q.q.to_lowercase();
        let list: Vec<_> = sorted
            .iter()
            .map(|k| &self.rows[k])
            .filter(|r| {
                search.is_empty()
                    || format!("{} {} {}", r.name, r.ticker, r.mint)
                        .to_lowercase()
                        .contains(&search)
            })
            .collect();
        let total = list.len();
        let pages_count = total.div_ceil(q.per_page).max(1);
        let page = q.page.min(pages_count);
        let selected: Vec<_> = list
            .into_iter()
            .skip((page - 1) * q.per_page)
            .take(q.per_page)
            .collect();
        let value = Cached::json(
            &json!({"tokens":selected,"page":page,"per_page":q.per_page,"pages":pages_count,"total":total,"cap_cents":CAP_CENTS}),
        )?;
        pages.put(key, value.clone());
        Ok((value, false))
    }
}
#[derive(Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ListQuery {
    status: String,
    sort: String,
    q: String,
    page: usize,
    per_page: usize,
}
impl Default for ListQuery {
    fn default() -> Self {
        Self {
            status: "graduated".into(),
            sort: "marketcap".into(),
            q: String::new(),
            page: 1,
            per_page: 6,
        }
    }
}
impl ListQuery {
    fn validate(&self) -> Result<()> {
        ensure!(
            matches!(self.status.as_str(), "raising" | "graduated")
                && matches!(
                    self.sort.as_str(),
                    "raised" | "recent" | "backers" | "fdv" | "marketcap" | "volume24h"
                ),
            "invalid status or sort"
        );
        ensure!(
            self.page > 0 && (1..=60).contains(&self.per_page) && self.q.len() <= 128,
            "invalid pagination or search"
        );
        Ok(())
    }
}

pub struct App {
    store: Mutex<Store>,
    source: PathBuf,
    source_stamp: Mutex<Option<(SystemTime, u64)>>,
    snapshot: RwLock<Arc<Snapshot>>,
    assets: RwLock<BTreeMap<String, Cached>>,
    image_urls: RwLock<BTreeMap<String, String>>,
    pinata: Arc<Pinata>,
    uploads: tokio::sync::Semaphore,
    config: Config,
    config_body: Cached,
    dirty: AtomicBool,
    source_reads: AtomicU64,
    rebuilds: AtomicU64,
    cache_hits: AtomicU64,
    cache_misses: AtomicU64,
    last_error: Mutex<Option<String>>,
    jobs_digest: Mutex<String>,
    market: RwLock<market::Cache>,
    market_initialized: AtomicBool,
}
pub fn failure(status: StatusCode, text: &str) -> Response {
    (status, Json(json!({"ok":false,"error":text}))).into_response()
}
/// Inlines the shared runtime and the page adapter into the page itself. Both
/// are inlined rather than linked because the page is served `no-cache` while a
/// linked script is cached for an hour: a visitor holding a stale copy of the
/// shared runtime would otherwise run fresh adapter code against missing
/// helpers.
/// A page of the site: its template, its adapter, the paths the live server
/// answers it on, and the file the static export writes it to. Both the server
/// and the exporter read this one table so they can never serve different sets.
pub struct Page {
    pub file: &'static str,
    pub logic: &'static str,
    pub aliases: &'static [&'static str],
    pub export_path: &'static str,
}

pub const PAGES: &[Page] = &[
    Page {
        file: "Token Launchpad.dc.html",
        logic: "home.js",
        aliases: &["/", "/index.html", "/Token Launchpad.dc.html"],
        export_path: "index.html",
    },
    Page {
        file: "Explore.dc.html",
        logic: "launchpad.js",
        aliases: &["/explore", "/explore/", "/Explore.dc.html"],
        export_path: "explore/index.html",
    },
    Page {
        file: "Deploy.dc.html",
        logic: "deploy.js",
        aliases: &["/deploy", "/Deploy.dc.html"],
        export_path: "deploy/index.html",
    },
    Page {
        file: "How It Works.dc.html",
        logic: "how-it-works.js",
        aliases: &["/how-it-works", "/how-it-works/", "/How It Works.dc.html"],
        export_path: "how-it-works/index.html",
    },
];

/// Image files the pages reference by absolute path. Served by the live server
/// and copied by the static export from this one list, so neither can drift.
pub struct StaticImage {
    pub published_path: &'static str,
    pub source_path: &'static str,
}

pub const STATIC_IMAGES: &[StaticImage] = &[
    StaticImage {
        published_path: "/logo-glass.png",
        source_path: "logo-glass.png",
    },
    StaticImage {
        published_path: "/profiles/profile-1.png",
        source_path: "profiles/profile-1.png",
    },
    StaticImage {
        published_path: "/profiles/profile-2.png",
        source_path: "profiles/profile-2.png",
    },
    StaticImage {
        published_path: "/profiles/profile-3.png",
        source_path: "profiles/profile-3.png",
    },
    StaticImage {
        published_path: "/profiles/profile-4.png",
        source_path: "profiles/profile-4.png",
    },
    StaticImage {
        published_path: "/profiles/profile-5.png",
        source_path: "profiles/profile-5.png",
    },
    StaticImage {
        published_path: "/profiles/profile-6.png",
        source_path: "profiles/profile-6.png",
    },
    StaticImage {
        published_path: "/profiles/profile-7.png",
        source_path: "profiles/profile-7.png",
    },
];

pub fn adapt_html(html: &str, runtime: &str, logic: &str) -> Result<String> {
    let start = html
        .find("<script type=\"text/x-dc\"")
        .context("missing logic block")?;
    let body = start + html[start..].find('>').context("missing script tag")? + 1;
    let end = body
        + html[body..]
            .find("</script>")
            .context("missing script close")?;
    let out = format!("{}\n{}\n{}", &html[..body], logic, &html[end..]);
    Ok(out.replace(
        "<script src=\"./support.js\"></script>",
        &format!("<script>{runtime}</script><script src=\"/support.js\"></script>"),
    ))
}
impl App {
    pub fn open(
        source: PathBuf,
        root: PathBuf,
        frontend: &FsPath,
        adapters: &FsPath,
        vendor: &FsPath,
        config: Config,
    ) -> Result<Arc<Self>> {
        let store = Store::open(root.clone())?;
        fs::create_dir_all(root.join("images"))?;
        fs::create_dir_all(root.join("settlements"))?;
        let mut assets = BTreeMap::new();
        for &Page {
            file,
            logic,
            aliases,
            ..
        } in PAGES
        {
            let html = adapt_html(
                &fs::read_to_string(frontend.join(file))?,
                &fs::read_to_string(adapters.join("runtime-config.js"))?,
                &fs::read_to_string(adapters.join(logic))?,
            )?;
            let cached = Cached::bytes(html.into_bytes(), "text/html; charset=utf-8", "no-cache");
            for alias in aliases {
                assets.insert((*alias).into(), cached.clone());
            }
        }
        for (name, path) in [
            ("/support.js", frontend.join("support.js")),
            (
                "/vendor/react.production.min.js",
                vendor.join("react.production.min.js"),
            ),
            (
                "/vendor/react-dom.production.min.js",
                vendor.join("react-dom.production.min.js"),
            ),
        ] {
            assets.insert(
                name.into(),
                Cached::bytes(
                    fs::read(path)?,
                    "application/javascript; charset=utf-8",
                    "public, max-age=3600",
                ),
            );
        }
        // A missing profile picture is skipped rather than fatal: the page falls
        // back to a lettered bubble for that slot.
        for image in STATIC_IMAGES {
            let path = frontend.join(image.source_path);
            if !path.exists() {
                continue;
            }
            assets.insert(
                image.published_path.into(),
                Cached::bytes(fs::read(&path)?, "image/png", "public, max-age=3600"),
            );
        }
        for entry in fs::read_dir(root.join("images"))? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.ends_with(".png") && !name.ends_with(".jpg") {
                continue;
            }
            let bytes = fs::read(entry.path())?;
            let mime = validate_image(&bytes)?;
            assets.insert(
                format!("/media/{name}"),
                Cached::bytes(bytes, mime, "public, max-age=31536000, immutable"),
            );
        }
        let mut pinata = Pinata::new(root)?;
        let image_urls = pinata
            .cached_image_hashes()
            .into_iter()
            .filter_map(|(uri, hash)| {
                ["png", "jpg"]
                    .into_iter()
                    .map(|ext| format!("/media/{hash}.{ext}"))
                    .find(|path| assets.contains_key(path))
                    .map(|path| (uri, path))
            })
            .collect();
        let snapshot = Snapshot::new(
            &store.data,
            &BTreeMap::new(),
            chrono::Utc::now().timestamp(),
            &image_urls,
            &pinata,
            &market::Cache::default(),
        )?;
        let config_body = Cached::json(&config)?;
        let app = Arc::new(Self {
            store: Mutex::new(store),
            source,
            source_stamp: Mutex::new(None),
            snapshot: RwLock::new(Arc::new(snapshot)),
            assets: RwLock::new(assets),
            image_urls: RwLock::new(image_urls),
            pinata: Arc::new(pinata),
            uploads: tokio::sync::Semaphore::new(2),
            config,
            config_body,
            dirty: AtomicBool::new(true),
            source_reads: AtomicU64::new(0),
            rebuilds: AtomicU64::new(0),
            cache_hits: AtomicU64::new(0),
            cache_misses: AtomicU64::new(0),
            last_error: Mutex::new(None),
            jobs_digest: Mutex::new(String::new()),
            market: RwLock::new(market::Cache::default()),
            market_initialized: AtomicBool::new(false),
        });
        app.refresh()?;
        Ok(app)
    }
    fn refresh(&self) -> Result<()> {
        let meta = fs::metadata(&self.source)?;
        let stamp = (meta.modified()?, meta.len());
        let mut previous = self.source_stamp.lock().unwrap();
        let mut store = self.store.lock().unwrap();
        let mut changed = self.dirty.swap(false, Ordering::Relaxed);
        // On startup, attempt live parameters before replaying newly received
        // launches and buys in the same inbox. Failure still permits ingestion;
        // those funded raises remain explicitly unpriced, never retrofitted.
        if previous.as_ref() != Some(&stamp) && self.market_initialized.load(Ordering::Acquire) {
            self.source_reads.fetch_add(1, Ordering::Relaxed);
            let inbox: Inbox = serde_json::from_reader(File::open(&self.source)?)?;
            changed |= store.ingest(inbox)?;
            *previous = Some(stamp);
        }
        let mut jobs = BTreeMap::new();
        for entry in fs::read_dir(store.root.join("settlements"))? {
            let path = entry?.path();
            if path.extension().is_none_or(|v| v != "json") {
                continue;
            }
            let job: crate::presale::Job = serde_json::from_reader(File::open(path)?)?;
            let mint = job.raise.mint.to_string();
            let coin = store
                .data
                .coins
                .get(&mint)
                .context("settlement for unknown token")?;
            ensure!(
                coin.metadata
                    .as_ref()
                    .is_some_and(|p| p.uri == job.raise.metadata_uri),
                "settlement metadata does not match pinned metadata"
            );
            if let crate::presale::Status::Distributed { receipt } = &job.status {
                ensure!(
                    receipt.tokens_received > 0
                        && receipt.tokens_received == receipt.tokens_distributed
                        && !receipt.launch_signature.is_empty(),
                    "incomplete distribution receipt"
                );
            }
            jobs.insert(mint, job);
        }
        let hash = model::digest(&jobs)?;
        let mut old_hash = self.jobs_digest.lock().unwrap();
        changed |= *old_hash != hash;
        let now = chrono::Utc::now().timestamp();
        changed |= self
            .snapshot
            .read()
            .unwrap()
            .next_deadline
            .is_some_and(|d| now >= d);
        if changed {
            let snapshot = Snapshot::new(
                &store.data,
                &jobs,
                now,
                &self.image_urls.read().unwrap(),
                &self.pinata,
                &self.market.read().unwrap(),
            )?;
            *self.snapshot.write().unwrap() = Arc::new(snapshot);
            *old_hash = hash;
            self.rebuilds.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }
    async fn refresh_markets(self: Arc<Self>) {
        let mut client = match market::PriceClient::new() {
            Ok(client) => client,
            Err(_) => {
                self.market.write().unwrap().health.last_error =
                    Some("price client initialization failed".into());
                self.market_initialized.store(true, Ordering::Release);
                return;
            }
        };
        let rpc_url = std::env::var("RPC_URL").unwrap_or_else(|_| market::DEFAULT_RPC_URL.into());
        let mut schedule = market::Schedule::default();
        let mut config_due = 0;
        loop {
            let now = chrono::Utc::now().timestamp();
            if now >= config_due {
                let state = self.clone();
                let rpc_url = rpc_url.clone();
                let result = tokio::task::spawn_blocking(move || -> Result<u64> {
                    let helius = helius::Helius::new_with_url(&rpc_url)
                        .map_err(|_| anyhow::anyhow!("invalid market RPC configuration"))?;
                    let config = crate::state::Config::fetch(&helius).map_err(|_| {
                        anyhow::anyhow!("could not verify current Pump curve configuration")
                    })?;
                    if state
                        .store
                        .lock()
                        .unwrap()
                        .set_market_config(config.snapshot())?
                    {
                        state.dirty.store(true, Ordering::Relaxed);
                    }
                    Ok(config.slot)
                })
                .await;
                let mut cache = self.market.write().unwrap();
                match result {
                    Ok(Ok(slot)) => {
                        cache.health.curve_config_slot = Some(slot);
                        cache.health.curve_config_error = None;
                    }
                    _ => {
                        cache.health.curve_config_error = Some(
                            "could not fetch, verify, or persist Pump curve configuration".into(),
                        )
                    }
                }
                config_due = chrono::Utc::now().timestamp() + market::REFRESH_SECONDS;
                self.market_initialized.store(true, Ordering::Release);
            }
            let mints: BTreeSet<_> = self
                .snapshot
                .read()
                .unwrap()
                .rows
                .values()
                .filter(|row| row.status == "graduated")
                .map(|row| row.mint.clone())
                .collect();
            {
                let mut cache = self.market.write().unwrap();
                cache.health.tracked_mints = mints.len();
                cache.quotes.retain(|mint, _| mints.contains(mint));
            }
            if client.ready() {
                let batch = schedule.next(&mints, chrono::Utc::now().timestamp());
                if !batch.is_empty() {
                    self.market.write().unwrap().health.price_requests += 1;
                    match client.prices(&batch).await {
                        Err(error) => {
                            self.market.write().unwrap().health.last_error = Some(error.to_string())
                        }
                        Ok(prices) => {
                            // Immediately clear prices Jupiter has withdrawn, even if RPC fails.
                            {
                                let mut cache = self.market.write().unwrap();
                                for mint in &batch {
                                    if prices.get(mint).is_none_or(|p| p.is_none()) {
                                        cache.quotes.remove(mint);
                                    }
                                }
                            }
                            self.dirty.store(true, Ordering::Relaxed);
                            let wanted: Vec<_> = batch
                                .iter()
                                .filter(|m| prices.get(*m).is_some_and(|p| p.is_some()))
                                .cloned()
                                .collect();
                            let supplies = if wanted.is_empty() {
                                Ok(BTreeMap::new())
                            } else {
                                self.market.write().unwrap().health.supply_requests += 1;
                                let rpc_url = rpc_url.clone();
                                match tokio::task::spawn_blocking(move || {
                                    market::supplies(&rpc_url, &wanted)
                                })
                                .await
                                {
                                    Ok(result) => result,
                                    Err(_) => Err(anyhow::anyhow!("mint supply worker failed")),
                                }
                            };
                            let now = chrono::Utc::now().timestamp();
                            let mut cache = self.market.write().unwrap();
                            match supplies {
                                Err(error) => cache.health.last_error = Some(error.to_string()),
                                Ok(supplies) => {
                                    let mut invalid = 0;
                                    for mint in &batch {
                                        let price = prices.get(mint).and_then(|p| p.as_ref());
                                        let quote = price.zip(supplies.get(mint)).and_then(
                                            |(p, supply)| market::Quote::new(p, *supply, now).ok(),
                                        );
                                        match quote {
                                            Some(quote)
                                                if cache.quotes.get(mint).is_none_or(|old| {
                                                    old.price_block <= quote.price_block
                                                        && old.supply_slot <= quote.supply_slot
                                                }) =>
                                            {
                                                cache.quotes.insert(mint.clone(), quote);
                                            }
                                            Some(_) => {
                                                invalid += 1;
                                            } // Never refresh a regressed response.
                                            None => {
                                                if price.is_some() {
                                                    invalid += 1;
                                                }
                                                cache.quotes.remove(mint);
                                            }
                                        }
                                    }
                                    cache.health.last_success_at = Some(now);
                                    cache.health.last_error = (invalid > 0).then(|| {
                                        format!(
                                            "{invalid} prices failed mint or freshness validation"
                                        )
                                    });
                                }
                            }
                            self.dirty.store(true, Ordering::Relaxed);
                        }
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
    pub fn start_workers(app: &Arc<Self>) {
        let state = app.clone();
        tokio::spawn(async move {
            state.refresh_markets().await;
        });
        let state = app.clone();
        tokio::spawn(async move {
            loop {
                let worker = state.clone();
                let result = tokio::task::spawn_blocking(move || worker.refresh()).await;
                let error = match result {
                    Ok(Ok(())) => None,
                    Ok(Err(e)) => Some(e.to_string()),
                    Err(e) => Some(e.to_string()),
                };
                if error.is_some() {
                    // Retry the snapshot even when the source stamp was already
                    // advanced before a later projection step failed.
                    state.dirty.store(true, Ordering::Relaxed);
                }
                {
                    let mut previous = state.last_error.lock().unwrap();
                    if error != *previous {
                        if let Some(e) = &error {
                            eprintln!("web projection: {e}");
                        }
                        *previous = error;
                    }
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
        let state = app.clone();
        tokio::spawn(async move {
            loop {
                let pending: Vec<_> = {
                    state
                        .store
                        .lock()
                        .unwrap()
                        .data
                        .coins
                        .values()
                        .filter(|c| c.metadata.is_none())
                        .map(|c| (c.mint.clone(), c.config.metadata()))
                        .collect()
                };
                for (mint, value) in pending {
                    match state.pinata.metadata(value).await {
                        Ok(pin) => {
                            let worker = state.clone();
                            let result = tokio::task::spawn_blocking(move || -> Result<()> {
                                let mut store = worker.store.lock().unwrap();
                                let mut next = store.data.clone();
                                next.coins.get_mut(&mint).context("missing mint")?.metadata =
                                    Some(pin);
                                store.commit(next)?;
                                worker.dirty.store(true, Ordering::Relaxed);
                                Ok(())
                            })
                            .await;
                            if !matches!(result, Ok(Ok(()))) {
                                eprintln!("failed to save pinned metadata");
                            }
                        }
                        Err(e) => eprintln!("metadata pinning: {e}"),
                    }
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });
    }
}
fn validate_image(bytes: &[u8]) -> Result<&'static str> {
    ensure!(
        !bytes.is_empty() && bytes.len() < 2 * 1024 * 1024,
        "image must be under 2 MB"
    );
    let mut reader = image::ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    let mime = match reader.format() {
        Some(image::ImageFormat::Png) => "image/png",
        Some(image::ImageFormat::Jpeg) => "image/jpeg",
        _ => anyhow::bail!("upload PNG or JPG"),
    };
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    let image = reader.decode().context("invalid image")?;
    ensure!(
        image.width() > 0 && image.width() == image.height(),
        "image must be square (1:1)"
    );
    Ok(mime)
}
async fn tokens(
    State(app): State<Arc<App>>,
    Query(q): Query<ListQuery>,
    headers: HeaderMap,
) -> Response {
    if let Err(e) = q.validate() {
        return failure(StatusCode::BAD_REQUEST, &e.to_string());
    }
    let snapshot = app.snapshot.read().unwrap().clone();
    match snapshot.page(&q) {
        Ok((cached, hit)) => {
            if hit {
                app.cache_hits.fetch_add(1, Ordering::Relaxed);
            } else {
                app.cache_misses.fetch_add(1, Ordering::Relaxed);
            }
            cached.response(&headers)
        }
        Err(_) => failure(StatusCode::INTERNAL_SERVER_ERROR, "could not render tokens"),
    }
}
async fn token(
    State(app): State<Arc<App>>,
    Path(mint): Path<String>,
    headers: HeaderMap,
) -> Response {
    let snapshot = app.snapshot.read().unwrap().clone();
    snapshot.details.get(&mint).map_or_else(
        || failure(StatusCode::NOT_FOUND, "token not found"),
        |v| v.response(&headers),
    )
}
async fn metadata(
    State(app): State<Arc<App>>,
    Path(mint): Path<String>,
    headers: HeaderMap,
) -> Response {
    let snapshot = app.snapshot.read().unwrap().clone();
    snapshot.metadata.get(&mint).map_or_else(
        || failure(StatusCode::NOT_FOUND, "token not found"),
        |v| v.response(&headers),
    )
}
async fn config(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    app.config_body.response(&headers)
}
async fn stats(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    app.snapshot.read().unwrap().stats.response(&headers)
}
async fn health(State(app): State<Arc<App>>) -> Response {
    let error = app.last_error.lock().unwrap();
    (if error.is_none(){StatusCode::OK}else{StatusCode::SERVICE_UNAVAILABLE},Json(json!({"ok":error.is_none(),"tokens":app.snapshot.read().unwrap().rows.len(),"source_reads":app.source_reads.load(Ordering::Relaxed),"cache_rebuilds":app.rebuilds.load(Ordering::Relaxed),"cache_hits":app.cache_hits.load(Ordering::Relaxed),"cache_misses":app.cache_misses.load(Ordering::Relaxed),"market":&app.market.read().unwrap().health}))).into_response()
}
async fn open_x(State(app): State<Arc<App>>) -> Response {
    match &app.config.x_money_url {
        Some(url) => Redirect::to(url).into_response(),
        None => failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "X Money destination has not been configured",
        ),
    }
}
async fn assets(State(app): State<Arc<App>>, uri: axum::http::Uri, headers: HeaderMap) -> Response {
    let decoded = match url::Url::parse(&format!("http://localhost{}", uri.path())) {
        Ok(v) => v,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    // Only exact registered public asset paths are served; never join request paths to disk.
    let path = decoded.path().replace("%20", " ");
    app.assets.read().unwrap().get(&path).map_or_else(
        || StatusCode::NOT_FOUND.into_response(),
        |a| a.response(&headers),
    )
}
async fn upload(State(app): State<Arc<App>>, body: Bytes) -> Response {
    let Ok(_permit) = app.uploads.try_acquire() else {
        return failure(
            StatusCode::TOO_MANY_REQUESTS,
            "another upload is in progress; try again",
        );
    };
    let image = body.to_vec();
    let checked =
        tokio::task::spawn_blocking(move || validate_image(&image).map(|mime| (image, mime))).await;
    let (bytes, mime) = match checked {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return failure(StatusCode::UNPROCESSABLE_ENTITY, &e.to_string()),
        Err(_) => return failure(StatusCode::INTERNAL_SERVER_ERROR, "image validation failed"),
    };
    let hash = format!("{:x}", Sha256::digest(&bytes));
    match app.pinata.image(&hash, bytes.clone(), mime).await {
        Ok(pin) => {
            let ext = if mime == "image/png" { "png" } else { "jpg" };
            let name = format!("{hash}.{ext}");
            let path = app.store.lock().unwrap().root.join("images").join(&name);
            let disk_bytes = bytes.clone();
            let saved = tokio::task::spawn_blocking(move || -> Result<()> {
                let parent = path.parent().unwrap();
                let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
                use std::io::Write;
                tmp.write_all(&disk_bytes)?;
                tmp.as_file().sync_all()?;
                tmp.persist(&path)?;
                File::open(parent)?.sync_all()?;
                Ok(())
            })
            .await;
            if !matches!(saved, Ok(Ok(()))) {
                return failure(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "could not save uploaded image",
                );
            }
            let local = format!("/media/{name}");
            app.assets.write().unwrap().insert(
                local.clone(),
                Cached::bytes(bytes, mime, "public, max-age=31536000, immutable"),
            );
            app.image_urls
                .write()
                .unwrap()
                .insert(pin.uri.clone(), local.clone());
            app.dirty.store(true, Ordering::Relaxed);
            Json(json!({"cid":pin.cid,"uri":pin.uri,"url":pin.url,"preview_url":local}))
                .into_response()
        }
        Err(e) => {
            eprintln!("image pinning: {e}");
            failure(StatusCode::BAD_GATEWAY, "Pinata upload failed; try again")
        }
    }
}
#[derive(Deserialize)]
struct BuyNoteRequest {
    mint: String,
    wallet: String,
}

/// Returns the exact note a buyer must put on their X Money payment. The note is
/// produced and checked by the catalog, so the page never assembles one itself.
async fn buy_note(State(app): State<Arc<App>>, Json(request): Json<BuyNoteRequest>) -> Response {
    let note = {
        let store = app.store.lock().unwrap();
        store.data.buy_note(&request.mint, &request.wallet)
    };
    match note {
        Ok(note) => Json(json!({ "note": note })).into_response(),
        Err(error) => failure(StatusCode::UNPROCESSABLE_ENTITY, &error.to_string()),
    }
}

async fn validate_config(Json(config): Json<LaunchConfig>) -> Response {
    match config.validate() {
        Ok(()) => Json(json!({"ok":true,"metadata":config.metadata()})).into_response(),
        Err(e) => failure(StatusCode::UNPROCESSABLE_ENTITY, &e.to_string()),
    }
}
async fn prepare_config(State(app): State<Arc<App>>, Json(config): Json<LaunchConfig>) -> Response {
    if let Err(e) = config.validate() {
        return failure(StatusCode::UNPROCESSABLE_ENTITY, &e.to_string());
    }
    if app
        .store
        .lock()
        .unwrap()
        .data
        .prepared
        .contains_key(&config.image_uri)
    {
        return failure(
            StatusCode::UNPROCESSABLE_ENTITY,
            "use the original uploaded image URI",
        );
    }
    let Ok(_permit) = app.uploads.try_acquire() else {
        return failure(
            StatusCode::TOO_MANY_REQUESTS,
            "another upload is in progress; try again",
        );
    };
    let metadata = config.metadata();
    let pin = match app.pinata.metadata(metadata.clone()).await {
        Ok(pin) => pin,
        Err(e) => {
            eprintln!("configuration pinning: {e}");
            return failure(
                StatusCode::BAD_GATEWAY,
                "could not save metadata to Pinata; try again",
            );
        }
    };
    let worker = app.clone();
    let saved_pin = pin.clone();
    let saved = tokio::task::spawn_blocking(move || {
        worker
            .store
            .lock()
            .unwrap()
            .save_prepared(config, saved_pin)
    })
    .await;
    if !matches!(saved, Ok(Ok(()))) {
        return failure(
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not save the metadata reference",
        );
    }
    Json(json!({"ok":true,"cid":pin.cid,"uri":pin.uri,"url":pin.url,"metadata":metadata}))
        .into_response()
}
/// Grants the separately hosted site cross-origin access to this API, and only
/// that one exact origin. A request carrying any other Origin is answered
/// without the header, so the browser refuses it. Preflight is answered here
/// because the API routes themselves accept only GET and POST.
async fn cross_origin(State(app): State<Arc<App>>, request: Request, next: Next) -> Response {
    let Some(allowed) = app.config.site_origin.clone() else {
        return next.run(request).await;
    };
    let matching = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|origin| origin == allowed);
    let preflight = request.method() == Method::OPTIONS;
    let mut response = if preflight {
        StatusCode::NO_CONTENT.into_response()
    } else {
        next.run(request).await
    };
    let headers = response.headers_mut();
    // Vary is set whether or not this request matched, so a shared cache cannot
    // serve one origin's permissive response to a different origin.
    headers.insert(header::VARY, HeaderValue::from_static("Origin"));
    if matching && let Ok(value) = HeaderValue::from_str(&allowed) {
        headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, value);
        headers.insert(
            header::ACCESS_CONTROL_EXPOSE_HEADERS,
            HeaderValue::from_static("etag"),
        );
        if preflight {
            headers.insert(
                header::ACCESS_CONTROL_ALLOW_METHODS,
                HeaderValue::from_static("GET, POST, OPTIONS"),
            );
            headers.insert(
                header::ACCESS_CONTROL_ALLOW_HEADERS,
                HeaderValue::from_static("content-type, if-none-match"),
            );
            headers.insert(
                header::ACCESS_CONTROL_MAX_AGE,
                HeaderValue::from_static("600"),
            );
        }
    }
    response
}

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/api/tokens", get(tokens))
        .route("/api/tokens/{mint}", get(token))
        .route("/api/tokens/{mint}/metadata", get(metadata))
        .route("/api/config", get(config))
        .route("/api/stats", get(stats))
        .route("/api/health", get(health))
        .route("/healthz", get(health))
        .route(
            "/api/images",
            post(upload).layer(DefaultBodyLimit::max(2 * 1024 * 1024)),
        )
        .route(
            "/api/launch-config/validate",
            post(validate_config).layer(DefaultBodyLimit::max(8192)),
        )
        .route(
            "/api/buy-note",
            post(buy_note).layer(DefaultBodyLimit::max(1024)),
        )
        .route(
            "/api/launch-config/prepare",
            post(prepare_config).layer(DefaultBodyLimit::max(8192)),
        )
        .route("/go/x-money", get(open_x))
        .fallback(get(assets))
        .layer(middleware::from_fn_with_state(app.clone(), cross_origin))
        .with_state(app)
}
