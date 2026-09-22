use super::model::{atomic_json, digest};
use anyhow::{Context, Result, ensure};
use reqwest::multipart::{Form, Part};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, fs::File, path::PathBuf, time::Duration};
use tokio::sync::Mutex;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Pinned {
    pub cid: String,
    pub uri: String,
    pub url: String,
}
#[derive(Deserialize)]
struct Reply {
    data: Uploaded,
}
#[derive(Deserialize)]
struct Uploaded {
    cid: String,
}
pub struct Pinata {
    http: reqwest::Client,
    jwt: String,
    gateway: String,
    cache: Mutex<BTreeMap<String, Pinned>>,
    path: PathBuf,
}
impl Pinata {
    pub fn new(root: PathBuf) -> Result<Self> {
        let jwt = std::env::var("PINATA_JWT").context("PINATA_JWT is required")?;
        ensure!(!jwt.trim().is_empty(), "PINATA_JWT is empty");
        let gateway = std::env::var("IPFS_GATEWAY").context("IPFS_GATEWAY is required")?;
        let gateway_url = url::Url::parse(&gateway)?;
        ensure!(
            gateway_url.scheme() == "https"
                && gateway_url.host_str().is_some()
                && gateway_url.username().is_empty()
                && gateway_url.password().is_none()
                && gateway_url.query().is_none()
                && gateway_url.fragment().is_none()
                && gateway.ends_with('/'),
            "IPFS_GATEWAY must be an HTTPS prefix ending in /"
        );
        let path = root.join("pins.json");
        let cache = match File::open(&path) {
            Ok(f) => serde_json::from_reader(f)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(e.into()),
        };
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(45))
                .build()?,
            jwt,
            gateway,
            cache: Mutex::new(cache),
            path,
        })
    }
    // Read once before sharing Pinata, to reconnect persisted image pins to local bytes.
    pub fn cached_image_hashes(&mut self) -> BTreeMap<String, String> {
        self.cache
            .get_mut()
            .iter()
            .filter_map(|(key, pin)| {
                key.strip_prefix("image:")
                    .map(|hash| (pin.uri.clone(), hash.to_owned()))
            })
            .collect()
    }
    pub fn browser_url(&self, uri: &str) -> String {
        match uri.strip_prefix("ipfs://") {
            Some(path) => format!("{}{path}", self.gateway),
            None => uri.to_owned(),
        }
    }
    async fn finish(&self, response: reqwest::Response) -> Result<Pinned> {
        let status = response.status();
        if !status.is_success() {
            let body = response.bytes().await?;
            let detail = String::from_utf8_lossy(&body)
                .replace(&self.jwt, "[redacted]")
                .chars()
                .take(1024)
                .collect::<String>();
            anyhow::bail!("Pinata upload failed with HTTP {status}: {detail}");
        }
        let reply: Reply = response.json().await.context("invalid Pinata reply")?;
        let reply = reply.data;
        ensure!(
            !reply.cid.is_empty()
                && reply.cid.len() <= 120
                && reply.cid.bytes().all(|c| c.is_ascii_alphanumeric()),
            "invalid Pinata CID"
        );
        Ok(Pinned {
            uri: format!("ipfs://{}", reply.cid),
            url: format!("{}{}", self.gateway, reply.cid),
            cid: reply.cid,
        })
    }
    pub async fn image(&self, hash: &str, bytes: Vec<u8>, mime: &str) -> Result<Pinned> {
        let key = format!("image:{hash}");
        let mut cache = self.cache.lock().await;
        if let Some(pin) = cache.get(&key) {
            return Ok(pin.clone());
        }
        let filename = if mime == "image/png" {
            "token.png"
        } else {
            "token.jpg"
        };
        self.upload(key, bytes, filename, mime, &mut cache).await
    }
    async fn upload(
        &self,
        key: String,
        bytes: Vec<u8>,
        filename: &str,
        mime: &str,
        cache: &mut BTreeMap<String, Pinned>,
    ) -> Result<Pinned> {
        let form = Form::new()
            .part(
                "file",
                Part::bytes(bytes)
                    .file_name(filename.to_owned())
                    .mime_str(mime)?,
            )
            .text("network", "public")
            .text("cid_version", "v1")
            .text("name", format!("xlaunch-{filename}"));
        let response = self
            .http
            .post("https://uploads.pinata.cloud/v3/files")
            .bearer_auth(&self.jwt)
            .multipart(form)
            .send()
            .await?;
        let pin = self.finish(response).await?;
        let mut next = cache.clone();
        next.insert(key, pin.clone());
        atomic_json(&self.path, &next)?;
        *cache = next;
        Ok(pin)
    }
    pub async fn metadata(&self, value: Value) -> Result<Pinned> {
        let key = format!("metadata:{}", digest(&value)?);
        let mut cache = self.cache.lock().await;
        if let Some(pin) = cache.get(&key) {
            return Ok(pin.clone());
        }
        self.upload(
            key,
            serde_json::to_vec(&value)?,
            "metadata.json",
            "application/json",
            &mut cache,
        )
        .await
    }
}
