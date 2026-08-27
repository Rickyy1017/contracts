use soroban_sdk::{contracttype, Address};

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EscrowStatus {
    Funded,
    Paid,
    Refunded,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Escrow {
    pub token: Address,
    pub amount: i128,
    pub total_fees_collected: i128,
    pub status: EscrowStatus,
    pub created_at: u64,
    pub deadline: u64,
    pub contributor_count: u32,
}

/// One sponsor's contribution toward a (possibly crowdfunded) escrow.
/// Stored under its own `DataKey::Contribution(issue_id, index)` entry
/// rather than inline in a `Vec` on `Escrow` itself, mirroring
/// `maintenance-pool::Deposit` — keeps each storage entry small and
/// bounded instead of one growing collection that has to be read/written
/// in full on every access. See `docs/escrow-crowdfunding-design.md`.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Contribution {
    pub sponsor: Address,
    pub amount: i128,
}

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Admin,
    Treasury,
    FeeBps,
    Escrow(u64),
    Contribution(u64, u32), // (issue_id, contribution_index)
}

impl mergefi_common::AdminKey for DataKey {
    fn admin_key() -> Self {
        DataKey::Admin
    }
}
