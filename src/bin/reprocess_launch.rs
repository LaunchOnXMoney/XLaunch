//! Reprocess one previously rejected launch while the web service is stopped.
use anyhow::{Context, Result, bail};
use std::{fs::File, path::PathBuf};
use xlaunch::web::model::{Inbox, Store};

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 4 {
        bail!("usage: reprocess_launch <data-dir> <inbox-state> <sequence>");
    }
    let sequence: u64 = args[3].parse()?;
    let inbox: Inbox = serde_json::from_reader(File::open(&args[2])?)?;
    let record = inbox
        .transactions
        .iter()
        .find(|r| r.sequence == sequence)
        .context("payment not found")?;
    let mut store = Store::open(PathBuf::from(&args[1]))?;
    let mint = store.recover_launch(record)?;
    println!("Recovered payment sequence {sequence}: {mint}");
    Ok(())
}
