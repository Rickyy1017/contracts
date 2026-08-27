use soroban_sdk::{contracttype, Address};

/// A recurring maintenance pool tied to a repository or org (identified by
/// an off-chain-assigned `pool_id`, e.g. a hash of "owner/repo"). Sponsors
/// can deposit into it repeatedly (recurring funding, not a single-issue
/// escrow); maintainers draw down rewards for ongoing maintenance work as
/// authorized by the backend oracle.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaintenancePool {
    pub token: Address,
    pub balance: i128,
    pub total_deposited: i128,
    pub total_withdrawn: i128,
    pub total_fees_collected: i128,
    pub created_at: u64,
    pub deposit_count: u32,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Deposit {
    pub sponsor: Address,
    pub amount: i128,
    pub timestamp: u64,
}

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Admin,
    Treasury,
    FeeBps,
    Pool(u64),
    Deposit(u64, u32), // (pool_id, deposit_index)
}

impl mergefi_common::AdminKey for DataKey {
    fn admin_key() -> Self {
        DataKey::Admin
    }
}
