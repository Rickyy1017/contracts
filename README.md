# MergeFi Contracts

**Where Open Source Meets Finance.**

MergeFi lets sponsors fund open-source work, maintainers turn GitHub issues
into paid bounties, and contributors get paid automatically when their work
is merged. GitHub stays the system of record for *code* — who opened what,
who merged what, what got approved. This repository is the *financial
layer*: a set of Soroban smart contracts on the Stellar network that hold
sponsor funds in escrow and release them according to rules that a
trusted, off-chain oracle (the sibling `mergefi-backend` service) reports.

Flow, end to end:

1. A sponsor funds a GitHub issue (or a milestone, or a repo's ongoing
   maintenance pool) by depositing a Stellar token into one of these
   contracts.
2. A maintainer marks the issue as bounty-eligible and a contributor does
   the work, exactly as they would on any other GitHub project.
3. `mergefi-backend` watches GitHub webhooks. When it sees the PR
   referencing the issue get merged/accepted, it calls `release` (or
   `release_issue`, or `withdraw`) on the relevant contract, authenticated
   as the contract's configured admin/oracle address.
4. The contract pays the contributor(s) — split across a team if the
   bounty had multiple collaborators — deducts a small protocol fee to the
   treasury, and marks the escrow as paid. Double-payment and
   already-refunded states are rejected at the contract level, so the
   worst the backend can do is retry a call safely.
5. If an issue is cancelled or nobody finishes the work before its
   deadline, the sponsor (or, after expiry, anyone) can trigger a refund.

GitHub remains the source of truth for *whether work happened*. These
contracts are deliberately dumb about that — they only know what the
oracle tells them — and focus entirely on holding and moving money
correctly.

## Why three contracts instead of one

The spec allows team-splits/milestones to be either separate contracts or
modules in one. This repo ships them as **three independent contract
crates** — `mergefi-escrow`, `mergefi-milestones`, `mergefi-maintenance-pool`
— reasoning:

- **Different lifecycles.** An escrow is single-issue, single-payout,
  bounded by a deadline. A milestone is a lump sum sliced across many
  issues in a release, closed once. A maintenance pool is open-ended and
  repeatedly topped up — it never "finishes." Cramming all three into one
  contract's storage model would mean one bloated `DataKey` enum and a lot
  of variants that don't apply to most calls.
- **Independent upgrade/audit surface.** If a bug is found in milestone
  allocation logic, you can fix and redeploy that contract without
  touching escrow funds that are mid-flight.
- **Team-splits are not a separate contract.** They're a parameter shape
  (`Vec<(Address, u32 basis_points)>`) accepted by `release` /
  `release_issue` in both the escrow and milestone contracts. A single
  bounty and a team bounty are the same code path; the only difference is
  how many recipients are in the vector.

The tradeoff: the basis-point split math and fee-deduction logic
(`compute_split`) is duplicated between `mergefi-escrow` and
`mergefi-milestones` rather than shared via a common library crate. For a
codebase this size the duplication is small and readable; the natural
next step if it grows is to extract a `mergefi-common` crate with shared
types/helpers, imported as a normal (non-contract) Rust dependency by each
contract crate. Noted under Roadmap.

### Cross-contract double-funding

Independence has a cost this section didn't previously name: **the three
contracts share no registry and never call each other**, so nothing
on-chain stops the same `issue_id` from being funded twice through two
different instruments — once via `escrow::fund(issue_id, ...)` and again
via `milestones::allocate(milestone_id, issue_id, ...)` for some release
milestone. Neither contract's storage namespace (`DataKey` in
`contracts/escrow/src/types.rs` vs `contracts/milestones/src/types.rs`)
overlaps with the other's, and neither contains a contract-id reference to,
or `Env::invoke_contract` call into, the other. Both can independently
reach `release`/`release_issue` and pay out in full for what is, off
GitHub, a single piece of work being compensated twice.

Three ways to close or accept this gap were considered:

- **A shared on-chain registry contract** — a fourth, minimal contract
  whose only job is "claim `issue_id` X for contract Y," called by `fund`
  and `allocate` before either proceeds. This closes the gap on-chain,
  but reintroduces the cross-contract calls this design otherwise avoids
  everywhere else, and makes all three contracts' liveness depend on a
  fourth one — exactly the coupling the "Independent upgrade/audit
  surface" reasoning above argues against.
- **A shared library crate with a common `DataKey` convention** — lower
  coupling than a live registry contract, but doesn't actually close the
  gap by itself: without a cross-contract call (or a single shared
  storage instance both contracts write to, which reintroduces the
  coupling above by another name), a shared *type* doesn't stop two
  independently-deployed contract instances from writing incompatible
  state that neither can see the other wrote.
- **Accept the gap on-chain; mitigate at the backend layer.**
  `mergefi-backend` already watches every `fund` and `allocate` call as
  the system of record for GitHub state, so it's the one component with
  a natural, already-required view of "is this issue committed anywhere"
  — and can refuse to originate a second commitment for an `issue_id` it
  already tracks as funded or allocated. No new contract, no new
  coupling.

**Decision: the third option.** It's the one actually consistent with
this section's own reasoning above — independence was chosen
deliberately, and a shared registry, on-chain or otherwise, reintroduces
the exact coupling that tradeoff was meant to avoid. The contracts
themselves make **no attempt to detect this collision**; `mergefi-backend`
is responsible for refusing to originate a second commitment for an
`issue_id` it already has on record as funded or allocated by either
contract. This is a known, accepted limitation of the independent-
contracts design, not an oversight: if `mergefi-backend`'s own database is
ever wrong, out of sync, or bypassed, nothing on-chain provides a second
line of defense against the same issue being paid out twice through two
different instruments. See the within-`mergefi-milestones`
double-allocation gap (narrower, single-contract-scoped, and fixable
independently of this cross-contract question) for the more contained
sibling of this issue.

### Split rounding and dust

Team payouts use integer token amounts, so `distributable * bps / 10000`
can leave rounding dust. Earlier versions assigned all accumulated dust to
the final recipient in the caller-supplied vector. That avoided stranded
funds, but made recipient order economically relevant.

`compute_split` now uses a largest-remainder allocation in both escrow and
milestone releases:

- each recipient first receives `floor(distributable * bps / 10000)`;
- the remaining dust is always less than `recipients.len()` token-minor
  units, because the basis points sum to exactly 10000;
- those units are assigned to the largest fractional remainders, with
  stable input-order tie-breaking.

This preserves the invariant that `sum(shares) == distributable` without
systematically rewarding the final recipient.

## Contracts

### 1. `contracts/escrow` — `mergefi-escrow`

Core single-issue bounty escrow.

```rust
fn initialize(env, admin: Address, treasury: Address, fee_bps: u32) -> Result<(), Error>;
fn fund(env, issue_id: u64, sponsor: Address, token: Address, amount: i128, deadline: u64) -> Result<(), Error>;
fn contribute(env, issue_id: u64, sponsor: Address, amount: i128) -> Result<(), Error>;
fn release(env, issue_id: u64, recipients: Vec<(Address, u32)>) -> Result<(), Error>;
fn refund(env, issue_id: u64) -> Result<(), Error>;
fn extend_deadline(env, issue_id: u64, caller: Address, new_deadline: u64) -> Result<(), Error>;
fn keep_alive(env, issue_id: u64) -> Result<(), Error>;
fn get_escrow(env, issue_id: u64) -> Result<Escrow, Error>;
fn get_contribution(env, issue_id: u64, index: u32) -> Result<Contribution, Error>;
fn get_admin(env) -> Result<Address, Error>;
fn get_treasury(env) -> Result<Address, Error>;
fn get_fee_bps(env) -> Result<u32, Error>;
```

- `fund`: `sponsor.require_auth()`. Transfers `amount` of `token` from the
  sponsor into the contract and *creates* the escrow. One escrow per
  `issue_id` — a second `fund` call on the same id is rejected
  (`AlreadyFunded`); every sponsor after the first uses `contribute`
  instead.
- `contribute`: `sponsor.require_auth()`. Adds an additional sponsor's
  funds to an already-`fund`ed escrow — this is how crowdfunding a single
  `issue_id` across several sponsors works. Uses the token already
  recorded on the escrow (no `token` param, so a top-up can't silently use
  a different asset). Each contribution is recorded individually
  (`Contribution { sponsor, amount }`, queryable via `get_contribution`)
  so `refund` can return each sponsor's own amount to their own address.
  Capped at `MAX_SPONSORS` (20) distinct contributions per escrow
  (`TooManySponsors` otherwise). Rejects `AlreadyPaid` / `AlreadyRefunded`.
  See `docs/escrow-crowdfunding-design.md` for the full design reasoning.
- `release`: admin-only (`require_auth` on the stored admin/oracle
  address). `recipients` basis points must sum to exactly 10000 or the
  call is rejected (`InvalidSplit`) — this is how team-bounty payouts
  work, a single recipient at 10000 bps is just the single-payee case.
  Deducts `fee_bps` off the top to the treasury, then allocates the
  remainder with largest-remainder rounding so the payout matches the
  configured split exactly while avoiding stranded dust. Pays out the
  full crowdfunded total (`escrow.amount`, the sum of every
  contribution) regardless of how many sponsors contributed. Rejects
  `AlreadyPaid` / `AlreadyRefunded`.
- `refund`: every contributor gets back exactly what *they* put in, to
  their own address — not an even split and not the full amount to a
  single sponsor. Callable by the admin at any time (e.g. issue
  cancelled), or by *anyone* once `deadline` has passed — refund is
  sponsor-protective, so it deliberately doesn't require any contributor's
  own signature. Rejects `AlreadyPaid` / `AlreadyRefunded`. See
  `docs/refund-permissionless-analysis.md` for the economics/griefing
  analysis of the permissionless path.
- `extend_deadline`: `caller.require_auth()`, and `caller` must be *any*
  current contributor to the escrow (not necessarily the original `fund`
  caller) — rejected with `Unauthorized` otherwise. Lets a contributor
  push the shared `deadline` later if the group wants more time before
  `refund`'s permissionless path opens — `new_deadline` must be strictly
  later than both the stored deadline and the current ledger time, so it
  can only delay that window, never shorten it. Rejects `AlreadyPaid` /
  `AlreadyRefunded`. See `docs/escrow-crowdfunding-design.md` for why any
  single contributor (rather than unanimous or weighted consent) can
  extend. The record's persistent-storage TTL is scaled to (approximately)
  cover `new_deadline`, not just the flat ~29-day bump every other call
  applies — see "Data models" → "Storage & TTL" below for what that does
  and does not guarantee for a very far-future deadline.
- `keep_alive`: no authorization required — callable by anyone (the
  sponsor, a contributor, or an automated `mergefi-backend` job), and
  touches only the record's TTL, never `deadline` or `status`. Re-applies
  the same TTL scaling `extend_deadline` does, toward the record's
  *currently-stored* `deadline`. Exists because a single TTL-extension call
  is capped at Soroban's own persistent-entry ceiling (~1 year on a
  typically-configured network) — a `deadline` set beyond that ceiling
  needs this called again periodically to keep surviving toward it, since
  no single call can cover unlimited future time.

### 2. `contracts/milestones` — `mergefi-milestones`

Lump-sum budget shared across the issues in a release.

```rust
fn initialize(env, admin: Address, treasury: Address, fee_bps: u32) -> Result<(), Error>;
fn create_milestone(env, milestone_id: u64, sponsor: Address, token: Address, total_budget: i128) -> Result<(), Error>;
fn contribute(env, milestone_id: u64, sponsor: Address, amount: i128) -> Result<(), Error>;
fn allocate(env, milestone_id: u64, issue_id: u64, amount: i128) -> Result<(), Error>;
fn release_issue(env, milestone_id: u64, issue_id: u64, recipients: Vec<(Address, u32)>) -> Result<(), Error>;
fn cancel_milestone(env, milestone_id: u64) -> Result<(), Error>;
fn get_milestone(env, milestone_id: u64) -> Result<Milestone, Error>;
fn get_issue_status(env, milestone_id: u64, issue_id: u64) -> Result<IssueStatus, Error>;
fn get_contribution(env, milestone_id: u64, index: u32) -> Result<Contribution, Error>;
```

- `create_milestone`: the original sponsor deposits `total_budget` once;
  the pool starts fully unallocated (`remaining_budget == total_budget`).
  One milestone per `milestone_id` — a second `create_milestone` on the
  same id is rejected; every sponsor after the first uses `contribute`
  instead.
- `contribute`: `sponsor.require_auth()`. Adds an additional sponsor's
  funds to an already-`create_milestone`d pool — this is how crowdfunding
  a release across several sponsors works. Uses the token already
  recorded on the milestone (no `token` param, so a top-up can't silently
  use a different asset). New funds arrive unallocated, so both
  `total_budget` and `remaining_budget` grow by the contribution. Each
  contribution is recorded individually (`Contribution { sponsor, amount
  }`, queryable via `get_contribution`, with the original funder always
  at index 0) so a cancellation refund can return each sponsor's
  proportional share to their own address. Capped at `MAX_SPONSORS` (20)
  distinct contributions per milestone (`TooManySponsors` otherwise).
  Rejects `MilestoneClosed`. See
  `docs/milestones-crowdfunding-design.md` for the full design reasoning.
- `allocate`: admin-only. Reserves a slice of `remaining_budget` for a
  specific `issue_id`. Over-allocating past what's left is rejected
  (`OverAllocation`); allocating an issue twice is rejected
  (`IssueAlreadyAllocated`).
- `release_issue`: admin-only, same split/fee mechanics as escrow's
  `release`, but draws from the issue's pre-reserved allocation rather
  than a fresh deposit. Rejects double release (`IssueAlreadyReleased`).
- `cancel_milestone`: admin-only. Refunds whatever is left in
  `remaining_budget` (i.e. never allocated) back to the contributors **in
  proportion to what each one put in** — `remaining_budget *
  contribution / total_budget`, with largest-remainder rounding so no
  dust is stranded — and closes the milestone; already-released issues
  are unaffected since their funds already left the contract. A
  single-sponsor milestone is the degenerate case (the whole remainder
  goes back to the one sponsor, as before). See
  `docs/milestones-crowdfunding-design.md` for the proportional
  accounting and why it stays correct across allocate/release cycles.

### 3. `contracts/maintenance-pool` — `mergefi-maintenance-pool`

Recurring, open-ended funding tied to a repo/org rather than one issue.

```rust
fn initialize(env, admin: Address, treasury: Address, fee_bps: u32) -> Result<(), Error>;
fn deposit(env, pool_id: u64, sponsor: Address, token: Address, amount: i128) -> Result<(), Error>;
fn withdraw(env, pool_id: u64, recipient: Address, amount: i128) -> Result<(), Error>;
fn get_pool(env, pool_id: u64) -> Result<MaintenancePool, Error>;
fn get_deposit(env, pool_id: u64, index: u32) -> Result<Deposit, Error>;
```

- `pool_id` is an off-chain-assigned identifier for a repo or org (e.g. a
  hash of `owner/repo`, minted by `mergefi-backend`) — not tied to any
  single issue.
- `deposit`: any sponsor can call repeatedly; the pool is created
  implicitly on first deposit. All deposits after the first must use the
  same `token` (`TokenMismatch` otherwise). Every deposit is recorded
  (`Deposit { sponsor, amount, timestamp }`, indexed by an incrementing
  counter) so the full contribution history is queryable.
- `withdraw`: admin-only — the backend authorizes a maintainer draw-down
  for completed maintenance work (this is *not* tied to a specific PR
  merge the way escrow/milestones are; it's off-chain-adjudicated
  "maintenance credit"). Deducts the fee, rejects if `amount` exceeds the
  pool's current balance (`InsufficientBalance`).

## Data models

```rust
// escrow
pub enum EscrowStatus { Funded, Paid, Refunded }
pub struct Escrow {
    pub token: Address,
    pub amount: i128, // sum of every contribution accepted so far
    pub total_fees_collected: i128, // cumulative protocol fees charged
    pub status: EscrowStatus,
    pub created_at: u64,
    pub deadline: u64,
    pub contributor_count: u32, // enumerate via get_contribution(0..contributor_count)
}
pub struct Contribution {
    pub sponsor: Address,
    pub amount: i128,
}

// milestones
pub struct Milestone {
    pub sponsor: Address, // original funder; always contribution index 0
    pub token: Address,
    pub total_budget: i128, // sum of every contribution accepted so far
    pub remaining_budget: i128, // unallocated remainder, refunded on cancel
    pub total_fees_collected: i128, // cumulative protocol fees charged
    pub created_at: u64,
    pub closed: bool,
    pub allocations: Map<u64, i128>, // issue_id -> allocated amount
    pub contributor_count: u32, // enumerate via get_contribution(0..contributor_count)
}
pub struct Contribution {
    pub sponsor: Address,
    pub amount: i128,
}
pub enum IssueStatus { Allocated, Released }

// maintenance-pool
pub struct MaintenancePool {
    pub token: Address,
    pub balance: i128,
    pub total_deposited: i128,
    pub total_withdrawn: i128,
    pub total_fees_collected: i128, // cumulative protocol fees charged
    pub created_at: u64,
    pub deposit_count: u32,
}
pub struct Deposit {
    pub sponsor: Address,
    pub amount: i128,
    pub timestamp: u64,
}
```

Each contract's config (`Admin`, `Treasury`, `FeeBps`) lives in **instance
storage** (small, always loaded with the contract). Per-issue/milestone/pool
records live in **persistent storage** keyed by an enum (`DataKey`) so they
survive independently and can be individually TTL-extended.

### Storage & TTL

Most persistent writes (`fund`, `contribute`, `release`, `refund`, and the
equivalent calls in `milestones`/`maintenance-pool`) re-bump TTL with a flat
`extend_ttl(..., 100_000, 500_000)` — about **~29 days** of real time at
Stellar's ~5-second ledger close (`500_000 × 5 ÷ 86,400`), tuned for a
multi-month bounty/release lifecycle and re-applied on essentially every
interaction, so an active record's TTL keeps being pushed out in practice.

`escrow::extend_deadline` is the one place that flat bump used to create a
real gap: it lets a contributor push `deadline` arbitrarily far into the
future, but the flat ~29-day TTL bump didn't scale with it — a
`new_deadline` a year out bought no more actual on-chain survivability than
one a week out, silently, with nothing in the interface warning that the
two aren't related (MergeFi/contracts#56). `extend_deadline` (and the
permissionless `keep_alive`) now scale the TTL bump toward the target
deadline instead, capped at Soroban's own real ceiling on how far a single
call can extend a persistent entry (`env.ledger().max_live_until_ledger()`,
roughly a year on a typically-configured network — a genuine protocol
limit, not a number this codebase chose).

**What this does *not* guarantee:** a `new_deadline` set further out than
that ceiling still only receives the maximum extension a single call can
grant — the record is not guaranteed to survive all the way to a
multi-year deadline from one `extend_deadline` call. For that, call
`keep_alive` again periodically (well within the ceiling's own window) —
the sponsor, any contributor, or an automated `mergefi-backend` job can do
this; it needs no authorization since it can only ever keep a record alive
longer, never change what it holds or who it pays. If a record's TTL does
lapse anyway (no one called anything on it for the full ~29-day-to-~1-year
window, depending which bump last applied), it moves toward archival and,
once archived, becomes fully inaccessible — unreadable via `get_escrow`,
unreleasable, unrefundable — until someone submits a `RestoreFootprint`
operation. Restoring an archived entry is a real Soroban operation but is
not automated by anything in this repo's scripts today.

## Security model

- **Admin / oracle authorization.** One `Address` (`admin`), set once at
  `initialize` and immutable thereafter, represents the `mergefi-backend`
  service. All state-changing calls that assert "the reported off-chain
  event actually happened" (`release`, `release_issue`, early `refund`,
  `allocate`, `withdraw`) require `admin.require_auth()`. Soroban's
  `require_auth` means the backend's signing key must actually authorize
  that specific invocation — there's no way to spoof it by simply calling
  the contract from an arbitrary account.
- **Sponsor authorization.** `fund`, `create_milestone`, and `deposit`
  require the sponsor's own `require_auth()` — a backend key can never
  move a sponsor's funds *into* escrow on their behalf without their
  signature (only *out*, once deposited, per the payout rules above).
- **No re-initialization.** `initialize` checks `storage().instance().has(&DataKey::Admin)`
  and rejects with `AlreadyInitialized` if already set, so admin/treasury/fee
  can't be silently swapped out post-deployment by calling `initialize` again.
- **`initialize` requires the named admin's own authorization.** All
  three contracts' `initialize` call `admin.require_auth()`, so nobody
  can name a third-party address as admin without that address's
  consent. This is a narrower guarantee than it might sound like — it
  does **not** prevent an attacker from front-running the legitimate
  deployer's `initialize` call by naming *themselves* as admin instead,
  since they can trivially authorize their own address. See
  `docs/access-control-audit.md` for the full analysis and why closing
  that race requires a structural change (an atomic deploy+init
  constructor) rather than an in-contract check.
- **Fee mechanics.** `fee_bps` is basis points (1/100 of a percent) out of
  10000, validated `<= 10000` at `initialize`. It's deducted from the top
  of every payout (`release`, `release_issue`, `withdraw`) before the
  remainder is split among recipients — the treasury is paid in the same
  transaction as the recipients, so there's no separate "sweep fees"
  step that could be skipped.
- **Replay / double-spend protection.** Every escrow/milestone-issue
  carries an explicit status (`Funded → Paid | Refunded`, or
  `Allocated → Released`). `release`/`release_issue`/`refund` all check
  this status first and reject (`AlreadyPaid`, `AlreadyRefunded`,
  `IssueAlreadyReleased`) rather than trusting the caller not to invoke
  twice — this is what makes it safe for the backend to retry a
  failed/uncertain call.
- **Deadline handling.** `deadline` is a ledger timestamp (`env.ledger().timestamp()`,
  Unix seconds) set by the sponsor at `fund` time. Before the deadline,
  only the admin can force a refund (e.g. issue explicitly cancelled).
  After the deadline, *anyone* can call `refund` — it always pays out to
  the original sponsor address stored in the record, never the caller, so
  permissionless-after-expiry doesn't create a theft vector; it just
  removes the backend as a liveness dependency for getting stuck funds
  back. The sponsor can call `extend_deadline` at any point before the
  escrow is paid or refunded to push their own deadline later (never
  earlier) if they want more time — see
  `docs/refund-permissionless-analysis.md`.
- **Split validation.** Basis points across all recipients in a `release`
  call must sum to exactly `10_000`; anything else is rejected
  (`InvalidSplit`) before any tokens move. An empty recipients vector is
  also rejected rather than silently paying no one.
- **Token transfers** go through the standard Soroban token interface
  (`soroban_sdk::token::Client`, compatible with the Stellar Asset
  Contract and any SEP-41-compliant custom token), so these contracts
  work with any asset issued on Stellar, not just XLM.

## Backend integration (`mergefi-backend`)

`mergefi-backend` is expected to hold the `admin` keypair for each
deployed contract (escrow, milestones, maintenance-pool — these can share
one admin key or use separate ones per environment) and drive them over
Soroban RPC using `stellar-sdk` / `soroban-client` (or the Rust
`soroban-cli`/`soroban_rpc` client, if the backend is Rust). Typical
integration points:

1. **On issue funded (Stellar payment observed / sponsor UI flow):**
   nothing to do here — `fund`/`create_milestone`/`deposit` are called
   directly by the sponsor's wallet, not by the backend. The backend just
   indexes the resulting contract events / `get_escrow` state to reflect
   funding status in the product UI.
2. **On PR merged (GitHub webhook):** backend resolves which
   `issue_id`/`milestone_id` the merged PR is tied to, resolves the
   contributor(s) and their split (single payee, or a team split it
   computed from co-author metadata / maintainer input), builds a
   `release` / `release_issue` invocation, signs it with the admin key,
   and submits it via Soroban RPC (`simulateTransaction` →
   `sendTransaction`). It should treat the call as idempotent — the
   contract itself rejects double-release, so a retry after a network
   timeout is safe to just re-send.
3. **On issue closed without merge / deadline passed:** backend calls
   `refund` (admin path) or lets it sit — since refund is permissionless
   after `deadline`, the backend doesn't strictly need to call it at all
   once expired, though it likely does for UX (so sponsors don't have to
   trigger it manually).
4. **Maintenance draw-downs:** backend authorizes `withdraw` against a
   pool when it determines (via its own off-chain rules — e.g. a
   maintainer's recurring stipend, or a one-off review-load payout) that
   a maintainer should be paid from the standing pool.
5. **Reading state:** all `get_*` view functions are free simulated calls
   (no signature/fee) and are the primary way the backend/API layer keeps
   its own database in sync with on-chain truth after any write.

## Build, test, deploy

### Prerequisites

- Rust (this repo was built/tested against `rustc 1.95.0`).
- The `wasm32v1-none` target for building deployable contract wasm:
  `rustup target add wasm32v1-none`.
  (Soroban's host requires this target rather than the legacy
  `wasm32-unknown-unknown` on Rust 1.82+ — `soroban-sdk`'s build script
  will tell you this explicitly if you try the wrong one.)
- [`stellar-cli`](https://developers.stellar.org/docs/tools/cli/install-cli)
  (the successor to `soroban-cli`) for `contract deploy` /
  `contract invoke` against testnet/mainnet. **Not installed in the
  environment this repo was built in** — deploy steps below are
  documented but untested-in-this-session; contract compilation and all
  unit tests were verified without it.

### Commands

```sh
make build   # cargo build --target wasm32v1-none --release, all 3 contracts
make test    # cargo test --workspace (native target, no wasm needed)
make deploy  # example stellar contract deploy calls, see Makefile
```

Or directly:

```sh
cargo test --workspace
cargo build --target wasm32v1-none --release \
  -p mergefi-escrow -p mergefi-milestones -p mergefi-maintenance-pool
```

Verified in this session: `cargo test --workspace` is green on the native
`Env::default()` test target; run `make test` for the current pass count,
which changes as the suite evolves and should not be hardcoded here.
The `wasm32v1-none` release build was also verified — all three contracts
compile to `.wasm` in `target/wasm32v1-none/release/`.

### Deployed on Stellar testnet

All three contracts are deployed and initialized on testnet as of this
writing. `stellar-cli`'s HTTP client couldn't reach the RPC endpoint from
the environment this was deployed from (a local TLS/cert issue, not a
Stellar-side problem), so `scripts/deploy.mjs` and `scripts/invoke.mjs`
(thin wrappers around `@stellar/stellar-sdk`) were used instead to
perform the same upload → create-contract → initialize flow the CLI
would otherwise do.

| Contract | Contract ID |
|---|---|
| `mergefi-escrow` | `CAY77D2SFDVQYONSPYHOEWARE3UIWQDYHWWI2WXNPFBLBKR2Q4GEWXFB` |
| `mergefi-milestones` | `CBBRLSL6TM6XCNP2XBVT4GFHJ3NNPFKI2BCZQJ4U3TI7GV7DO2F2HG6F` |
| `mergefi-maintenance-pool` | `CD46U7WTEM2I77TXQI2VIBRQXOHEFEYYR2XFA7OVGTXX5M2F7Z3ZQOX2` |

All three were initialized with the same admin/treasury address
(`GBUXADZJ7O4NM7S7CDZYVXGP37M772D2TYMFBT2QFH2JSRCFEJPAVW5N`, a
throwaway testnet-only account) and a 250 bps (2.5%) treasury fee.
View them on
[Stellar Expert](https://stellar.expert/explorer/testnet/contract/CAY77D2SFDVQYONSPYHOEWARE3UIWQDYHWWI2WXNPFBLBKR2Q4GEWXFB).

To redeploy (e.g. after a contract change), once `stellar-cli` has
working network access:

```sh
stellar keys generate mergefi-admin --network testnet --fund
stellar contract deploy \
  --wasm target/wasm32v1-none/release/mergefi_escrow.wasm \
  --source mergefi-admin \
  --network testnet
# then, e.g.
stellar contract invoke \
  --id <CONTRACT_ID> --source mergefi-admin --network testnet \
  -- initialize --admin <ADMIN_G...> --treasury <TREASURY_G...> --fee_bps 250
```

Or, in an environment where the CLI's own network calls are blocked but
plain Node.js `fetch` works (as was the case here):

```sh
node scripts/deploy.mjs <SECRET_KEY> target/wasm32v1-none/release/mergefi_escrow.wasm escrow
node scripts/invoke.mjs <SECRET_KEY> <CONTRACT_ID> initialize \
  address:<ADMIN_G...> address:<TREASURY_G...> u32:250
```

Both scripts default to Stellar testnet but can be pointed at other networks
via environment variables:

```sh
RPC_URL=https://soroban-mainnet.stellar.org \
NETWORK_PASSPHRASE=Public Global Stellar Network ; September 2015 \
  node scripts/deploy.mjs <SECRET_KEY> target/wasm32v1-none/release/mergefi_escrow.wasm escrow
```

## Roadmap

- Extract shared split/fee math (`compute_split`) into a common
  non-contract Rust crate to remove the duplication between
  `mergefi-escrow` and `mergefi-milestones` noted above.
- Emit contract events (`env.events().publish(...)`) on fund/release/refund
  so the backend can index state changes from the ledger directly instead
  of only polling `get_*` view calls.
- Consider a two-key admin model (oracle key for routine `release` calls,
  separate higher-trust key for `initialize`/admin rotation) once the
  contracts move past initial testnet iteration.
- Support partial milestone/pool refunds and issue re-allocation
  (currently `allocate` is one-shot per issue).
- Add integration tests against `stellar-cli`'s local sandbox network
  once available, to validate actual RPC-level invocation from a
  `mergefi-backend`-shaped client rather than only `testutils`.
- Revisit the accepted cross-contract double-funding gap (see "Why three
  contracts instead of one" → "Cross-contract double-funding") if backend-
  layer mitigation ever proves insufficient in practice — the shared
  on-chain registry contract considered and rejected there remains the
  fallback if a stronger, on-chain guarantee becomes worth the coupling
  cost.
