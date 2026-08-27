//! # Market Contract — Optimised View Helpers
//!
//! ## Problem (issue #1148)
//! Read-heavy view functions in the Market contract performed redundant storage
//! reads.  The most common patterns were:
//!
//! 1. Loading `Config` from instance storage multiple times within a single
//!    function to access `fee_bps` and `fee_recipient` separately.
//! 2. Fetching an `Escrow` record, checking fields, then fetching it *again*
//!    inside a helper for the fee split — 2 reads for the same record.
//! 3. Batch callers iterating over escrow ids and calling `get_escrow` then
//!    separately checking `arbitration` status → 2 reads per escrow.
//!
//! ## Solution
//! Provides per-call cached read helpers:
//! - `CachedConfigView` — loads `Config` once from instance storage.
//! - `CachedEscrowView` — loads an `Escrow` once from persistent storage.
//! - `get_escrow_with_arbitration_cached` — fetches both records in exactly 2 reads.
//! - `get_escrow_status_batch` — returns lightweight status summaries at N reads for N escrows.
//!
//! ### Storage-read comparison (per invocation)
//! | Helper                              | Before | After |
//! |-------------------------------------|--------|-------|
//! | Fee check + tip dispatch            | 2      | 1     |
//! | `get_escrow` + `get_arbitration`    | 2      | 2*    |
//! | `get_escrow_status_batch(N)`        | 2×N    | N     |
//!
//! *Already 2 — the optimisation is removing the duplicated `Config` read that
//!  happened inside `release_escrow` after it was already read by the caller.

extern crate alloc;

use alloc::vec::Vec as StdVec;
use soroban_sdk::{Env, Symbol, Vec};

use crate::{Arbitration, Config, DataKey, Escrow, MultiSigEscrow};

// =============================================================================
// Cached config view
// =============================================================================

/// A zero-copy wrapper around a single `Config` instance-storage read.
///
/// Before this helper, functions like `tip` and `release_escrow` each called
/// `env.storage().instance().get(&DataKey::Config)` — if both were invoked in
/// the same call chain the config was fetched twice.  `CachedConfigView`
/// ensures the struct is loaded exactly once.
///
/// # Storage reads: **1** (instance storage)
pub struct CachedConfigView {
    config: Config,
}

impl CachedConfigView {
    /// Load `Config` from instance storage.
    ///
    /// # Panics
    /// Panics with `"Not initialized"` if the contract has not been initialised.
    ///
    /// # Storage reads: **1**
    pub fn load(env: &Env) -> Self {
        let config: Config = env
            .storage()
            .instance()
            .get(&DataKey::Config)
            .expect("Not initialized");
        CachedConfigView { config }
    }

    /// Protocol fee in basis points (0–500).
    pub fn fee_bps(&self) -> u32 {
        self.config.fee_bps
    }

    /// Whether a non-zero fee is configured.
    pub fn has_fee(&self) -> bool {
        self.config.fee_bps > 0
    }

    /// Consume the view and return the underlying `Config`.
    pub fn into_config(self) -> Config {
        self.config
    }

    /// Borrow the underlying `Config`.
    pub fn config(&self) -> &Config {
        &self.config
    }
}

// =============================================================================
// Cached escrow view
// =============================================================================

/// A wrapper around a single `Escrow` persistent-storage read.
///
/// Constructed with [`CachedEscrowView::load`]; all field accesses are
/// in-memory after that single read.
///
/// # Storage reads: **1** (persistent storage)
pub struct CachedEscrowView {
    escrow: Escrow,
}

impl CachedEscrowView {
    /// Load an `Escrow` from persistent storage.
    ///
    /// Returns `Some(CachedEscrowView)` if found, `None` otherwise.
    ///
    /// # Storage reads: **1**
    pub fn load(env: &Env, id: Symbol) -> Option<Self> {
        env.storage()
            .persistent()
            .get::<DataKey, Escrow>(&DataKey::Escrow(id))
            .map(|escrow| CachedEscrowView { escrow })
    }

    /// Whether the escrow has been released.
    pub fn is_released(&self) -> bool {
        self.escrow.released
    }

    /// Whether the escrow has been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.escrow.cancelled
    }

    /// Whether the escrow is still active (not released and not cancelled).
    pub fn is_active(&self) -> bool {
        !self.escrow.released && !self.escrow.cancelled
    }

    /// Whether an arbitration request is pending.
    pub fn arbitration_requested(&self) -> bool {
        self.escrow.arbitration_requested
    }

    /// The locked amount.
    pub fn amount(&self) -> i128 {
        self.escrow.amount
    }

    /// The expiry timestamp.
    pub fn expiry(&self) -> u64 {
        self.escrow.expiry
    }

    /// Whether the escrow has expired at the given timestamp.
    pub fn is_expired(&self, now: u64) -> bool {
        now >= self.escrow.expiry
    }

    /// Consume the view and return the underlying `Escrow`.
    pub fn into_escrow(self) -> Escrow {
        self.escrow
    }

    /// Borrow the underlying `Escrow`.
    pub fn escrow(&self) -> &Escrow {
        &self.escrow
    }
}

// =============================================================================
// Composite view: escrow + arbitration
// =============================================================================

/// Return an escrow and its arbitration record in exactly **2** reads.
///
/// Before this helper, code that needed to check both records performed:
/// ```rust,ignore
/// let escrow = env.storage().persistent().get(&DataKey::Escrow(id.clone()));   // read 1
/// let arb    = env.storage().persistent().get(&DataKey::Arbitration(id));      // read 2
/// // … then checked escrow.arbitration_requested AND arb.resolved separately,
/// //   sometimes re-reading escrow later in the same invocation
/// ```
///
/// With this helper: exactly **2 reads** in all cases.
///
/// # Storage reads: **2** (`Escrow`, `Arbitration`)
///
/// # Returns
/// `Some((Escrow, Option<Arbitration>))` if the escrow exists, `None` otherwise.
pub fn get_escrow_with_arbitration_cached(
    env: &Env,
    id: Symbol,
) -> Option<(Escrow, Option<Arbitration>)> {
    // Read 1: Escrow (mandatory)
    let escrow: Escrow = env
        .storage()
        .persistent()
        .get(&DataKey::Escrow(id.clone()))?;

    // Read 2: Arbitration (optional)
    let arbitration: Option<Arbitration> = env
        .storage()
        .persistent()
        .get(&DataKey::Arbitration(id));

    Some((escrow, arbitration))
}

// =============================================================================
// Composite view: multi-sig escrow + arbitration
// =============================================================================

/// Return a multi-sig escrow and its arbitration record in exactly **2** reads.
///
/// # Storage reads: **2** (`MultiSigEscrow`, `Arbitration`)
pub fn get_multisig_with_arbitration_cached(
    env: &Env,
    id: Symbol,
) -> Option<(MultiSigEscrow, Option<Arbitration>)> {
    // Read 1: MultiSigEscrow
    let escrow: MultiSigEscrow = env
        .storage()
        .persistent()
        .get(&DataKey::MultiSigEscrow(id.clone()))?;

    // Read 2: Arbitration
    let arbitration: Option<Arbitration> = env
        .storage()
        .persistent()
        .get(&DataKey::Arbitration(id));

    Some((escrow, arbitration))
}

// =============================================================================
// Batch escrow status view
// =============================================================================

/// Lightweight status summary for a single escrow.
///
/// Derived from a single `Escrow` storage read.
#[derive(Clone)]
pub struct EscrowStatusSummary {
    /// Escrow identifier.
    pub id: Symbol,
    /// Locked amount.
    pub amount: i128,
    /// Expiry timestamp.
    pub expiry: u64,
    /// Whether released.
    pub released: bool,
    /// Whether cancelled.
    pub cancelled: bool,
    /// Whether arbitration has been requested.
    pub arbitration_requested: bool,
}

/// Return status summaries for a slice of escrow ids.
///
/// Each escrow costs exactly **1** storage read.  Missing ids are silently skipped.
///
/// ### Before this helper
/// Callers iterating over ids typically did `get_escrow(id)` then separately
/// read `get_arbitration(id)` → **2 reads per escrow**.
///
/// ### After
/// Costs exactly **N reads** (one per existing escrow). Arbitration status is
/// captured from the `arbitration_requested` flag on `Escrow`, which avoids
/// the second read for the simple "is arbitration pending?" question.
///
/// # Storage reads: **N** (one per existing escrow id)
///
/// Returns a native Rust `Vec` — this is a view-layer construct, not an
/// on-chain storage type.
pub fn get_escrow_status_batch(env: &Env, ids: Vec<Symbol>) -> StdVec<EscrowStatusSummary> {
    let mut summaries: StdVec<EscrowStatusSummary> = StdVec::new();

    for id in ids.iter() {
        if let Some(escrow) = env
            .storage()
            .persistent()
            .get::<DataKey, Escrow>(&DataKey::Escrow(id.clone()))
        {
            summaries.push(EscrowStatusSummary {
                id,
                amount: escrow.amount,
                expiry: escrow.expiry,
                released: escrow.released,
                cancelled: escrow.cancelled,
                arbitration_requested: escrow.arbitration_requested,
            });
        }
    }

    summaries
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MarketContract, MarketContractClient, ROLE_DISPUTE_MGR, ROLE_FEE_MANAGER, ROLE_PAUSER, ROLE_UPGRADER};
    use soroban_sdk::{
        testutils::Address as _,
        token::StellarAssetClient,
        Address, Env, Symbol,
    };

    struct TestEnv {
        env: Env,
        contract_id: Address,
        admin: Address,
        payer: Address,
        worker: Address,
        token_addr: Address,
    }

    impl TestEnv {
        fn new_with_fee(fee_bps: u32) -> Self {
            let env = Env::default();
            env.mock_all_auths();

            let admin = Address::generate(&env);
            let payer = Address::generate(&env);
            let worker = Address::generate(&env);

            let token_id = env.register_stellar_asset_contract_v2(admin.clone());
            let token_addr = token_id.address();
            StellarAssetClient::new(&env, &token_addr).mint(&payer, &1_000_000);

            let contract_id = env.register_contract(None, MarketContract);
            MarketContractClient::new(&env, &contract_id)
                .initialize(&admin, &fee_bps, &admin);

            let client = MarketContractClient::new(&env, &contract_id);
            client.grant_role(&admin, &Symbol::new(&env, ROLE_PAUSER), &admin);
            client.grant_role(&admin, &Symbol::new(&env, ROLE_FEE_MANAGER), &admin);
            client.grant_role(&admin, &Symbol::new(&env, ROLE_DISPUTE_MGR), &admin);
            client.grant_role(&admin, &Symbol::new(&env, ROLE_UPGRADER), &admin);

            TestEnv { env, contract_id, admin, payer, worker, token_addr }
        }

        fn new() -> Self { Self::new_with_fee(0) }

        fn client(&self) -> MarketContractClient {
            MarketContractClient::new(&self.env, &self.contract_id)
        }

        fn escrow_id(&self) -> Symbol {
            Symbol::new(&self.env, "esc1")
        }
    }

    // ---- CachedConfigView ---------------------------------------------------

    #[test]
    fn test_cached_config_view_load() {
        let t = TestEnv::new_with_fee(100);
        t.env.as_contract(&t.contract_id, || {
            let view = CachedConfigView::load(&t.env);
            assert_eq!(view.fee_bps(), 100);
            assert!(view.has_fee());
        });
    }

    #[test]
    fn test_cached_config_view_zero_fee() {
        let t = TestEnv::new(); // fee_bps = 0
        t.env.as_contract(&t.contract_id, || {
            let view = CachedConfigView::load(&t.env);
            assert_eq!(view.fee_bps(), 0);
            assert!(!view.has_fee());
        });
    }

    #[test]
    fn test_cached_config_view_into_config() {
        let t = TestEnv::new_with_fee(250);
        t.env.as_contract(&t.contract_id, || {
            let view = CachedConfigView::load(&t.env);
            let cfg = view.into_config();
            assert_eq!(cfg.fee_bps, 250);
        });
    }

    // ---- CachedEscrowView ---------------------------------------------------

    #[test]
    fn test_cached_escrow_view_load_existing() {
        let t = TestEnv::new();
        t.client().create_escrow(
            &t.escrow_id(), &t.payer, &t.worker, &t.token_addr, &300_000, &9_999,
        );

        t.env.as_contract(&t.contract_id, || {
            let view = CachedEscrowView::load(&t.env, t.escrow_id())
                .expect("escrow must exist");

            assert!(view.is_active());
            assert!(!view.is_released());
            assert!(!view.is_cancelled());
            assert!(!view.arbitration_requested());
            assert_eq!(view.amount(), 300_000);
            assert_eq!(view.expiry(), 9_999);
        });
    }

    #[test]
    fn test_cached_escrow_view_load_nonexistent() {
        let t = TestEnv::new();
        t.env.as_contract(&t.contract_id, || {
            let view = CachedEscrowView::load(&t.env, Symbol::new(&t.env, "ghost"));
            assert!(view.is_none());
        });
    }

    #[test]
    fn test_cached_escrow_view_is_expired() {
        let t = TestEnv::new();
        // Create escrow with expiry far in the future; no time manipulation needed.
        t.client().create_escrow(
            &t.escrow_id(), &t.payer, &t.worker, &t.token_addr, &100_000, &9_999_999,
        );

        t.env.as_contract(&t.contract_id, || {
            let view = CachedEscrowView::load(&t.env, t.escrow_id()).unwrap();
            // Ledger timestamp starts at 0 in tests — expiry of 9_999_999 is in the future
            assert!(!view.is_expired(0));         // before expiry
            assert!(view.is_expired(9_999_999));  // at expiry
            assert!(view.is_expired(10_000_000)); // after expiry
        });
    }

    #[test]
    fn test_cached_escrow_view_after_release() {
        let t = TestEnv::new();
        t.client().create_escrow(
            &t.escrow_id(), &t.payer, &t.worker, &t.token_addr, &100_000, &9_999,
        );
        t.client().release_escrow(&t.escrow_id(), &t.payer);

        t.env.as_contract(&t.contract_id, || {
            let view = CachedEscrowView::load(&t.env, t.escrow_id()).unwrap();
            assert!(view.is_released());
            assert!(!view.is_active());
        });
    }

    // ---- get_escrow_with_arbitration_cached ---------------------------------

    #[test]
    fn test_get_escrow_with_arbitration_cached_no_arbitration() {
        let t = TestEnv::new();
        t.client().create_escrow(
            &t.escrow_id(), &t.payer, &t.worker, &t.token_addr, &100_000, &9_999,
        );

        t.env.as_contract(&t.contract_id, || {
            let (escrow, arb) =
                get_escrow_with_arbitration_cached(&t.env, t.escrow_id()).unwrap();

            assert!(!escrow.released);
            assert!(arb.is_none()); // no arbitration requested yet
        });
    }

    #[test]
    fn test_get_escrow_with_arbitration_cached_with_arbitration() {
        let t = TestEnv::new();
        let arbitrator = Address::generate(&t.env);
        t.client().add_arbitrator(&arbitrator);
        t.client().create_escrow(
            &t.escrow_id(), &t.payer, &t.worker, &t.token_addr, &100_000, &9_999,
        );
        t.client().request_arbitration(&t.escrow_id(), &t.payer, &arbitrator, &0);

        t.env.as_contract(&t.contract_id, || {
            let (escrow, arb) =
                get_escrow_with_arbitration_cached(&t.env, t.escrow_id()).unwrap();

            assert!(escrow.arbitration_requested);
            let a = arb.expect("arbitration record must exist");
            assert_eq!(a.arbitrator, arbitrator);
            assert!(!a.resolved);
        });
    }

    #[test]
    fn test_get_escrow_with_arbitration_cached_nonexistent() {
        let t = TestEnv::new();
        t.env.as_contract(&t.contract_id, || {
            let result = get_escrow_with_arbitration_cached(
                &t.env, Symbol::new(&t.env, "ghost"),
            );
            assert!(result.is_none());
        });
    }

    // ---- get_multisig_with_arbitration_cached --------------------------------

    #[test]
    fn test_get_multisig_with_arbitration_cached_no_arb() {
        let t = TestEnv::new();
        let s1 = Address::generate(&t.env);
        let signers = soroban_sdk::vec![&t.env, s1.clone()];
        let ms_id = Symbol::new(&t.env, "ms1");

        t.client().create_multisig_escrow(
            &ms_id, &t.payer, &t.worker, &t.token_addr,
            &100_000, &9_999, &signers, &1,
        );

        t.env.as_contract(&t.contract_id, || {
            let (escrow, arb) =
                get_multisig_with_arbitration_cached(&t.env, ms_id).unwrap();
            assert!(!escrow.released);
            assert!(arb.is_none());
        });
    }

    // ---- get_escrow_status_batch --------------------------------------------

    #[test]
    fn test_get_escrow_status_batch_all_present() {
        let t = TestEnv::new();
        let ids: Vec<Symbol> = soroban_sdk::vec![
            &t.env,
            Symbol::new(&t.env, "e1"),
            Symbol::new(&t.env, "e2"),
            Symbol::new(&t.env, "e3"),
        ];

        for id in ids.iter() {
            t.client().create_escrow(
                &id, &t.payer, &t.worker, &t.token_addr, &50_000, &9_999,
            );
        }

        t.env.as_contract(&t.contract_id, || {
            let summaries = get_escrow_status_batch(&t.env, ids);
            assert_eq!(summaries.len(), 3);
            for s in &summaries {
                assert!(!s.released);
                assert!(!s.cancelled);
                assert_eq!(s.amount, 50_000);
            }
        });
    }

    #[test]
    fn test_get_escrow_status_batch_skips_missing() {
        let t = TestEnv::new();
        t.client().create_escrow(
            &t.escrow_id(), &t.payer, &t.worker, &t.token_addr, &100_000, &9_999,
        );

        t.env.as_contract(&t.contract_id, || {
            let ids = soroban_sdk::vec![
                &t.env,
                t.escrow_id(),
                Symbol::new(&t.env, "ghost1"),
                Symbol::new(&t.env, "ghost2"),
            ];
            let summaries = get_escrow_status_batch(&t.env, ids);
            // Only 1 of 3 exists
            assert_eq!(summaries.len(), 1);
            assert_eq!(summaries[0].amount, 100_000);
        });
    }

    #[test]
    fn test_get_escrow_status_batch_empty() {
        let t = TestEnv::new();
        t.env.as_contract(&t.contract_id, || {
            let summaries = get_escrow_status_batch(&t.env, Vec::new(&t.env));
            assert_eq!(summaries.len(), 0);
        });
    }

    #[test]
    fn test_get_escrow_status_batch_reflects_released_state() {
        let t = TestEnv::new();
        let id = Symbol::new(&t.env, "rel1");
        t.client().create_escrow(
            &id, &t.payer, &t.worker, &t.token_addr, &200_000, &9_999,
        );
        t.client().release_escrow(&id, &t.payer);

        t.env.as_contract(&t.contract_id, || {
            let summaries = get_escrow_status_batch(
                &t.env,
                soroban_sdk::vec![&t.env, id],
            );
            assert_eq!(summaries.len(), 1);
            assert!(summaries[0].released);
        });
    }
}
