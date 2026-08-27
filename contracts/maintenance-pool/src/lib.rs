//! MergeFi Recurring Maintenance Pool Contract
//!
//! Unlike the escrow contract (funds tied to a single issue) or the
//! milestones contract (a lump sum allocated across issues in a release),
//! a maintenance pool is a standing balance tied to a repository or org.
//! Sponsors can deposit into it repeatedly over time; maintainers draw
//! down rewards for ongoing maintenance-type work as authorized by the
//! backend oracle, which tracks off-chain maintenance activity.
#![no_std]

mod error;
mod types;

#[cfg(test)]
mod test;

use error::Error;
use soroban_sdk::{contract, contractimpl, token, Address, Env};
use types::{DataKey, Deposit, MaintenancePool};

pub const BPS_DENOMINATOR: i128 = 10_000;

#[contract]
pub struct MaintenancePoolContract;

#[contractimpl]
impl MaintenancePoolContract {
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

    /// Sponsor deposits `amount` of `token` into the pool identified by
    /// `pool_id` (an off-chain-assigned id for a repo/org). Creates the
    /// pool on first deposit; subsequent deposits must use the same token.
    /// Requires sponsor authorization. Every deposit is recorded so the
    /// full contribution history can be queried.
    pub fn deposit(
        env: Env,
        pool_id: u64,
        sponsor: Address,
        token: Address,
        amount: i128,
    ) -> Result<(), Error> {
        sponsor.require_auth();

        if amount <= 0 {
            return Err(Error::InvalidAmount);
        }

        let pkey = DataKey::Pool(pool_id);
        let mut pool: MaintenancePool = match env.storage().persistent().get(&pkey) {
            Some(p) => p,
            None => MaintenancePool {
                token: token.clone(),
                balance: 0,
                total_deposited: 0,
                total_withdrawn: 0,
                total_fees_collected: 0,
                created_at: env.ledger().timestamp(),
                deposit_count: 0,
            },
        };

        if pool.deposit_count > 0 && pool.token != token {
            return Err(Error::TokenMismatch);
        }

        let token_client = token::Client::new(&env, &token);
        token_client.transfer(&sponsor, env.current_contract_address(), &amount);

        pool.balance += amount;
        pool.total_deposited += amount;
        let index = pool.deposit_count;
        pool.deposit_count += 1;

        env.storage().persistent().set(&pkey, &pool);
        extend_ttl(&env, &pkey);

        let dkey = DataKey::Deposit(pool_id, index);
        env.storage().persistent().set(
            &dkey,
            &Deposit {
                sponsor,
                amount,
                timestamp: env.ledger().timestamp(),
            },
        );
        extend_ttl(&env, &dkey);

        // Refresh every prior deposit sub-record so older entries don't
        // archive while the pool stays active with recurring new deposits.
        // The newly-written record at `index` was already extended above;
        // the loop covers the full range so all historical records are refreshed.
        for i in 0..pool.deposit_count {
            extend_ttl(&env, &DataKey::Deposit(pool_id, i));
        }

        Ok(())
    }

    /// Admin-only: pays `amount` (minus the protocol fee) out of the pool
    /// to `recipient` (a maintainer), as authorized off-chain by the
    /// backend oracle for completed maintenance work. Rejects if the pool
    /// balance is insufficient.
    pub fn withdraw(env: Env, pool_id: u64, recipient: Address, amount: i128) -> Result<(), Error> {
        require_admin(&env)?.require_auth();

        if amount <= 0 {
            return Err(Error::InvalidAmount);
        }

        let pkey = DataKey::Pool(pool_id);
        let mut pool: MaintenancePool = env
            .storage()
            .persistent()
            .get(&pkey)
            .ok_or(Error::PoolNotFound)?;

        if amount > pool.balance {
            return Err(Error::InsufficientBalance);
        }

        let fee_bps: u32 = env
            .storage()
            .instance()
            .get(&DataKey::FeeBps)
            .ok_or(Error::NotInitialized)?;
        let fee = amount * (fee_bps as i128) / BPS_DENOMINATOR;
        let payout = amount - fee;

        let treasury: Address = env.storage().instance().get(&DataKey::Treasury).unwrap();
        let token_client = token::Client::new(&env, &pool.token);
        let contract_address = env.current_contract_address();

        if fee > 0 {
            token_client.transfer(&contract_address, &treasury, &fee);
            // record collected fee on-chain for auditability
            pool.total_fees_collected += fee;
        }
        token_client.transfer(&contract_address, &recipient, &payout);

        pool.balance -= amount;
        pool.total_withdrawn += amount;
        env.storage().persistent().set(&pkey, &pool);
        extend_ttl(&env, &pkey);

        // Refresh all deposit sub-records on every withdrawal so historical
        // deposit records stay queryable across a long-running pool's lifetime.
        for i in 0..pool.deposit_count {
            extend_ttl(&env, &DataKey::Deposit(pool_id, i));
        }

        Ok(())
    }

    /// Permissionless TTL refresh: re-extends `pool_id`'s persistent-storage
    /// TTL (and those of all its `Deposit` sub-records) by the standard flat
    /// bump, without touching any pool state. Exists because individual
    /// deposit entries have their own TTL and will archive independently of
    /// the parent `MaintenancePool` record if never re-extended.
    ///
    /// This matters most for the maintenance pool because it is explicitly
    /// designed to be open-ended and long-lived ("it never finishes") — the
    /// contract with the longest expected lifetime also has the largest
    /// accumulation of historical deposit records, each of which needs its
    /// own TTL refreshed to stay queryable. Without periodic `keep_alive`
    /// calls, `get_deposit` silently breaks for older records even while the
    /// pool itself remains fully active.
    ///
    /// Unlike `escrow::keep_alive`, pools have no deadline timestamp, so
    /// this applies the flat ~29-day bump rather than a deadline-scaled
    /// extension. Call it at least once within any ~29-day window to keep
    /// the full deposit history alive and enumerable.
    ///
    /// Callable by anyone and needs no authorization: it can only ever keep
    /// records alive longer, never change what they hold.
    pub fn keep_alive(env: Env, pool_id: u64) -> Result<(), Error> {
        let pkey = DataKey::Pool(pool_id);
        let pool: MaintenancePool = env
            .storage()
            .persistent()
            .get(&pkey)
            .ok_or(Error::PoolNotFound)?;

        extend_ttl(&env, &pkey);

        // Keep every deposit sub-record alive alongside the parent so the
        // full contribution history advertised by the README stays queryable.
        for i in 0..pool.deposit_count {
            extend_ttl(&env, &DataKey::Deposit(pool_id, i));
        }

        Ok(())
    }

    pub fn get_pool(env: Env, pool_id: u64) -> Result<MaintenancePool, Error> {
        env.storage()
            .persistent()
            .get(&DataKey::Pool(pool_id))
            .ok_or(Error::PoolNotFound)
    }

    pub fn get_deposit(env: Env, pool_id: u64, index: u32) -> Result<Deposit, Error> {
        env.storage()
            .persistent()
            .get(&DataKey::Deposit(pool_id, index))
            .ok_or(Error::PoolNotFound)
    }
}

fn require_admin(env: &Env) -> Result<Address, Error> {
    mergefi_common::require_admin::<DataKey>(env).ok_or(Error::NotInitialized)
}

fn extend_ttl(env: &Env, key: &DataKey) {
    mergefi_common::extend_ttl(env, key);
}
