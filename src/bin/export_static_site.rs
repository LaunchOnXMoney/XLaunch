//! Writes the site as static files for a CDN host, using the same page table and
//! the same assembly the live server uses. The exported pages call the backend at
//! an absolute origin, so the API can live behind a tunnel on another host.
use anyhow::{Context, Result, ensure};
use std::{fs, path::PathBuf};
use xlaunch::web::server::{PAGES, STATIC_IMAGES, adapt_html};

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let usage =
        "usage: export_static_site API_ORIGIN OUTPUT_DIR FRONTEND_DIR ADAPTER_DIR VENDOR_DIR";
    let api_origin = args.next().context(usage)?;
    let output: PathBuf = args.next().context(usage)?.into();
    let frontend: PathBuf = args.next().context(usage)?.into();
    let adapters: PathBuf = args.next().context(usage)?.into();
    let vendor: PathBuf = args.next().context(usage)?.into();

    // An empty origin keeps same-origin behaviour; anything else must be a bare
    // HTTPS origin, because it is baked into every exported page.
    if !api_origin.is_empty() {
        let parsed = url::Url::parse(&api_origin).context("API_ORIGIN must be a URL")?;
        let loopback = matches!(parsed.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
        ensure!(
            (parsed.scheme() == "https" || (parsed.scheme() == "http" && loopback))
                && parsed.host_str().is_some()
                && parsed.username().is_empty()
                && parsed.password().is_none()
                && parsed.path() == "/"
                && parsed.query().is_none()
                && parsed.fragment().is_none(),
            "API_ORIGIN must be a bare HTTPS origin, or http on loopback for local runs"
        );
        ensure!(
            parsed.origin().ascii_serialization() == api_origin.trim_end_matches('/'),
            "API_ORIGIN must have no trailing path"
        );
    }

    fs::create_dir_all(&output)?;
    let runtime = fs::read_to_string(adapters.join("runtime-config.js"))?;
    // Injected ahead of the shared runtime, which only defaults the value.
    let runtime = format!(
        "window.xlaunchApiBase={};\n{runtime}",
        serde_json::to_string(api_origin.trim_end_matches('/'))?
    );

    for page in PAGES {
        let html = adapt_html(
            &fs::read_to_string(frontend.join(page.file))?,
            &runtime,
            &fs::read_to_string(adapters.join(page.logic))?,
        )?;
        let path = output.join(page.export_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, html)?;
        println!("wrote {}", path.display());
    }

    for image in STATIC_IMAGES {
        let source = frontend.join(image.source_path);
        if !source.exists() {
            println!("skipped missing {}", image.source_path);
            continue;
        }
        let path = output.join(image.published_path.trim_start_matches('/'));
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&source, &path)?;
        println!("wrote {}", path.display());
    }
    for (name, source) in [
        ("support.js", frontend.join("support.js")),
        (
            "vendor/react.production.min.js",
            vendor.join("react.production.min.js"),
        ),
        (
            "vendor/react-dom.production.min.js",
            vendor.join("react-dom.production.min.js"),
        ),
    ] {
        let path = output.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&source, &path)?;
        println!("wrote {}", path.display());
    }
    Ok(())
}
