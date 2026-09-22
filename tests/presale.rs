use anyhow::Result;
use solana_sdk::pubkey::Pubkey;
use xlaunch::{
    allocation::Contribution,
    presale::{Executor, RAISE_CAP_MICRO_USDC, Raise, Settlement, Status, Trigger},
    scheduler::Queue,
    state::Config,
};

fn payment(sequence: u64, amount: u64) -> Contribution {
    Contribution {
        transfer_id: format!("p-{sequence}"),
        sequence,
        wallet: Pubkey::new_from_array([sequence as u8 + 1; 32]),
        quote_budget: amount,
    }
}
fn raise() -> Result<Raise> {
    Raise::new(
        &Config::saved_evidence()?,
        Pubkey::new_from_array([71; 32]),
        Pubkey::new_from_array([72; 32]),
        1000,
        "Timer test".into(),
        "TEST".into(),
        "https://example.com/test.json".into(),
    )
}
#[test]
fn cap_boundary_preserves_excess_and_arrival_weights() -> Result<()> {
    let mut r = raise()?;
    r.record(payment(0, 100_000_000), 1000)?;
    r.record(payment(1, 100_000_000), 1001)?;
    r.record(payment(2, RAISE_CAP_MICRO_USDC), 1002)?;
    assert_eq!(r.accepted_micro_usdc(), RAISE_CAP_MICRO_USDC);
    assert_eq!(r.payments()[2].unaccepted_micro_usdc, 200_000_000);
    assert_eq!(r.trigger(1002), Some(Trigger::CapReached));
    assert!(r.record(payment(2, 1), 1003).is_err());
    let rows = r.allocations()?;
    assert!(rows[0].weight > rows[1].weight);
    for p in r.payments() {
        assert_eq!(
            p.accepted_micro_usdc + p.unaccepted_micro_usdc,
            p.contribution.quote_budget
        );
    }
    Ok(())
}
#[test]
fn one_hour_is_exclusive_for_acceptance_and_inclusive_for_settlement() -> Result<()> {
    let mut r = raise()?;
    r.record(payment(0, 100_000_000), 4599)?;
    r.record(payment(1, 100_000_000), 4600)?;
    assert_eq!(r.accepted_micro_usdc(), 100_000_000);
    assert_eq!(r.payments()[1].unaccepted_micro_usdc, 100_000_000);
    assert_eq!(r.trigger(4599), None);
    assert_eq!(r.trigger(4600), Some(Trigger::DeadlineElapsed));
    Ok(())
}
struct Ambiguous {
    calls: usize,
}
impl Executor for Ambiguous {
    fn settle(&mut self, _: &Raise, _: Trigger) -> Result<Settlement> {
        self.calls += 1;
        anyhow::bail!("submission outcome unknown")
    }
}
#[test]
fn deadline_and_failed_claim_survive_restart_without_resubmission() -> Result<()> {
    let root = tempfile::tempdir()?;
    let mut q = Queue::open(root.path())?;
    assert!(Queue::open(root.path()).is_err());
    let r = raise()?;
    let mint = r.mint;
    q.create(r)?;
    q.record(mint, payment(0, 100_000_000), 1001)?;
    let mut e = Ambiguous { calls: 0 };
    assert_eq!(q.tick(4599, &mut e)?, 0);
    assert_eq!(e.calls, 0);
    drop(q);
    let mut q = Queue::open(root.path())?;
    assert_eq!(q.tick(4600, &mut e)?, 0);
    assert_eq!(e.calls, 1);
    assert!(matches!(
        q.job(mint).unwrap().status,
        Status::NeedsReconciliation { .. }
    ));
    assert!(q.record(mint, payment(1, 100), 4601).is_err());
    drop(q);
    let mut q = Queue::open(root.path())?;
    q.tick(5000, &mut e)?;
    assert_eq!(e.calls, 1);
    Ok(())
}
