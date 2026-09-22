//! Durable single-owner queue. Call run() to execute cap/deadline settlements.
//! All adapter writes go through this owner; X routes are intentionally separate.
use crate::{
    allocation::Contribution,
    presale::{Executor, Job, Raise, Status},
};
use anyhow::{Context, Result, ensure};
use solana_sdk::pubkey::Pubkey;
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub struct Queue {
    root: PathBuf,
    jobs: BTreeMap<Pubkey, Job>,
    _owner: File,
}
impl Queue {
    /// Exclusive ownership is held using a durable lock file. A crash leaves it
    /// behind intentionally: reconcile Settling jobs/journals before clearing it.
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(&root)?;
        let owner = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(root.join("owner.lock"))
            .context(
                "queue already owned; after a crash reconcile journals before removing owner.lock",
            )?;
        owner.sync_all()?;
        File::open(&root)?.sync_all()?;
        let mut queue = Self {
            root,
            jobs: BTreeMap::new(),
            _owner: owner,
        };
        for entry in fs::read_dir(&queue.root)? {
            let path = entry?.path();
            if path.extension().is_some_and(|x| x == "json") {
                let job: Job = serde_json::from_slice(&fs::read(path)?)?;
                ensure!(
                    queue.jobs.insert(job.raise.mint, job).is_none(),
                    "duplicate persisted mint"
                );
            }
        }
        Ok(queue)
    }
    fn save(&self, job: &Job) -> Result<()> {
        let path = self.root.join(format!("{}.json", job.raise.mint));
        let tmp = path.with_extension("tmp");
        let mut file = File::create(&tmp)?;
        file.write_all(&serde_json::to_vec_pretty(job)?)?;
        file.sync_all()?;
        fs::rename(tmp, path)?;
        File::open(&self.root)?.sync_all()?;
        Ok(())
    }
    pub fn create(&mut self, raise: Raise) -> Result<()> {
        ensure!(!self.jobs.contains_key(&raise.mint), "mint already queued");
        let job = Job {
            raise,
            status: Status::Open,
        };
        self.save(&job)?;
        self.jobs.insert(job.raise.mint, job);
        Ok(())
    }
    pub fn job(&self, mint: Pubkey) -> Option<&Job> {
        self.jobs.get(&mint)
    }
    pub fn record(
        &mut self,
        mint: Pubkey,
        contribution: Contribution,
        received_at: u64,
    ) -> Result<()> {
        let mut job = self.jobs.get(&mint).context("unknown raise")?.clone();
        ensure!(
            matches!(job.status, Status::Open),
            "ledger closed; record late funds for refund in the payment adapter"
        );
        job.raise.record(contribution, received_at)?;
        self.save(&job)?;
        self.jobs.insert(mint, job);
        Ok(())
    }
    /// Invoke on each settled payment as well as from the clock loop. Persist
    /// the claim BEFORE calling the executor; never retry an ambiguous outcome.
    pub fn tick(&mut self, now: u64, executor: &mut impl Executor) -> Result<usize> {
        let due: Vec<_> = self
            .jobs
            .iter()
            .filter(|(_, j)| matches!(j.status, Status::Open))
            .filter_map(|(mint, j)| j.raise.trigger(now).map(|t| (*mint, t)))
            .collect();
        let mut count = 0;
        for (mint, trigger) in due {
            let mut job = self.jobs[&mint].clone();
            if job.raise.accepted_micro_usdc() == 0 {
                job.status = Status::Empty;
                self.save(&job)?;
                self.jobs.insert(mint, job);
                continue;
            }
            job.status = Status::Settling {
                trigger: trigger.clone(),
            };
            self.save(&job)?;
            self.jobs.insert(mint, job.clone());
            job.status = match executor.settle(&job.raise, trigger) {
                Ok(receipt)
                    if receipt.tokens_received > 0
                        && receipt.tokens_received == receipt.tokens_distributed
                        && !receipt.launch_signature.is_empty()
                        && !receipt.payout_signatures.is_empty() =>
                {
                    count += 1;
                    Status::Distributed { receipt }
                }
                Ok(_) => Status::NeedsReconciliation {
                    error: "incomplete distribution receipt".into(),
                },
                Err(error) => Status::NeedsReconciliation {
                    error: format!("{error:#}"),
                },
            };
            self.save(&job)?;
            self.jobs.insert(mint, job);
        }
        Ok(count)
    }
    /// Integration service owns this worker. Keep ingesting complete settled
    /// payment batches between ticks; tick uses an explicit time for fork tests.
    pub fn run(
        &mut self,
        executor: &mut impl Executor,
        mut ingest: impl FnMut(&mut Self) -> Result<()>,
    ) -> Result<()> {
        loop {
            ingest(self)?;
            self.tick(
                SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
                executor,
            )?;
            std::thread::sleep(Duration::from_secs(1));
        }
    }
}
impl Drop for Queue {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.root.join("owner.lock"));
    }
}
