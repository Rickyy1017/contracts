use soroban_sdk::{contracttype, Address, Map};

/// A milestone pools one or more sponsors' contributions into a lump-sum
/// budget shared across several issues that belong to the same release.
/// Each issue is allocated a slice of the budget up front; as issues
/// resolve, their allocation is paid out and deducted from the pool.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Milestone {
    /// The original funder (the address that called `create_milestone`).
    /// Retained for backward compatibility with the single-sponsor API; it
    /// is always identical to contribution index `0` in the contribution
    /// ledger and never changes, so it cannot drift out of sync.
    pub sponsor: Address,
    pub token: Address,
    /// Running sum of every accepted contribution (starts at the
    /// `create_milestone` deposit, grows with each `contribute` call).
    pub total_budget: i128,
    /// Unallocated remainder of the pool. Starts equal to `total_budget`,
    /// shrinks with each `allocate`, and (as of the future deallocate
    /// issue) can grow again — it is the amount a `cancel_milestone`
    /// refund returns to contributors, proportionally.
    pub remaining_budget: i128,
    /// Running total of protocol fees collected by this milestone (auditability).
    pub total_fees_collected: i128,
    pub created_at: u64,
    pub closed: bool,
    /// issue_id -> allocated amount (0 once released and removed from the
    /// "open" set is not necessary; we track release via `IssueStatus`).
    pub allocations: Map<u64, i128>,
    /// Number of distinct contributions recorded (`1` for a single-sponsor
    /// milestone); enumerate the ledger via
    /// `get_contribution(0..contributor_count)`.
    pub contributor_count: u32,
}

/// One sponsor's contribution toward a (possibly crowdfunded) milestone.
/// Stored under its own `DataKey::Contribution(milestone_id, index)`
/// entry rather than inline in a `Vec` on `Milestone` itself, mirroring
/// `escrow::Contribution` and `maintenance-pool::Deposit` — keeps each
/// storage entry small and bounded instead of one growing collection that
/// has to be read/written in full on every access. See
/// `docs/milestones-crowdfunding-design.md`.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Contribution {
    pub sponsor: Address,
    pub amount: i128,
}

#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IssueStatus {
    Allocated,
    Released,
}

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Admin,
    Treasury,
    FeeBps,
    Milestone(u64),
    IssueStatus(u64, u64),  // (milestone_id, issue_id)
    Contribution(u64, u32), // (milestone_id, contribution_index)
}

impl mergefi_common::AdminKey for DataKey {
    fn admin_key() -> Self {
        DataKey::Admin
    }
}
