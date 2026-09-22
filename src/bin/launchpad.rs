use anyhow::{Result, bail};
use std::{net::SocketAddr, path::PathBuf, time::Duration};
use xlaunch::web::server::{App, Config, router};
#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 7 {
        bail!(
            "usage: launchpad <bind-address> <inbox-state> <data-dir> <frontend-dir> <adapters-dir> <vendor-dir>"
        )
    }
    let addr: SocketAddr = args[1].parse()?;
    let app = App::open(
        PathBuf::from(&args[2]),
        PathBuf::from(&args[3]),
        &PathBuf::from(&args[4]),
        &PathBuf::from(&args[5]),
        &PathBuf::from(&args[6]),
        Config::from_env()?,
    )?;
    App::start_workers(&app);
    let handle = axum_server::Handle::new();
    let shutdown = handle.clone();
    tokio::spawn(async move {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler");
        tokio::select! {_=term.recv()=>{},_=tokio::signal::ctrl_c()=>{}}
        shutdown.graceful_shutdown(Some(Duration::from_secs(60)));
    });
    println!("Launchpad listening at http://{addr}");
    axum_server::bind(addr)
        .handle(handle)
        .serve(router(app).into_make_service())
        .await?;
    Ok(())
}
