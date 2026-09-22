//! Passive CDP recorder for one explicitly selected Kameleo profile.
//! See docs/browser-capture.md for verified interfaces and capture limitations.
use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{BufWriter, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    net::TcpStream,
    signal::unix::{SignalKind, signal},
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Default)]
struct Capture {
    entries: Vec<Value>,
    active: HashMap<(String, String), usize>,
    starts: HashMap<usize, f64>,
    received: HashMap<usize, f64>,
}

fn text(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}

fn headers(v: &Value) -> Vec<Value> {
    v.as_object()
        .into_iter()
        .flatten()
        .flat_map(|(name, val)| {
            text(val)
                .split('\n')
                .map(move |value| json!({"name":name,"value":value}))
        })
        .collect()
}

fn header<'a>(v: &'a Value, name: &str) -> &'a str {
    v.as_object()
        .and_then(|h| h.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)))
        .map(|(_, v)| text(v))
        .unwrap_or("")
}

impl Capture {
    fn detach(&mut self, session: &str) {
        self.active.retain(|(s, _), i| {
            if s != session {
                return true;
            }
            if self.entries[*i]["_incomplete"] == true {
                self.entries[*i]["_targetDetached"] = true.into();
            }
            false
        });
    }

    fn response(&mut self, i: usize, r: &Value) {
        let e = &mut self.entries[i];
        e["response"] = json!({
            "status":r["status"].as_i64().unwrap_or(0), "statusText":text(&r["statusText"]),
            "httpVersion":text(&r["protocol"]), "headers":headers(&r["headers"]), "cookies":[],
            "content":{"size":0,"mimeType":text(&r["mimeType"])},
            "redirectURL":header(&r["headers"], "location"), "headersSize":-1,"bodySize":-1,
            "_cdp":r
        });
        e["request"]["httpVersion"] = r["protocol"].as_str().unwrap_or("").into();
    }

    fn finish(&mut self, i: usize, timestamp: f64) {
        let elapsed = ((timestamp - self.starts[&i]) * 1000.0).max(0.0);
        let before_headers = self
            .received
            .get(&i)
            .map(|t| ((t - self.starts[&i]) * 1000.0).max(0.0));
        // Preserve exact CDP timing separately. HAR groups the pre-header interval
        // in wait rather than claiming to have measured individual phases.
        let wait = before_headers.unwrap_or(elapsed).min(elapsed);
        self.entries[i]["time"] = elapsed.into();
        self.entries[i]["timings"] = json!({"send":0,"wait":wait,"receive":elapsed-wait,
            "comment":"Coarse event timings; wait includes connection and send. Exact browser timings are in response._cdp.timing."});
        self.entries[i]["_incomplete"] = false.into();
    }

    fn request(&mut self, session: &str, p: &Value) -> Result<Option<usize>> {
        let key = (session.to_owned(), text(&p["requestId"]).to_owned());
        let ts = p["timestamp"]
            .as_f64()
            .context("request timestamp missing")?;
        if let Some(i) = self.active.remove(&key)
            && p.get("redirectResponse").is_some()
        {
            self.response(i, &p["redirectResponse"]);
            self.finish(i, ts);
            self.entries[i]["_bodyUnavailable"] =
                "Redirect response body is not exposed by CDP".into();
        }
        let r = &p["request"];
        let url = url::Url::parse(text(&r["url"]))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Ok(None);
        }
        let wall = p["wallTime"].as_f64().context("request wallTime missing")?;
        let date = chrono::DateTime::from_timestamp_millis((wall * 1000.0) as i64)
            .context("invalid request wallTime")?
            .to_rfc3339();
        let query: Vec<_> = url
            .query_pairs()
            .map(|(k, v)| json!({"name":k,"value":v}))
            .collect();
        let mut request = json!({"method":r["method"],"url":r["url"],"httpVersion":"",
            "headers":headers(&r["headers"]),"cookies":[],"queryString":query,
            "headersSize":-1,"bodySize":if r["hasPostData"]==true {-1} else {0}});
        if let Some(post) = r["postData"].as_str() {
            request["postData"] =
                json!({"mimeType":header(&r["headers"],"content-type"),"text":post});
            request["bodySize"] = post.len().into();
        }
        let i = self.entries.len();
        self.entries
            .push(json!({"startedDateTime":date,"time":0,"request":request,
            "response":{"status":0,"statusText":"","httpVersion":"","cookies":[],"headers":[],
                "content":{"size":0,"mimeType":""},"redirectURL":"","headersSize":-1,"bodySize":-1},
            "cache":{},"timings":{"send":0,"wait":0,"receive":0},
            "_sessionId":session,"_requestId":p["requestId"],"_resourceType":p["type"],
            "_initiator":p["initiator"],"_incomplete":true}));
        self.active.insert(key, i);
        self.starts.insert(i, ts);
        Ok(Some(i))
    }

    fn body(&mut self, i: usize, reply: &Value) -> Result<()> {
        if let Some(error) = reply.get("error") {
            self.entries[i]["_bodyUnavailable"] = error.clone();
        } else {
            let result = &reply["result"];
            let body = result["body"].as_str().context("body missing")?;
            let encoded = result["base64Encoded"]
                .as_bool()
                .context("body encoding missing")?;
            let size = if encoded {
                STANDARD.decode(body)?.len()
            } else {
                body.len()
            };
            let content = &mut self.entries[i]["response"]["content"];
            content["text"] = body.into();
            content["size"] = size.into();
            if encoded {
                content["encoding"] = "base64".into();
            }
        }
        Ok(())
    }

    fn save(&self, out: &Path) -> Result<()> {
        let tmp = out.join("capture.har.tmp");
        let mut file = private_file(&tmp, false)?;
        serde_json::to_writer(
            &mut file,
            &json!({"log":{"version":"1.2",
            "creator":{"name":"xlaunch-kameleo-capture","version":"0.1.0"},
            "comment":"Private raw capture. Network extra-info and WebSocket frames are preserved in events.jsonl. Cookies are retained in headers, not parsed into cookie arrays.",
            "entries":self.entries}}),
        )?;
        file.flush()?;
        file.get_ref().sync_all()?;
        fs::rename(tmp, out.join("capture.har"))?;
        Ok(())
    }
}

fn private_file(path: &Path, new: bool) -> Result<BufWriter<File>> {
    let mut opts = OpenOptions::new();
    opts.write(true).mode(0o600);
    if new {
        opts.create_new(true);
    } else {
        opts.create(true).truncate(true);
    }
    Ok(BufWriter::new(opts.open(path)?))
}

enum Pending {
    Enable(String),
    Required(String),
    Body(usize),
    Post(usize),
}

struct PendingCommand {
    session: Option<String>,
    action: Pending,
}

async fn command(
    ws: &mut Socket,
    next: &mut u64,
    pending: &mut HashMap<u64, PendingCommand>,
    session: Option<&str>,
    method: &str,
    params: Value,
    action: Pending,
) -> Result<()> {
    *next += 1;
    let mut msg = json!({"id":next,"method":method,"params":params});
    if let Some(s) = session {
        msg["sessionId"] = s.into();
    }
    ws.send(Message::Text(msg.to_string().into())).await?;
    pending.insert(
        *next,
        PendingCommand {
            session: session.map(str::to_owned),
            action,
        },
    );
    Ok(())
}

fn attach_params() -> Value {
    json!({"autoAttach":true,"waitForDebuggerOnStart":true,"flatten":true,
        "filter":[{"type":"page"},{"type":"iframe"},{"type":"worker"},
            {"type":"service_worker"},{"type":"shared_worker"},{"exclude":true}]})
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 3 {
        bail!("usage: kameleo_capture <profile-cdp-websocket> <new-output-directory>");
    }
    let out = PathBuf::from(&args[2]);
    fs::create_dir(&out).context("capture directory must be new; never overwrite a recording")?;
    fs::set_permissions(&out, fs::Permissions::from_mode(0o700))?;
    let mut raw = private_file(&out.join("events.jsonl"), true)?;
    let mut metadata = private_file(&out.join("requests.jsonl"), true)?;
    let (mut ws, _) = connect_async(&args[1]).await?;
    let mut capture = Capture::default();
    let mut next = 0;
    let mut pending = HashMap::new();
    let mut sessions = HashSet::new();
    command(
        &mut ws,
        &mut next,
        &mut pending,
        None,
        "Target.setAutoAttach",
        attach_params(),
        Pending::Required("Target.setAutoAttach".into()),
    )
    .await?;
    let mut checkpoint = tokio::time::interval(Duration::from_secs(5));
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let result: Result<()> = async {
        loop {
            let msg = tokio::select! {
                _ = checkpoint.tick() => {
                    raw.flush()?; raw.get_ref().sync_data()?; metadata.flush()?;
                    capture.save(&out)?; continue;
                }
                _ = terminate.recv() => break,
                _ = interrupt.recv() => break,
                msg = ws.next() => msg.context("CDP connection ended")??,
            };
            let Message::Text(payload) = msg else {
                if matches!(msg, Message::Close(_)) {bail!("CDP connection closed");}
                continue;
            };
            let v: Value = serde_json::from_str(&payload)?;
            serde_json::to_writer(&mut raw,&v)?; raw.write_all(b"\n")?;
            if let Some(id) = v["id"].as_u64() {
                if let Some(reply_to) = pending.remove(&id) {
                    // A target can detach while its initialization commands are in flight.
                    // Only skip setup for a session whose detach we actually observed.
                    // Body replies are still processed to preserve explicit capture errors.
                    if matches!(&reply_to.action, Pending::Enable(_) | Pending::Required(_))
                        && reply_to.session.as_ref().is_some_and(|s| !sessions.contains(s)) {
                        continue;
                    }
                    match reply_to.action {
                        Pending::Body(i) => capture.body(i,&v)?,
                        Pending::Post(i) => {
                            if let Some(error) = v.get("error") {capture.entries[i]["_postDataUnavailable"] = error.clone();}
                            else {
                                let post = v["result"]["postData"].as_str().context("postData missing")?;
                                capture.entries[i]["request"]["postData"] = json!({"mimeType":"","text":post,
                                    "comment":"CDP getRequestPostData omits multipart file data"});
                            }
                        }
                        Pending::Required(name) => {if v.get("error").is_some() {bail!("{name} failed: {}",v["error"]);}}
                        Pending::Enable(s) => {
                            if v.get("error").is_some() {bail!("Network.enable failed: {}",v["error"]);}
                            command(&mut ws,&mut next,&mut pending,Some(&s),"Target.setAutoAttach",attach_params(),
                                Pending::Required("recursive Target.setAutoAttach".into())).await?;
                            command(&mut ws,&mut next,&mut pending,Some(&s),"Runtime.runIfWaitingForDebugger",json!({}),
                                Pending::Required("Runtime.runIfWaitingForDebugger".into())).await?;
                            println!("Recording attached target {s}");
                            fs::write(out.join("ready"),b"network enabled\n")?;
                        }
                    }
                }
                continue;
            }
            let method = text(&v["method"]);
            let p = &v["params"];
            if method=="Target.detachedFromTarget" {
                let s = text(&p["sessionId"]);
                sessions.remove(s);
                capture.detach(s);
                continue;
            }
            if method=="Target.attachedToTarget" {
                let s = text(&p["sessionId"]);
                if !sessions.insert(s.to_owned()) {continue;}
                if text(&p["targetInfo"]["url"]).starts_with("chrome-extension:") {
                    command(&mut ws,&mut next,&mut pending,Some(s),"Runtime.runIfWaitingForDebugger",json!({}),
                        Pending::Required("resume extension".into())).await?;
                    continue;
                }
                command(&mut ws,&mut next,&mut pending,Some(s),"Network.enable",
                    json!({"maxTotalBufferSize":268435456,"maxResourceBufferSize":67108864,"maxPostDataSize":16777216}),
                    Pending::Enable(s.to_owned())).await?;
                continue;
            }
            let s = text(&v["sessionId"]);
            let request_id = text(&p["requestId"]);
            if method=="Network.requestWillBeSent" {
                if let Some(i) = capture.request(s,p)? {
                    // The convenience request log omits all headers, body and query values.
                    let url = url::Url::parse(text(&p["request"]["url"]))?;
                    serde_json::to_writer(&mut metadata,&json!({"entry":i,"method":p["request"]["method"],
                        "origin":url.origin().ascii_serialization(),"path":url.path(),"type":p["type"]}))?;
                    metadata.write_all(b"\n")?;
                    if p["request"]["hasPostData"]==true && p["request"].get("postData").is_none() {
                        command(&mut ws,&mut next,&mut pending,Some(s),"Network.getRequestPostData",
                            json!({"requestId":request_id}),Pending::Post(i)).await?;
                    }
                }
                continue;
            }
            let Some(&i) = capture.active.get(&(s.to_owned(),request_id.to_owned())) else {continue;};
            match method {
                "Network.responseReceived" => {
                    capture.response(i,&p["response"]);
                    if let Some(t) = p["timestamp"].as_f64() {capture.received.insert(i,t);}
                }
                "Network.loadingFinished"|"Network.loadingFailed" => {
                    capture.finish(i,p["timestamp"].as_f64().context("completion timestamp missing")?);
                    if method=="Network.loadingFinished" {
                        capture.entries[i]["_transferSize"] = p["encodedDataLength"].clone();
                        command(&mut ws,&mut next,&mut pending,Some(s),"Network.getResponseBody",
                            json!({"requestId":request_id}),Pending::Body(i)).await?;
                    } else {capture.entries[i]["_error"] = p.clone();}
                }
                _ => {}
            }
        }
        Ok(())
    }.await;
    raw.flush()?;
    raw.get_ref().sync_all()?;
    metadata.flush()?;
    capture.save(&out)?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(url: &str, ts: f64) -> Value {
        json!({"requestId":"same-id","timestamp":ts,"wallTime":1_790_000_000.0+ts,
            "request":{"url":url,"method":"GET","headers":{}},"type":"Fetch"})
    }

    #[test]
    fn redirects_and_identical_ids_in_different_sessions_stay_separate() -> Result<()> {
        let mut c = Capture::default();
        c.request("page", &request("https://example.test/start", 1.0))?;
        c.request("worker", &request("https://example.test/worker", 1.1))?;
        let mut redirect = request("https://example.test/end", 2.0);
        redirect["redirectResponse"] = json!({"status":302,"statusText":"Found",
            "protocol":"h2","mimeType":"text/html","headers":{"location":"https://example.test/end"}});
        c.request("page", &redirect)?;
        assert_eq!(c.entries.len(), 3);
        assert_eq!(c.entries[0]["response"]["status"], 302);
        assert_eq!(c.entries[0]["time"], 1000.0);
        assert_eq!(c.active[&("worker".into(), "same-id".into())], 1);
        assert_eq!(c.active[&("page".into(), "same-id".into())], 2);
        c.body(
            1,
            &json!({"result":{"body":"AAEC/w==","base64Encoded":true}}),
        )?;
        assert_eq!(c.entries[1]["response"]["content"]["size"], 4);
        assert_eq!(c.entries[1]["response"]["content"]["encoding"], "base64");
        Ok(())
    }

    #[test]
    fn unavailable_bodies_are_explicit_and_checkpoint_is_private() -> Result<()> {
        let mut c = Capture::default();
        c.request("page", &request("https://example.test/", 1.0))?;
        c.body(
            0,
            &json!({"error":{"code":-32000,"message":"No resource with given identifier found"}}),
        )?;
        assert!(c.entries[0].get("_bodyUnavailable").is_some());
        assert!(c.entries[0]["response"]["content"].get("text").is_none());
        let dir = tempfile::tempdir()?;
        c.save(dir.path())?;
        let path = dir.path().join("capture.har");
        assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o600);
        let har: Value = serde_json::from_reader(File::open(path)?)?;
        assert_eq!(har["log"]["entries"].as_array().unwrap().len(), 1);
        Ok(())
    }

    #[test]
    fn detached_target_keeps_its_partial_requests_without_disrupting_other_targets() -> Result<()> {
        let mut c = Capture::default();
        c.request("closing", &request("https://example.test/closing", 1.0))?;
        c.request("remaining", &request("https://example.test/remaining", 1.0))?;
        c.detach("closing");
        assert_eq!(c.entries[0]["_incomplete"], true);
        assert_eq!(c.entries[0]["_targetDetached"], true);
        assert!(!c.active.contains_key(&("closing".into(), "same-id".into())));
        assert_eq!(c.active[&("remaining".into(), "same-id".into())], 1);
        Ok(())
    }
}
