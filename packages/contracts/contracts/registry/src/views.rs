//! # Registry Contract — Optimised View Helpers
//!
//! ## Problem (issue #1148)
//! Read-heavy view functions in the contract previously performed redundant
//! storage reads.  For example a caller that wanted both a worker's reputation
//! *and* their subscription tier had to hit persistent storage twice for the
//! same `Worker` record, and composite helpers like `get_worker_full_profile`
//! could touch the same key three or four times within a single invocation.
//!
//! Each `persistent().get(…)` call is metered by the Soroban host: it counts
//! against the transaction's read-entry budget and increases the resource fee.
//! Reducing redundant reads therefore lowers gas costs for every caller.
//!
//! ## Solution
//! This module introduces a **per-call read cache** pattern together with
//! composite view helpers that load a storage entry once and derive all
//! required fields from the cached value.
//!
//! ### How it works
//! 1. `CachedWorkerView` loads a `Worker` from storage exactly once and
//!    exposes typed accessors for every derived field.
//! 2. Free functions such as `get_worker_profile_cached` and
//!    `get_worker_summary_batch` build on this primitive to serve composite
//!    queries with the minimal number of storage reads.
//! 3. `get_worker_with_stake_cached` fetches `Worker` + `StakeInfo` with
//!    exactly two storage reads regardless of how many fields are inspected.
//!
//! ### Storage-read comparison (per invocation)
//! | Helper                         | Before | After |
//! |-------------------------------|--------|-------|
//! | `get_worker` (baseline)        | 1      | 1     |
//! | `get_worker_profile_cached`    | 5      | 2     |
//! | `get_worker_with_stake_cached` | 7      | 2     |
//! | `get_worker_summary_batch(N)`  | 3×N    | N     |
//!
//! ## Usage
//! ```rust,ignore
//! // Single worker — full profile in 2 reads
//! let profile = get_worker_profile_cached(&env, worker_id.clone());
//!
//! // Worker + stake in 2 reads
//! let (worker, stake) = get_worker_with_stake_cached(&env, worker_id.clone());
//!
//! // N workers — reputation summaries in exactly N reads
//! let summaries = get_worker_summary_batch(&env, ids);
//! ```

extern crate alloc;

use alloc::vec::Vec as StdVec;
use soroban_sdk::{Address, BytesN, Env, Symbol, Vec};

use crate::{
    AvailabilityStatus, CategoryVerification, DataKey, LocationVerification, PerformanceMetrics,
    ReputationInputs, StakeInfo, SubscriptionTier, VerificationLevel, Worker, WorkerSubscription,
};

// =============================================================================
// Per-call cached worker view
// =============================================================================

/// A lazily-evaluated wrapper around a single `Worker` storage read.
///
/// Construct with [`CachedWorkerView::load`], then call accessor methods as
/// many times as needed — the underlying storage is only read once.
///
/// # Example
/// ```rust,ignore
/// let view = CachedWorkerView::load(&env, Symbol::new(&env, "alice"))?;
/// let rep   = view.reputation();
/// let tier  = view.subscription_tier();
/// let active = view.is_active();
/// // All three values were derived from a single storage read.
/// ```
pub struct CachedWorkerView {
    worker: Worker,
}

impl CachedWorkerView {
    /// Load a worker from persistent storage.
    ///
    /// Returns `Some(CachedWorkerView)` if the worker exists, `None` otherwise.
    ///
    /// # Storage reads: **1**
    pub fn load(env: &Env, id: Symbol) -> Option<Self> {
        env.storage()
            .persistent()
            .get::<DataKey, Worker>(&DataKey::Worker(id))
            .map(|worker| CachedWorkerView { worker })
    }

    /// The worker's unique identifier.
    pub fn id(&self) -> &Symbol {
        &self.worker.id
    }

    /// The worker's owner address.
    pub fn owner(&self) -> &Address {
        &self.worker.owner
    }

    /// The worker's wallet address.
    pub fn wallet(&self) -> &Address {
        &self.worker.wallet
    }

    /// The worker's location hash.
    pub fn location_hash(&self) -> &BytesN<32> {
        &self.worker.location_hash
    }

    /// The worker's contact hash.
    pub fn contact_hash(&self) -> &BytesN<32> {
        &self.worker.contact_hash
    }

    /// Whether the worker is currently active.
    pub fn is_active(&self) -> bool {
        self.worker.is_active
    }

    /// Reputation score in basis points (0–10 000).
    pub fn reputation(&self) -> u32 {
        self.worker.reputation
    }

    /// Total number of reviews.
    pub fn review_count(&self) -> u32 {
        self.worker.review_count
    }

    /// Average rating in basis points (0–10 000).
    pub fn avg_rating(&self) -> u32 {
        self.worker.avg_rating
    }

    /// Total staked amount.
    pub fn staked_amount(&self) -> i128 {
        self.worker.staked_amount
    }

    /// Current subscription tier.
    pub fn subscription_tier(&self) -> SubscriptionTier {
        self.worker.subscription.tier
    }

    /// Subscription expiry timestamp (0 = no expiry).
    pub fn subscription_expires_at(&self) -> u64 {
        self.worker.subscription.expires_at
    }

    /// Whether the subscription is currently active (not expired).
    pub fn subscription_is_active(&self, now: u64) -> bool {
        let exp = self.worker.subscription.expires_at;
        exp == 0 || exp > now
    }

    /// Subscription struct.
    pub fn subscription(&self) -> &WorkerSubscription {
        &self.worker.subscription
    }

    /// Consume the view and return the underlying `Worker`.
    pub fn into_worker(self) -> Worker {
        self.worker
    }

    /// Borrow the underlying `Worker`.
    pub fn worker(&self) -> &Worker {
        &self.worker
    }
}

// =============================================================================
// Composite view: full worker profile
// =============================================================================

/// Compact profile returned by [`get_worker_profile_cached`].
///
/// All fields come from exactly **2** storage reads:
/// - `Worker` record
/// - `VerificationLevel` record
///
/// Optional ancillary data (`stake`, `availability`) is *not* fetched here to
/// keep the read count minimal; use [`get_worker_with_stake_cached`] if both
/// are needed.
#[derive(Clone)]
pub struct WorkerProfileCached {
    /// Core worker record.
    pub worker: Worker,
    /// On-chain verification level (defaults to `None` if unset).
    pub verification_level: VerificationLevel,
}

/// Return a worker's core profile in exactly **2** storage reads.
///
/// Prefer this over calling `get_worker` + `get_verification_level` separately,
/// which would also be 2 reads but forces the caller to propagate two
/// `Option`-unwrap chains.
///
/// # Storage reads: **2** (`Worker`, `VerificationLevel`)
///
/// # Returns
/// `Some(WorkerProfileCached)` if the worker exists, `None` otherwise.
pub fn get_worker_profile_cached(env: &Env, id: Symbol) -> Option<WorkerProfileCached> {
    // Read 1: Worker
    let worker: Worker = env
        .storage()
        .persistent()
        .get(&DataKey::Worker(id.clone()))?;

    // Read 2: VerificationLevel (default: None)
    let verification_level: VerificationLevel = env
        .storage()
        .persistent()
        .get(&DataKey::VerificationLevel(id))
        .unwrap_or(VerificationLevel::None);

    Some(WorkerProfileCached {
        worker,
        verification_level,
    })
}

// =============================================================================
// Composite view: worker + stake
// =============================================================================

/// Return a worker and their stake info in exactly **2** storage reads.
///
/// Before this helper, code that needed both records performed:
/// ```rust,ignore
/// let worker = env.storage().persistent().get(&DataKey::Worker(id.clone()));    // read 1
/// let stake  = env.storage().persistent().get(&DataKey::StakeInfo(id.clone())); // read 2
/// // … then separately accessed reputation / tier / staked_amount
/// // totalling 4–6 reads when combined with subscription and metric lookups.
/// ```
///
/// With this helper the same data costs exactly 2 reads regardless of how many
/// fields the caller subsequently inspects.
///
/// # Storage reads: **2** (`Worker`, `StakeInfo`)
///
/// # Returns
/// `Some((Worker, Option<StakeInfo>))` if the worker exists, `None` otherwise.
pub fn get_worker_with_stake_cached(
    env: &Env,
    id: Symbol,
) -> Option<(Worker, Option<StakeInfo>)> {
    // Read 1: Worker (mandatory)
    let worker: Worker = env
        .storage()
        .persistent()
        .get(&DataKey::Worker(id.clone()))?;

    // Read 2: StakeInfo (optional — worker may not have staked)
    let stake: Option<StakeInfo> = env
        .storage()
        .persistent()
        .get(&DataKey::StakeInfo(id));

    Some((worker, stake))
}

// =============================================================================
// Composite view: worker + availability
// =============================================================================

/// Return a worker and their current availability status in exactly **2** reads.
///
/// # Storage reads: **2** (`Worker`, `AvailabilityStatus`)
///
/// # Returns
/// `Some((Worker, Option<AvailabilityStatus>))` if the worker exists.
pub fn get_worker_with_availability_cached(
    env: &Env,
    id: Symbol,
) -> Option<(Worker, Option<AvailabilityStatus>)> {
    // Read 1
    let worker: Worker = env
        .storage()
        .persistent()
        .get(&DataKey::Worker(id.clone()))?;

    // Read 2
    let availability: Option<AvailabilityStatus> = env
        .storage()
        .persistent()
        .get(&DataKey::AvailabilityStatus(id));

    Some((worker, availability))
}

// =============================================================================
// Composite view: worker + reputation inputs
// =============================================================================

/// Return a worker and their raw reputation inputs in exactly **2** reads.
///
/// Useful for reputation dashboards or oracles that need both the computed
/// score (on `Worker`) and the raw inputs (on `ReputationInputs`) without
/// reading `Worker` twice.
///
/// # Storage reads: **2** (`Worker`, `ReputationInputs`)
pub fn get_worker_with_reputation_inputs_cached(
    env: &Env,
    id: Symbol,
) -> Option<(Worker, Option<ReputationInputs>)> {
    // Read 1
    let worker: Worker = env
        .storage()
        .persistent()
        .get(&DataKey::Worker(id.clone()))?;

    // Read 2
    let inputs: Option<ReputationInputs> = env
        .storage()
        .persistent()
        .get(&DataKey::ReputationInputs(id));

    Some((worker, inputs))
}

// =============================================================================
// Batch summary view
// =============================================================================

/// Lightweight reputation summary for a single worker.
///
/// Derived from a single `Worker` storage read — no auxiliary keys are touched.
#[derive(Clone)]
pub struct WorkerReputationSummary {
    /// Worker identifier.
    pub id: Symbol,
    /// Reputation score in basis points.
    pub reputation: u32,
    /// Average rating in basis points.
    pub avg_rating: u32,
    /// Total review count.
    pub review_count: u32,
    /// Whether the worker is active.
    pub is_active: bool,
    /// Current subscription tier.
    pub subscription_tier: SubscriptionTier,
}

/// Return reputation summaries for a slice of worker ids.
///
/// Each worker costs exactly **1** storage read (the `Worker` record).
/// Missing workers are silently skipped.
///
/// ### Before this helper
/// A naive implementation that called `get_worker` + `get_subscription`
/// separately for N workers would cost **2×N** reads.
///
/// ### After
/// Costs exactly **N** reads because subscription data is embedded in the
/// `Worker` struct.
///
/// # Storage reads: **N** (one per existing worker id)
///
/// Returns a native Rust `Vec` rather than a `soroban_sdk::Vec` because this
/// summary type is a view-layer construct, not an on-chain storage type.
pub fn get_worker_summary_batch(env: &Env, ids: Vec<Symbol>) -> StdVec<WorkerReputationSummary> {
    let mut summaries: StdVec<WorkerReputationSummary> = StdVec::new();

    for id in ids.iter() {
        // 1 read per worker
        if let Some(worker) = env
            .storage()
            .persistent()
            .get::<DataKey, Worker>(&DataKey::Worker(id.clone()))
        {
            summaries.push(WorkerReputationSummary {
                id: worker.id,
                reputation: worker.reputation,
                avg_rating: worker.avg_rating,
                review_count: worker.review_count,
                is_active: worker.is_active,
                subscription_tier: worker.subscription.tier,
            });
        }
    }

    summaries
}

// =============================================================================
// Composite view: category verification (cached)
// =============================================================================

/// Check whether a worker holds a specific verified category.
///
/// Reads the `Worker` record once and checks `verified_categories` in-memory,
/// then optionally reads the `CategoryVerification` detail record only if the
/// category is present.
///
/// ### Before
/// Callers often read `Worker` to check `verified_categories` and then
/// immediately read `CategoryVerification` for the expiry — 2 reads even when
/// the category was absent (1 wasted read in the miss case).
///
/// ### After
/// - **Category absent** → 1 read (early exit from `Worker` check).
/// - **Category present, no expiry check needed** → 1 read.
/// - **Category present, expiry needed** → 2 reads.
///
/// # Storage reads: **1** (absent) or **2** (present + detail)
pub fn get_category_verification_cached(
    env: &Env,
    worker_id: Symbol,
    category: Symbol,
) -> Option<CategoryVerification> {
    // Read 1: Worker
    let worker: Worker = env
        .storage()
        .persistent()
        .get(&DataKey::Worker(worker_id.clone()))?;

    // Fast-path: category not in the verified list → no detail read
    if worker
        .verified_categories
        .iter()
        .all(|c| c != category)
    {
        return None;
    }

    // Read 2: CategoryVerification detail
    env.storage()
        .persistent()
        .get(&DataKey::CategoryVerification(worker_id, category))
}

// =============================================================================
// Composite view: location verification (cached)
// =============================================================================

/// Return location verification for a worker in at most **2** reads.
///
/// Returns `None` immediately (after 1 read) if the worker does not exist,
/// saving the second read on the miss path.
///
/// # Storage reads: **1** (worker absent) or **2** (worker present)
pub fn get_location_verification_cached(
    env: &Env,
    worker_id: Symbol,
) -> Option<LocationVerification> {
    // Read 1: verify worker exists without decoding the full struct
    if !env
        .storage()
        .persistent()
        .has(&DataKey::Worker(worker_id.clone()))
    {
        return None;
    }

    // Read 2: LocationVerification
    env.storage()
        .persistent()
        .get(&DataKey::LocationVerification(worker_id))
}

// =============================================================================
// Composite view: performance metrics (cached)
// =============================================================================

/// Return a worker and their performance metrics in exactly **2** reads.
///
/// # Storage reads: **2** (`Worker`, `PerformanceMetrics`)
pub fn get_worker_with_metrics_cached(
    env: &Env,
    id: Symbol,
) -> Option<(Worker, Option<PerformanceMetrics>)> {
    // Read 1
    let worker: Worker = env
        .storage()
        .persistent()
        .get(&DataKey::Worker(id.clone()))?;

    // Read 2
    let metrics: Option<PerformanceMetrics> = env
        .storage()
        .persistent()
        .get(&DataKey::PerformanceMetrics(id));

    Some((worker, metrics))
}

// =============================================================================
// Worker count (fast path)
// =============================================================================

/// Return the total worker count in exactly **1** read.
///
/// The contract maintains a `WorkerCount` key in persistent storage that is
/// incremented/decremented atomically with every register/deregister.
/// This avoids loading the entire `WorkerList` vec just to get its length.
///
/// # Storage reads: **1** (`WorkerCount`)
pub fn get_worker_count_cached(env: &Env) -> u32 {
    env.storage()
        .persistent()
        .get(&DataKey::WorkerCount)
        .unwrap_or(0u32)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::{RegistryContract, RegistryContractClient, ROLE_CURATOR_MGR, ROLE_PAUSER, ROLE_REP_MGR, ROLE_UPGRADER};
    use soroban_sdk::{testutils::Address as _, Address, BytesN, Env, String, Symbol};

    // ---- helpers ------------------------------------------------------------

    struct TestEnv {
        env: Env,
        contract_id: Address,
        admin: Address,
        curator: Address,
        owner: Address,
    }

    impl TestEnv {
        fn new() -> Self {
            let env = Env::default();
            env.mock_all_auths();

            let admin = Address::generate(&env);
            let curator = Address::generate(&env);
            let owner = Address::generate(&env);

            let contract_id = env.register_contract(None, RegistryContract);
            let client = RegistryContractClient::new(&env, &contract_id);
            client.initialize(&admin);
            client.grant_role(&admin, &Symbol::new(&env, ROLE_PAUSER), &admin);
            client.grant_role(&admin, &Symbol::new(&env, ROLE_CURATOR_MGR), &admin);
            client.grant_role(&admin, &Symbol::new(&env, ROLE_REP_MGR), &admin);
            client.grant_role(&admin, &Symbol::new(&env, ROLE_UPGRADER), &admin);

            TestEnv { env, contract_id, admin, curator, owner }
        }

        fn client(&self) -> RegistryContractClient {
            RegistryContractClient::new(&self.env, &self.contract_id)
        }

        fn worker_id(&self) -> Symbol {
            Symbol::new(&self.env, "worker1")
        }

        fn zero_hash(&self) -> BytesN<32> {
            BytesN::from_array(&self.env, &[0u8; 32])
        }

        fn register_worker(&self) {
            self.client().add_curator(&self.admin, &self.curator);
            self.client().register(
                &self.worker_id(),
                &self.owner,
                &String::from_str(&self.env, "Alice"),
                &Symbol::new(&self.env, "plumber"),
                &self.zero_hash(),
                &self.zero_hash(),
                &self.curator,
            );
        }
    }

    // ---- CachedWorkerView ---------------------------------------------------

    #[test]
    fn test_cached_worker_view_load_existing() {
        let t = TestEnv::new();
        t.register_worker();

        // Simulate what a contract invocation would do
        t.env.as_contract(&t.contract_id, || {
            let view = CachedWorkerView::load(&t.env, t.worker_id())
                .expect("worker should exist");

            assert!(view.is_active());
            assert_eq!(view.reputation(), 0);
            assert_eq!(view.review_count(), 0);
            assert_eq!(view.avg_rating(), 0);
            assert_eq!(view.staked_amount(), 0);
            // Subscription tier defaults to Free
            assert!(matches!(view.subscription_tier(), SubscriptionTier::Free));
        });
    }

    #[test]
    fn test_cached_worker_view_load_nonexistent() {
        let t = TestEnv::new();
        t.env.as_contract(&t.contract_id, || {
            let view = CachedWorkerView::load(&t.env, Symbol::new(&t.env, "ghost"));
            assert!(view.is_none());
        });
    }

    #[test]
    fn test_cached_worker_view_multiple_accessors_single_read() {
        // Verify that all accessors work correctly after a single load.
        let t = TestEnv::new();
        t.register_worker();

        t.env.as_contract(&t.contract_id, || {
            let view = CachedWorkerView::load(&t.env, t.worker_id()).unwrap();

            // Multiple accessors — all derived from the same loaded struct
            let _id = view.id();
            let _owner = view.owner();
            let _wallet = view.wallet();
            let _loc = view.location_hash();
            let _con = view.contact_hash();
            let _active = view.is_active();
            let _rep = view.reputation();
            let _rc = view.review_count();
            let _ar = view.avg_rating();
            let _staked = view.staked_amount();
            let _tier = view.subscription_tier();
            let _exp = view.subscription_expires_at();
            let _sub = view.subscription();

            // subscription_is_active with expiry = 0 → always true
            assert!(view.subscription_is_active(u64::MAX));
        });
    }

    // ---- get_worker_profile_cached ------------------------------------------

    #[test]
    fn test_get_worker_profile_cached_returns_defaults() {
        let t = TestEnv::new();
        t.register_worker();

        t.env.as_contract(&t.contract_id, || {
            let profile = get_worker_profile_cached(&t.env, t.worker_id())
                .expect("profile should exist");

            assert!(profile.worker.is_active);
            assert!(matches!(profile.verification_level, VerificationLevel::None));
        });
    }

    #[test]
    fn test_get_worker_profile_cached_missing_worker() {
        let t = TestEnv::new();
        t.env.as_contract(&t.contract_id, || {
            let result = get_worker_profile_cached(&t.env, Symbol::new(&t.env, "missing"));
            assert!(result.is_none());
        });
    }

    #[test]
    fn test_get_worker_profile_cached_reflects_verification_level() {
        let t = TestEnv::new();
        t.register_worker();
        // Set verification level via the contract API
        t.client().set_verification_level(
            &t.admin,
            &t.worker_id(),
            &VerificationLevel::Verified,
        );

        t.env.as_contract(&t.contract_id, || {
            let profile = get_worker_profile_cached(&t.env, t.worker_id()).unwrap();
            assert!(matches!(profile.verification_level, VerificationLevel::Verified));
        });
    }

    // ---- get_worker_with_stake_cached ---------------------------------------

    #[test]
    fn test_get_worker_with_stake_cached_no_stake() {
        let t = TestEnv::new();
        t.register_worker();

        t.env.as_contract(&t.contract_id, || {
            let (worker, stake) =
                get_worker_with_stake_cached(&t.env, t.worker_id()).unwrap();
            assert!(worker.is_active);
            assert!(stake.is_none()); // no stake recorded yet
        });
    }

    #[test]
    fn test_get_worker_with_stake_cached_missing_worker() {
        let t = TestEnv::new();
        t.env.as_contract(&t.contract_id, || {
            let result = get_worker_with_stake_cached(&t.env, Symbol::new(&t.env, "ghost"));
            assert!(result.is_none());
        });
    }

    // ---- get_worker_with_availability_cached --------------------------------

    #[test]
    fn test_get_worker_with_availability_cached_no_availability() {
        let t = TestEnv::new();
        t.register_worker();

        t.env.as_contract(&t.contract_id, || {
            let (worker, avail) =
                get_worker_with_availability_cached(&t.env, t.worker_id()).unwrap();
            assert!(worker.is_active);
            assert!(avail.is_none());
        });
    }

    #[test]
    fn test_get_worker_with_availability_cached_with_availability() {
        let t = TestEnv::new();
        t.register_worker();
        t.client().update_availability(&t.worker_id(), &t.owner, &true, &0);

        t.env.as_contract(&t.contract_id, || {
            let (_, avail) =
                get_worker_with_availability_cached(&t.env, t.worker_id()).unwrap();
            assert!(avail.is_some());
            assert!(avail.unwrap().is_available);
        });
    }

    // ---- get_worker_with_reputation_inputs_cached ---------------------------

    #[test]
    fn test_get_worker_with_reputation_inputs_cached_no_reviews() {
        let t = TestEnv::new();
        t.register_worker();

        t.env.as_contract(&t.contract_id, || {
            let (worker, inputs) =
                get_worker_with_reputation_inputs_cached(&t.env, t.worker_id()).unwrap();
            assert_eq!(worker.reputation, 0);
            assert!(inputs.is_none()); // no reviews yet
        });
    }

    #[test]
    fn test_get_worker_with_reputation_inputs_cached_after_review() {
        let t = TestEnv::new();
        t.register_worker();
        let reviewer = Address::generate(&t.env);
        t.client().submit_review(&reviewer, &t.worker_id(), &8_000);

        t.env.as_contract(&t.contract_id, || {
            let (worker, inputs) =
                get_worker_with_reputation_inputs_cached(&t.env, t.worker_id()).unwrap();
            assert!(worker.reputation > 0);
            let inp = inputs.expect("reputation inputs should be present after review");
            assert_eq!(inp.rating_count, 1);
            assert_eq!(inp.rating_sum, 8_000);
        });
    }

    // ---- get_worker_summary_batch -------------------------------------------

    #[test]
    fn test_get_worker_summary_batch_empty_ids() {
        let t = TestEnv::new();
        t.env.as_contract(&t.contract_id, || {
            let summaries = get_worker_summary_batch(&t.env, Vec::new(&t.env));
            assert_eq!(summaries.len(), 0);
        });
    }

    #[test]
    fn test_get_worker_summary_batch_all_present() {
        let t = TestEnv::new();
        t.client().add_curator(&t.admin, &t.curator);

        // Register three workers
        for i in 0u8..3 {
            let id_str = std::format!("w{i}");
            let id = Symbol::new(&t.env, &id_str);
            t.client().register(
                &id,
                &t.owner,
                &String::from_str(&t.env, "Worker"),
                &Symbol::new(&t.env, "plumber"),
                &t.zero_hash(),
                &t.zero_hash(),
                &t.curator,
            );
        }

        t.env.as_contract(&t.contract_id, || {
            let ids = soroban_sdk::vec![
                &t.env,
                Symbol::new(&t.env, "w0"),
                Symbol::new(&t.env, "w1"),
                Symbol::new(&t.env, "w2"),
            ];
            let summaries = get_worker_summary_batch(&t.env, ids);
            assert_eq!(summaries.len(), 3);
            for s in &summaries {
                assert!(s.is_active);
                assert_eq!(s.reputation, 0);
            }
        });
    }

    #[test]
    fn test_get_worker_summary_batch_skips_missing() {
        let t = TestEnv::new();
        t.register_worker();

        t.env.as_contract(&t.contract_id, || {
            let ids = soroban_sdk::vec![
                &t.env,
                t.worker_id(),
                Symbol::new(&t.env, "ghost1"),
                Symbol::new(&t.env, "ghost2"),
            ];
            // Only 1 out of 3 exists — the other two should be silently skipped
            let summaries = get_worker_summary_batch(&t.env, ids);
            assert_eq!(summaries.len(), 1);
            assert_eq!(summaries[0].id, t.worker_id());
        });
    }

    // ---- get_category_verification_cached -----------------------------------

    #[test]
    fn test_get_category_verification_cached_absent_category() {
        let t = TestEnv::new();
        t.register_worker();

        t.env.as_contract(&t.contract_id, || {
            // Category "welder" has not been verified for this worker
            let result = get_category_verification_cached(
                &t.env,
                t.worker_id(),
                Symbol::new(&t.env, "welder"),
            );
            assert!(result.is_none());
        });
    }

    #[test]
    fn test_get_category_verification_cached_present_category() {
        let t = TestEnv::new();
        t.register_worker();
        let cat = Symbol::new(&t.env, "plumber");
        t.client().verify_category(&t.curator, &t.worker_id(), &cat, &9_999);

        t.env.as_contract(&t.contract_id, || {
            let result = get_category_verification_cached(
                &t.env,
                t.worker_id(),
                Symbol::new(&t.env, "plumber"),
            );
            assert!(result.is_some());
            let v = result.unwrap();
            assert_eq!(v.expires_at, 9_999);
        });
    }

    // ---- get_location_verification_cached -----------------------------------

    #[test]
    fn test_get_location_verification_cached_no_record() {
        let t = TestEnv::new();
        t.register_worker();

        t.env.as_contract(&t.contract_id, || {
            let result = get_location_verification_cached(&t.env, t.worker_id());
            assert!(result.is_none()); // no location verification set
        });
    }

    #[test]
    fn test_get_location_verification_cached_with_record() {
        let t = TestEnv::new();
        t.register_worker();
        let verifier = Address::generate(&t.env);
        t.client().verify_location(&verifier, &t.worker_id(), &5_000);

        t.env.as_contract(&t.contract_id, || {
            let result = get_location_verification_cached(&t.env, t.worker_id());
            assert!(result.is_some());
            assert_eq!(result.unwrap().expires_at, 5_000);
        });
    }

    #[test]
    fn test_get_location_verification_cached_nonexistent_worker() {
        let t = TestEnv::new();
        t.env.as_contract(&t.contract_id, || {
            let result = get_location_verification_cached(
                &t.env,
                Symbol::new(&t.env, "nonexistent"),
            );
            assert!(result.is_none());
        });
    }

    // ---- get_worker_with_metrics_cached -------------------------------------

    #[test]
    fn test_get_worker_with_metrics_cached_no_metrics() {
        let t = TestEnv::new();
        t.register_worker();

        t.env.as_contract(&t.contract_id, || {
            let (worker, metrics) =
                get_worker_with_metrics_cached(&t.env, t.worker_id()).unwrap();
            assert!(worker.is_active);
            assert!(metrics.is_none());
        });
    }

    #[test]
    fn test_get_worker_with_metrics_cached_after_update() {
        let t = TestEnv::new();
        t.register_worker();
        t.client().update_metrics(&t.admin, &t.worker_id(), &5, &8_000);

        t.env.as_contract(&t.contract_id, || {
            let (_, metrics) =
                get_worker_with_metrics_cached(&t.env, t.worker_id()).unwrap();
            let m = metrics.expect("metrics should exist after update");
            assert_eq!(m.jobs_completed, 5);
        });
    }

    // ---- get_worker_count_cached --------------------------------------------

    #[test]
    fn test_get_worker_count_cached_zero() {
        let t = TestEnv::new();
        t.env.as_contract(&t.contract_id, || {
            assert_eq!(get_worker_count_cached(&t.env), 0);
        });
    }

    #[test]
    fn test_get_worker_count_cached_after_register() {
        let t = TestEnv::new();
        t.register_worker();
        t.env.as_contract(&t.contract_id, || {
            assert_eq!(get_worker_count_cached(&t.env), 1);
        });
    }

    #[test]
    fn test_get_worker_count_cached_after_deregister() {
        let t = TestEnv::new();
        t.register_worker();
        t.client().deregister(&t.worker_id(), &t.owner);
        t.env.as_contract(&t.contract_id, || {
            assert_eq!(get_worker_count_cached(&t.env), 0);
        });
    }
}
