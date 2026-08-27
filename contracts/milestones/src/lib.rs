//! MergeFi Milestone Funding Contract
//!
//! A milestone pools one or more sponsors' contributions into a lump-sum
//! budget shared across multiple GitHub issues that make up a release.
//! The first sponsor deposits to open the pool; the backend oracle
//! allocates slices of the budget to individual issues and later releases
//! each allocation (optionally split across a team) as issues are merged,
//! exactly like the escrow contract's `release`, but drawn from a shared
//! pool instead of a single-issue deposit. If the release is cancelled,
//! the unallocated remainder is refunded to every contributor in
//! proportion to what they put in.
#![no_std]

mod error;
mod types;

#[cfg(test)]
mod test;

use error::Error;
use soroban_sdk::{contract, contractimpl, token, Address, Env, Map, Vec};
use types::{Contribution, DataKey, IssueStatus, Milestone};

pub const BPS_DENOMINATOR: i128 = 10_000;

/// Maximum number of distinct contributions (sponsors) a single milestone
/// can accumulate. Bounds the per-contributor loop in `cancel_milestone`
/// (and any future timeout-triggered wind-down that reuses
/// `refund_remaining_budget`) to a small, predictable constant regardless
/// of how popular a release gets. See `docs/milestones-crowdfunding-design.md`.
pub const MAX_SPONSORS: u32 = 20;

#[contract]
pub struct MilestonesContract;

#[contractimpl]
impl MilestonesContract {
    /// One-time setup. Requires `admin`'s own authorization, so nobody can
    /// name a third-party address as admin without that address's consent
    /// — see `docs/access-control-audit.md` for what this does and does
    /// not protect against (it does not stop initializer front-running).
    pub fn initialize(
        env: Env,
        admin: Address,
        treasury: Address,
        fee_bps: u32,
    ) -> Result<(), Error> {
        admin.require_auth();

        if env.storage().instance().has(&DataKey::Admin) {
            return Err(Error::AlreadyInitialized);
        }
        if fee_bps as i128 > BPS_DENOMINATOR {
            return Err(Error::InvalidFee);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Treasury, &treasury);
        env.storage().instance().set(&DataKey::FeeBps, &fee_bps);
        Ok(())
    }

    /// The original sponsor deposits `total_budget` of `token` to open a
    /// new milestone pool. Requires that sponsor's authorization. One
    /// milestone per `milestone_id` — a second `create_milestone` call on
    /// the same id is rejected (`IssueAlreadyAllocated`); every sponsor
    /// after the first uses `contribute` instead. See
    /// `docs/milestones-crowdfunding-design.md` for why creation and
    /// contribution are kept as two separate entrypoints.
    pub fn create_milestone(
        env: Env,
        milestone_id: u64,
        sponsor: Address,
        token: Address,
        total_budget: i128,
    ) -> Result<(), Error> {
        sponsor.require_auth();

        if total_budget <= 0 {
            return Err(Error::InvalidAmount);
        }

        let key = DataKey::Milestone(milestone_id);
        if env.storage().persistent().has(&key) {
            return Err(Error::IssueAlreadyAllocated);
        }

        let token_client = token::Client::new(&env, &token);
        token_client.transfer(&sponsor, env.current_contract_address(), &total_budget);

        // The original funder is always contribution index 0, exactly like
        // `escrow::fund`; every later sponsor appends via `contribute`.
        let contribution_key = DataKey::Contribution(milestone_id, 0);
        env.storage().persistent().set(
            &contribution_key,
            &Contribution {
                sponsor: sponsor.clone(),
                amount: total_budget,
            },
        );
        extend_ttl(&env, &contribution_key);

        let milestone = Milestone {
            sponsor,
            token,
            total_budget,
            remaining_budget: total_budget,
            total_fees_collected: 0,
            created_at: env.ledger().timestamp(),
            closed: false,
            allocations: Map::new(&env),
            contributor_count: 1,
        };
        env.storage().persistent().set(&key, &milestone);
        extend_ttl(&env, &key);
        Ok(())
    }

    /// Adds an additional sponsor's contribution to an already-created
    /// milestone, enabling crowdfunding: several sponsors can co-fund the
    /// same `milestone_id`. Requires the contributing sponsor's
    /// authorization. Uses the token already recorded on the milestone (no
    /// `token` parameter), so a top-up can never silently use a different
    /// asset than the original funder intended. Rejects `MilestoneNotFound`,
    /// `MilestoneClosed`, and `TooManySponsors` once `MAX_SPONSORS`
    /// contributions have already been recorded.
    pub fn contribute(
        env: Env,
        milestone_id: u64,
        sponsor: Address,
        amount: i128,
    ) -> Result<(), Error> {
        sponsor.require_auth();

        if amount <= 0 {
            return Err(Error::InvalidAmount);
        }

        let mkey = DataKey::Milestone(milestone_id);
        let mut milestone: Milestone = env
            .storage()
            .persistent()
            .get(&mkey)
            .ok_or(Error::MilestoneNotFound)?;

        if milestone.closed {
            return Err(Error::MilestoneClosed);
        }
        let mut existing_index = None;
        for i in 0..milestone.contributor_count {
            let contribution_key = DataKey::Contribution(milestone_id, i);
            let contribution: Contribution =
                env.storage().persistent().get(&contribution_key).unwrap();
            if contribution.sponsor == sponsor {
                existing_index = Some(i);
                break;
            }
        }

        if existing_index.is_none() && milestone.contributor_count >= MAX_SPONSORS {
            return Err(Error::TooManySponsors);
        }

        let token_client = token::Client::new(&env, &milestone.token);
        token_client.transfer(&sponsor, env.current_contract_address(), &amount);

        if let Some(index) = existing_index {
            let contribution_key = DataKey::Contribution(milestone_id, index);
            let mut contribution: Contribution =
                env.storage().persistent().get(&contribution_key).unwrap();
            contribution.amount += amount;
            env.storage()
                .persistent()
                .set(&contribution_key, &contribution);
            extend_ttl(&env, &contribution_key);
        } else {
            let contribution_key = DataKey::Contribution(milestone_id, milestone.contributor_count);
            env.storage()
                .persistent()
                .set(&contribution_key, &Contribution { sponsor, amount });
            extend_ttl(&env, &contribution_key);
            milestone.contributor_count += 1;
        }

        // New funds arrive unallocated: the pool's total *and* its
        // unallocated remainder both grow by exactly the contribution, so
        // a later proportional refund treats them like any other share.
        milestone.total_budget += amount;
        milestone.remaining_budget += amount;
        env.storage().persistent().set(&mkey, &milestone);
        extend_ttl(&env, &mkey);

        // Refresh every prior contribution's TTL so older records don't
        // archive ahead of the parent while the milestone stays active.
        // The newly-written record (contributor_count - 1) was already
        // extended above; iterate the full range to cover all of them.
        for i in 0..milestone.contributor_count {
            extend_ttl(&env, &DataKey::Contribution(milestone_id, i));
        }

        Ok(())
    }

    /// Admin-only: reserves `amount` of the milestone's remaining budget for
    /// `issue_id`. Rejects if the issue is already allocated, the milestone
    /// is closed, or `amount` exceeds the remaining (unallocated) budget.
    ///
    /// Note: this contract has no visibility into `mergefi-escrow` —
    /// nothing here stops the same `issue_id` from also being funded via
    /// `escrow::fund` as a standalone bounty. See README "Why three
    /// contracts instead of one" → "Cross-contract double-funding" for why
    /// that gap is accepted here and handled by `mergefi-backend` instead.
    pub fn allocate(env: Env, milestone_id: u64, issue_id: u64, amount: i128) -> Result<(), Error> {
        require_admin(&env)?.require_auth();

        if amount <= 0 {
            return Err(Error::InvalidAmount);
        }

        let mkey = DataKey::Milestone(milestone_id);
        let mut milestone: Milestone = env
            .storage()
            .persistent()
            .get(&mkey)
            .ok_or(Error::MilestoneNotFound)?;

        if milestone.closed {
            return Err(Error::MilestoneClosed);
        }
        if milestone.allocations.contains_key(issue_id) {
            return Err(Error::IssueAlreadyAllocated);
        }
        if amount > milestone.remaining_budget {
            return Err(Error::OverAllocation);
        }

        milestone.remaining_budget -= amount;
        milestone.allocations.set(issue_id, amount);
        env.storage().persistent().set(&mkey, &milestone);
        extend_ttl(&env, &mkey);

        // Refresh all contribution sub-records so they stay alive alongside
        // the parent record as the milestone accumulates allocations over time.
        for i in 0..milestone.contributor_count {
            extend_ttl(&env, &DataKey::Contribution(milestone_id, i));
        }

        let skey = DataKey::IssueStatus(milestone_id, issue_id);
        env.storage()
            .persistent()
            .set(&skey, &IssueStatus::Allocated);
        extend_ttl(&env, &skey);

        Ok(())
    }

    /// Admin-only: releases the previously allocated amount for `issue_id`
    /// to `recipients` (basis points summing to 10000), minus the protocol
    /// fee, exactly as in the escrow contract.
    pub fn release_issue(
        env: Env,
        milestone_id: u64,
        issue_id: u64,
        recipients: Vec<(Address, u32)>,
    ) -> Result<(), Error> {
        require_admin(&env)?.require_auth();

        let mkey = DataKey::Milestone(milestone_id);
        let mut milestone: Milestone = env
            .storage()
            .persistent()
            .get(&mkey)
            .ok_or(Error::MilestoneNotFound)?;

        let skey = DataKey::IssueStatus(milestone_id, issue_id);
        let status: IssueStatus = env
            .storage()
            .persistent()
            .get(&skey)
            .ok_or(Error::IssueNotAllocated)?;
        if status == IssueStatus::Released {
            return Err(Error::IssueAlreadyReleased);
        }

        let amount = milestone
            .allocations
            .get(issue_id)
            .ok_or(Error::IssueNotAllocated)?;

        let payouts = compute_split(&env, amount, &recipients)?;
        let treasury: Address = env.storage().instance().get(&DataKey::Treasury).unwrap();
        let token_client = token::Client::new(&env, &milestone.token);
        let contract_address = env.current_contract_address();

        if payouts.fee > 0 {
            token_client.transfer(&contract_address, &treasury, &payouts.fee);
            // record collected fee on-chain for auditability
            milestone.total_fees_collected += payouts.fee;
            env.storage().persistent().set(&mkey, &milestone);
            extend_ttl(&env, &mkey);
        }
        for (recipient, share) in payouts.shares.iter() {
            if share > 0 {
                token_client.transfer(&contract_address, &recipient, &share);
            }
        }

        env.storage()
            .persistent()
            .set(&skey, &IssueStatus::Released);
        extend_ttl(&env, &skey);

        // Refresh the parent milestone's Contribution sub-records so they
        // stay alive as individual issues are released over the milestone's
        // lifetime — release_issue doesn't touch contributions directly but
        // is called repeatedly on long-lived milestones.
        for i in 0..milestone.contributor_count {
            extend_ttl(&env, &DataKey::Contribution(milestone_id, i));
        }

        Ok(())
    }

    /// Admin-only: closes the milestone and refunds any unallocated budget
    /// back to its contributors, in proportion to what each one put in
    /// (e.g. release cancelled with issues remaining). A single-sponsor
    /// milestone is the degenerate case: the whole remainder goes back to
    /// the one sponsor, exactly as before this contract supported
    /// crowdfunding. See `refund_remaining_budget` and
    /// `docs/milestones-crowdfunding-design.md`.
    pub fn cancel_milestone(env: Env, milestone_id: u64) -> Result<(), Error> {
        require_admin(&env)?.require_auth();

        let mkey = DataKey::Milestone(milestone_id);
        let mut milestone: Milestone = env
            .storage()
            .persistent()
            .get(&mkey)
            .ok_or(Error::MilestoneNotFound)?;

        if milestone.closed {
            return Err(Error::MilestoneClosed);
        }

        if milestone.remaining_budget > 0 {
            refund_remaining_budget(&env, milestone_id, &milestone)?;
            milestone.remaining_budget = 0;
        }
        milestone.closed = true;
        env.storage().persistent().set(&mkey, &milestone);
        extend_ttl(&env, &mkey);
        Ok(())
    }

    /// Permissionless TTL refresh: re-extends `milestone_id`'s
    /// persistent-storage TTL (and those of all its `Contribution`
    /// sub-records) by the standard flat bump, without touching any
    /// milestone state. Exists because individual contribution entries
    /// have their own TTL and will archive independently of the parent
    /// `Milestone` record if never re-extended — for a long-lived
    /// milestone whose mutating calls (allocate/release_issue) don't touch
    /// older contributions, those records can silently fall off-ledger.
    ///
    /// Unlike `escrow::keep_alive`, milestones have no natural deadline
    /// timestamp, so this applies the flat ~29-day bump rather than a
    /// deadline-scaled extension. Call it periodically (at least once
    /// within any ~29-day window) to keep a long-lived milestone and all
    /// its contribution history alive.
    ///
    /// Callable by anyone and needs no authorization: it can only ever keep
    /// records alive longer, never change what they hold.
    pub fn keep_alive(env: Env, milestone_id: u64) -> Result<(), Error> {
        let mkey = DataKey::Milestone(milestone_id);
        let milestone: Milestone = env
            .storage()
            .persistent()
            .get(&mkey)
            .ok_or(Error::MilestoneNotFound)?;

        extend_ttl(&env, &mkey);

        // Keep every contribution sub-record alive alongside the parent so
        // they can't archive independently while the milestone stays live.
        for i in 0..milestone.contributor_count {
            extend_ttl(&env, &DataKey::Contribution(milestone_id, i));
        }

        Ok(())
    }

    pub fn get_milestone(env: Env, milestone_id: u64) -> Result<Milestone, Error> {
        env.storage()
            .persistent()
            .get(&DataKey::Milestone(milestone_id))
            .ok_or(Error::MilestoneNotFound)
    }

    pub fn get_issue_status(
        env: Env,
        milestone_id: u64,
        issue_id: u64,
    ) -> Result<IssueStatus, Error> {
        env.storage()
            .persistent()
            .get(&DataKey::IssueStatus(milestone_id, issue_id))
            .ok_or(Error::IssueNotAllocated)
    }

    /// Returns the `index`-th contribution recorded for `milestone_id`
    /// (`0` is always the original `create_milestone` caller; subsequent
    /// indices are `contribute` calls in the order they were accepted),
    /// letting off-chain callers enumerate the full contribution ledger
    /// via `0..milestone.contributor_count`.
    pub fn get_contribution(
        env: Env,
        milestone_id: u64,
        index: u32,
    ) -> Result<Contribution, Error> {
        env.storage()
            .persistent()
            .get(&DataKey::Contribution(milestone_id, index))
            .ok_or(Error::MilestoneNotFound)
    }
}

struct Payouts {
    fee: i128,
    shares: Vec<(Address, i128)>,
}

fn compute_split(
    env: &Env,
    total: i128,
    recipients: &Vec<(Address, u32)>,
) -> Result<Payouts, Error> {
    if recipients.is_empty() {
        return Err(Error::InvalidSplit);
    }

    let mut bps_sum: i128 = 0;
    for (_, bps) in recipients.iter() {
        bps_sum += bps as i128;
    }
    if bps_sum != BPS_DENOMINATOR {
        return Err(Error::InvalidSplit);
    }

    let fee_bps: u32 = env
        .storage()
        .instance()
        .get(&DataKey::FeeBps)
        .ok_or(Error::NotInitialized)?;

    let fee = total * (fee_bps as i128) / BPS_DENOMINATOR;
    let distributable = total - fee;

    let mut shares: Vec<(Address, i128)> = Vec::new(env);
    let mut order: Vec<(u32, i128, Address)> = Vec::new(env);
    let mut allocated: i128 = 0;

    for (recipient, bps) in recipients.iter() {
        let numerator = distributable * (bps as i128);
        let share = numerator / BPS_DENOMINATOR;
        let remainder = numerator % BPS_DENOMINATOR;
        allocated += share;
        shares.push_back((recipient.clone(), share));
        order.push_back((order.len(), remainder, recipient));
    }

    // Distribute the rounding dust by largest remainder (with the existing
    // address-based tie-break) in O(n log n): sort the (index, remainder,
    // address) records once, then award one unit to each of the first `dust`
    // entries. This is equivalent to the previous repeated-linear-scan loop,
    // because each award only consumes the selected entry and never changes
    // any other entry's remainder. `dust` is at most `recipients.len() - 1`,
    // so the first `dust` sorted entries always exist.
    let dust = distributable - allocated;
    if dust > 0 {
        sort_remainders_desc(&mut order);
        for k in 0..dust as u32 {
            let (index, _, _) = order.get(k).unwrap();
            let (recipient, share) = shares.get(index).unwrap();
            shares.set(index, (recipient, share + 1));
        }
    }

    Ok(Payouts { fee, shares })
}

/// True if `a` sorts before `b` in largest-remainder order: remainder
/// descending, then address ascending, then original index ascending (which
/// reproduces the address-based tie-break of the previous O(n²) loop).
fn remainder_order_less(a: &(u32, i128, Address), b: &(u32, i128, Address)) -> bool {
    b.1.cmp(&a.1)
        .then_with(|| a.2.cmp(&b.2))
        .then_with(|| a.0.cmp(&b.0))
        == core::cmp::Ordering::Less
}

/// Sifts the element at `start` down a max-heap occupying `[start, end)`,
/// ordering elements by [`remainder_order_less`].
fn sift_down_remainder_order(order: &mut Vec<(u32, i128, Address)>, start: u32, end: u32) {
    let mut root = start;
    loop {
        let mut child = 2 * root + 1;
        if child >= end {
            break;
        }
        if child + 1 < end
            && remainder_order_less(&order.get(child).unwrap(), &order.get(child + 1).unwrap())
        {
            child += 1;
        }
        if remainder_order_less(&order.get(root).unwrap(), &order.get(child).unwrap()) {
            let a = order.get(root).unwrap();
            let b = order.get(child).unwrap();
            order.set(root, b);
            order.set(child, a);
            root = child;
        } else {
            break;
        }
    }
}

/// In-place heapsort of `(index, remainder, address)` records into
/// largest-remainder order. O(n log n) worst case, with no recursion and no
/// heap allocation, so it is safe under `#![no_std]` and only mutates the
/// host-backed `order` through `get`/`set`.
fn sort_remainders_desc(order: &mut Vec<(u32, i128, Address)>) {
    let n = order.len();
    if n < 2 {
        return;
    }

    // Build a max-heap over the whole array.
    let mut start = n / 2;
    loop {
        start -= 1;
        sift_down_remainder_order(order, start, n);
        if start == 0 {
            break;
        }
    }

    // Repeatedly move the largest remaining element to the end of the array,
    // shrinking the heap until the array is sorted ascending by `less`.
    let mut end = n;
    while end > 1 {
        end -= 1;
        let a = order.get(0).unwrap();
        let b = order.get(end).unwrap();
        order.set(0, b);
        order.set(end, a);
        sift_down_remainder_order(order, 0, end);
    }
}

/// Pays each contributor their share of `milestone.remaining_budget` (the
/// unallocated remainder of the pool), computed as
/// `remaining_budget * contribution.amount / total_budget` — i.e. in
/// proportion to what each sponsor actually put in, not to any nominal
/// split of the original deposit. Because the contribution ledger is
/// append-only and `total_budget` / `remaining_budget` are maintained
/// additively, each sponsor's share is a fixed fraction of the pool no
/// matter how many intervening `allocate` / `release_issue` (and, once the
/// deallocate issue lands, `deallocate`) calls happened between the
/// deposits and this refund — the remainder to return is simply sliced by
/// those fixed fractions at refund time.
///
/// Uses largest-remainder rounding (the same invariant as
/// `compute_split`): every share is floored first, then the remaining dust
/// (always strictly less than `contributor_count` units) is granted one
/// unit at a time to the entry with the largest fractional remainder,
/// tie-broken by contribution index so the outcome is deterministic and
/// independent of anything a caller controls. The full `remaining_budget`
/// is returned, so no dust is stranded in the contract. A single-sponsor
/// milestone is the degenerate case: contribution 0's amount equals
/// `total_budget`, so the share is exactly `remaining_budget`.
fn refund_remaining_budget(
    env: &Env,
    milestone_id: u64,
    milestone: &Milestone,
) -> Result<(), Error> {
    let remaining = milestone.remaining_budget;
    if remaining <= 0 {
        return Ok(());
    }

    let token_client = token::Client::new(env, &milestone.token);
    let contract_address = env.current_contract_address();

    let mut shares: Vec<(Address, i128)> = Vec::new(env);
    let mut order: Vec<(u32, i128, Address)> = Vec::new(env);
    let mut allocated: i128 = 0;

    for i in 0..milestone.contributor_count {
        let contribution_key = DataKey::Contribution(milestone_id, i);
        let contribution: Contribution = env.storage().persistent().get(&contribution_key).unwrap();
        let numerator = remaining * contribution.amount;
        let share = numerator / milestone.total_budget;
        let remainder = numerator % milestone.total_budget;
        allocated += share;
        shares.push_back((contribution.sponsor.clone(), share));
        order.push_back((order.len(), remainder, contribution.sponsor));
    }

    let dust = remaining - allocated;
    if dust > 0 {
        sort_remainders_desc(&mut order);
        for k in 0..dust as u32 {
            let (index, _, _) = order.get(k).unwrap();
            let (recipient, share) = shares.get(index).unwrap();
            shares.set(index, (recipient, share + 1));
        }
    }

    for (recipient, share) in shares.iter() {
        if share > 0 {
            token_client.transfer(&contract_address, &recipient, &share);
        }
    }

    Ok(())
}

fn require_admin(env: &Env) -> Result<Address, Error> {
    mergefi_common::require_admin::<DataKey>(env).ok_or(Error::NotInitialized)
}

fn extend_ttl(env: &Env, key: &DataKey) {
    mergefi_common::extend_ttl(env, key);
}


