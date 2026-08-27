//! # BlueCollar Market Contract
//!
//! Deployed on Stellar (Soroban), this contract handles token transfers between
//! users and workers in the BlueCollar protocol. It supports two payment modes:
//!
//! - **Direct tips** via [`tip`]: Immediate token transfer with an optional protocol fee.
//! - **Escrow payments** via [`create_escrow`] / [`release_escrow`] / [`cancel_escrow`]:
//!   Funds are locked until the payer approves release or the escrow expires.
//!
//! ## Access Control
//! - **Admin**: Set once at [`initialize`]. Can update the protocol fee and upgrade the contract.
//! - **Payer (`from`)**: Creates and can release or cancel (after expiry) an escrow.
//! - **Worker (`to`)**: Can also release an escrow to claim funds.
//!
//! ## Fee Model
//! A protocol fee in basis points (`fee_bps`) is deducted from each tip.
//! The fee is capped at [`MAX_FEE_BPS`] (500 bps = 5%).
//! Fees are sent to the `fee_recipient` address configured at initialisation.

#![no_std]

use soroban_sdk::{contract, contractimpl, contracttype, symbol_short, token, Address, Env, Symbol, Vec};

mod fees;
use fees::split_fee;

/// Maximum allowed protocol fee: 500 bps = 5%.
pub const MAX_FEE_BPS: u32 = 500;

/// Event schema version — bump when adding/removing/renaming events.
pub const VERSION: u32 = 1;

// =============================================================================
// Roles
// =============================================================================

/// Full admin — can grant/revoke any role.
pub const ROLE_ADMIN: &str = "admin";
/// May pause and unpause the contract.
pub const ROLE_PAUSER: &str = "pauser";
/// May update the protocol fee.
pub const ROLE_FEE_MANAGER: &str = "fee_mgr";
/// May resolve disputed milestones.
pub const ROLE_DISPUTE_MGR: &str = "dispute_mgr";
/// May upgrade the contract WASM.
pub const ROLE_UPGRADER: &str = "upgrader";

// =============================================================================
// Gas Optimization Constants for Roles
// =============================================================================

/// Role IDs for storage key optimization.
/// Maps role strings to compact u64 IDs for efficient storage.
const ROLE_ADMIN_ID: u64 = 0;
const ROLE_PAUSER_ID: u64 = 1;
const ROLE_FEE_MANAGER_ID: u64 = 2;
const ROLE_DISPUTE_MGR_ID: u64 = 3;
const ROLE_UPGRADER_ID: u64 = 4;

/// Convert a role symbol to its compact u64 ID for storage optimization.
///
/// Unknown roles map to `u64::MAX` so they share a distinct bucket without
/// colliding with the known role IDs above.
fn role_to_id(env: &Env, role: &Symbol) -> u64 {
    if *role == Symbol::new(env, ROLE_ADMIN) {
        ROLE_ADMIN_ID
    } else if *role == Symbol::new(env, ROLE_PAUSER) {
        ROLE_PAUSER_ID
    } else if *role == Symbol::new(env, ROLE_FEE_MANAGER) {
        ROLE_FEE_MANAGER_ID
    } else if *role == Symbol::new(env, ROLE_DISPUTE_MGR) {
        ROLE_DISPUTE_MGR_ID
    } else if *role == Symbol::new(env, ROLE_UPGRADER) {
        ROLE_UPGRADER_ID
    } else {
        u64::MAX
    }
}

// =============================================================================
// Types
// =============================================================================

/// Protocol configuration stored in instance storage.
#[contracttype]
#[derive(Clone)]
pub struct Config {
    /// Protocol fee in basis points (e.g. 100 = 1%). Capped at [`MAX_FEE_BPS`].
    pub fee_bps: u32,
    /// Address that receives collected protocol fees.
    pub fee_recipient: Address,
}

/// Escrow state stored in persistent storage, keyed by a caller-supplied [`Symbol`] id.
#[contracttype]
#[derive(Clone)]
pub struct Escrow {
    /// Address that funded the escrow (the payer).
    pub from: Address,
    /// Address that will receive the funds on release (the worker).
    pub to: Address,
    /// Token contract address (e.g. XLM or a custom Stellar asset).
    pub token: Address,
    /// Locked amount in the token's smallest unit.
    pub amount: i128,
    /// Unix timestamp (seconds) after which the payer may cancel.
    pub expiry: u64,
    /// `true` once funds have been released to `to`.
    pub released: bool,
    /// `true` once funds have been refunded to `from`.
    pub cancelled: bool,
    /// `true` if arbitration has been requested.
    pub arbitration_requested: bool,
}

/// Arbitration record for a disputed escrow.
#[contracttype]
#[derive(Clone)]
pub struct Arbitration {
    /// Escrow id being arbitrated.
    pub escrow_id: Symbol,
    /// Address that requested arbitration.
    pub requester: Address,
    /// Assigned arbitrator address.
    pub arbitrator: Address,
    /// Arbitration fee paid by requester.
    pub fee: i128,
    /// `true` once arbitrator has made a decision.
    pub resolved: bool,
}

/// Multi-signature escrow requiring `threshold` approvals before funds are released.
///
/// Any address in `signers` may call [`MarketContract::approve_multisig_release`].
/// Once `approvals` reaches `threshold` the funds are automatically transferred to `to`.
#[contracttype]
#[derive(Clone)]
pub struct MultiSigEscrow {
    /// Address that funded the escrow.
    pub from: Address,
    /// Address that will receive funds once threshold is met.
    pub to: Address,
    /// Token contract address.
    pub token: Address,
    /// Locked amount.
    pub amount: i128,
    /// Unix timestamp after which `from` may cancel.
    pub expiry: u64,
    /// Ordered list of addresses authorised to approve release.
    pub signers: Vec<Address>,
    /// Number of approvals required to release funds.
    pub threshold: u32,
    /// Addresses that have already approved.
    pub approvals: Vec<Address>,
    /// `true` once funds have been released.
    pub released: bool,
    /// `true` once funds have been refunded.
    pub cancelled: bool,
}

/// Storage keys used throughout the contract.
#[contracttype]
pub enum DataKey {
    /// Instance storage — [`Config`] struct, set once at [`MarketContract::initialize`].
    Config,
    /// Instance storage — paused flag; when `true` all state-mutating functions revert.
    Paused,
    /// Persistent storage — admin address.
    Admin,
    /// Persistent storage — `Vec<Address>` of members for a given role.
    RoleMembers(u64),
    /// Persistent storage — [`Escrow`] struct keyed by a caller-supplied id [`Symbol`].
    Escrow(Symbol),
    /// Persistent storage — [`MultiSigEscrow`] keyed by a caller-supplied id [`Symbol`].
    MultiSigEscrow(Symbol),
    /// Persistent storage — [`Arbitration`] keyed by escrow id [`Symbol`].
    Arbitration(Symbol),
    /// Persistent storage — list of approved arbitrator addresses.
    Arbitrators,
    /// Persistent storage — current storage schema version (u32), used by [`migrate`].
    SchemaVersion,
}

// =============================================================================
// Contract
// =============================================================================

#[contract]
pub struct MarketContract;

#[contractimpl]
impl MarketContract {
    // -------------------------------------------------------------------------
    // Initialise
    // -------------------------------------------------------------------------

    /// Initialise the contract with an admin, fee in basis points, and fee recipient.
    ///
    /// Must be called once before any other function.
    ///
    /// # Parameters
    /// - `admin`: Address that will have admin privileges.
    /// - `fee_bps`: Protocol fee in basis points (0–500). E.g. `100` = 1%.
    /// - `fee_recipient`: Address that receives collected fees.
    ///
    /// # Panics
    /// - `"Already initialized"` if called more than once.
    /// - `"fee_bps exceeds maximum (500)"` if `fee_bps > MAX_FEE_BPS`.
    pub fn initialize(env: Env, admin: Address, fee_bps: u32, fee_recipient: Address) {
        assert!(
            !env.storage().instance().has(&DataKey::Config),
            "Already initialized"
        );
        assert!(fee_bps <= MAX_FEE_BPS, "fee_bps exceeds maximum (500)");
        // Store admin in persistent storage
        env.storage().persistent().set(&DataKey::Admin, &admin);
        // Set initial schema version
        env.storage().persistent().set(&DataKey::SchemaVersion, &1u32);
        // Store config in instance storage
        let config = Config { fee_bps, fee_recipient };
        env.storage().instance().set(&DataKey::Config, &config);
        // Bootstrap: grant ROLE_ADMIN to the initial admin.
        let role = Symbol::new(&env, ROLE_ADMIN);
        let mut members: Vec<Address> = Vec::new(&env);
        members.push_back(admin.clone());
        env.storage().persistent().set(&DataKey::RoleMembers(role_to_id(&env, &role)), &members);
        env.events().publish((symbol_short!("RlGrnt"), role, admin), ());
    }

    // -------------------------------------------------------------------------
    // Admin
    // -------------------------------------------------------------------------

    /// Update the protocol fee (admin only, capped at [`MAX_FEE_BPS`]).
    ///
    /// # Parameters
    /// - `new_fee_bps`: New fee in basis points (0–500).
    ///
    /// # Panics
    /// - `"fee_bps exceeds maximum (500)"` if `new_fee_bps > MAX_FEE_BPS`.
    /// - `"Unauthorized"` if caller does not match the stored admin.
    /// - `"Not initialized"` if [`initialize`] has not been called.
    pub fn update_fee(env: Env, new_fee_bps: u32) {
        let admin = Self::get_admin(env.clone());
        Self::require_role(&env, &Symbol::new(&env, ROLE_FEE_MANAGER), &admin);
        assert!(new_fee_bps <= MAX_FEE_BPS, "fee_bps exceeds maximum (500)");
        let mut config: Config = env
            .storage()
            .instance()
            .get(&DataKey::Config)
            .expect("Not initialized");
        config.fee_bps = new_fee_bps;
        env.storage().instance().set(&DataKey::Config, &config);
    }
    /// Update the treasury (fee recipient) address. Caller must hold [`ROLE_ADMIN`].
    ///
    /// # Parameters
    /// - `caller`: Must hold `ROLE_ADMIN`; `require_auth()` is enforced.
    /// - `new_treasury`: New address that will receive collected protocol fees.
    ///
    /// # Panics
    /// - `"Missing role"` if `caller` does not hold `ROLE_ADMIN`.
    /// - `"Not initialized"` if [`initialize`] has not been called.
    pub fn set_treasury(env: Env, caller: Address, new_treasury: Address) {
        Self::require_role(&env, &Symbol::new(&env, ROLE_ADMIN), &caller);
        let mut config: Config = env
            .storage()
            .instance()
            .get(&DataKey::Config)
            .expect("Not initialized");
        config.fee_recipient = new_treasury.clone();
        env.storage().instance().set(&DataKey::Config, &config);
        env.events().publish((symbol_short!("TrsSet"), caller), new_treasury);
    }

    // -------------------------------------------------------------------------
    // Internal RBAC helpers
    // -------------------------------------------------------------------------

/// Return the member list for a role, or empty vec if no members exist.
fn get_role_members(env: &Env, role: &Symbol) -> Vec<Address> {
    env.storage()
        .persistent()
        .get(&DataKey::RoleMembers(role_to_id(env, role)))
        .unwrap_or(Vec::new(env))
}

    /// Assert that `caller` holds `role` and has authorised this call.
    ///
    /// # Panics
    /// Panics with `"Missing role"` if `caller` does not hold the role.
    fn require_role(env: &Env, role: &Symbol, caller: &Address) {
        caller.require_auth();
        let members = Self::get_role_members(env, role);
        assert!(members.iter().any(|m| m == *caller), "Missing role");
    }

    /// Create a role symbol (gas-optimization helper, mirrors the registry contract).
    fn role_symbol(env: &Env, role_str: &str) -> Symbol {
        Symbol::new(env, role_str)
    }

    // -------------------------------------------------------------------------
    // Role management (ROLE_ADMIN only)
    // -------------------------------------------------------------------------

    /// Grant a role to an address. Caller must hold [`ROLE_ADMIN`].
    ///
    /// Idempotent — granting an already-held role is a no-op.
    ///
    /// # Parameters
    /// - `caller`: Must hold `ROLE_ADMIN`; `require_auth()` is enforced.
    /// - `role`: The role symbol to grant.
    /// - `account`: Address to receive the role.
    ///
    /// # Panics
    /// - `"Missing role"` if `caller` does not hold `ROLE_ADMIN`.
    /// - `"Contract is paused"` if paused.
    ///
    /// # Events
    /// Emits `("RlGrnt", role, account)`.
    pub fn grant_role(env: Env, caller: Address, role: Symbol, account: Address) {
        let admin_role = Symbol::new(&env, ROLE_ADMIN);
        Self::require_role(&env, &admin_role, &caller);
        Self::require_not_paused(&env);

        let mut members = Self::get_role_members(&env, &role);
        if members.iter().all(|m| m != account) {
            members.push_back(account.clone());
            env.storage().persistent().set(&DataKey::RoleMembers(role_to_id(&env, &role)), &members);
        }

        env.events().publish((symbol_short!("RlGrnt"), role, account), ());
    }

    /// Revoke a role from an address. Caller must hold [`ROLE_ADMIN`].
    ///
    /// # Parameters
    /// - `caller`: Must hold `ROLE_ADMIN`; `require_auth()` is enforced.
    /// - `role`: The role symbol to revoke.
    /// - `account`: Address to lose the role.
    ///
    /// # Panics
    /// - `"Missing role"` if `caller` does not hold `ROLE_ADMIN`.
    /// - `"Account does not hold role"` if `account` is not a member.
    /// - `"Contract is paused"` if paused.
    ///
    /// # Events
    /// Emits `("RlRvkd", role, account)`.
    pub fn revoke_role(env: Env, caller: Address, role: Symbol, account: Address) {
        let admin_role = Symbol::new(&env, ROLE_ADMIN);
        Self::require_role(&env, &admin_role, &caller);
        Self::require_not_paused(&env);

        let members = Self::get_role_members(&env, &role);
        let mut updated: Vec<Address> = Vec::new(&env);
        let mut found = false;
        for m in members.iter() {
            if m == account {
                found = true;
            } else {
                updated.push_back(m);
            }
        }
        assert!(found, "Account does not hold role");
        env.storage().persistent().set(&DataKey::RoleMembers(role_to_id(&env, &role)), &updated);

        env.events().publish((symbol_short!("RlRvkd"), role, account), ());
    }

    /// Returns `true` if `account` holds `role`.
    pub fn has_role(env: Env, role: Symbol, account: Address) -> bool {
        Self::get_role_members(&env, &role).iter().any(|m| m == account)
    }

    /// Return all members of a role.
    pub fn get_role_members_list(env: Env, role: Symbol) -> Vec<Address> {
        Self::get_role_members(&env, &role)
    }

    // -------------------------------------------------------------------------
    // Pause / Unpause (admin only)
    // -------------------------------------------------------------------------

    /// Assert that the contract is not paused.
    ///
    /// # Panics
    /// Panics with `"Contract is paused"` if the paused flag is set.
    fn require_not_paused(env: &Env) {
        let paused: bool = env
            .storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false);
        assert!(!paused, "Contract is paused");
    }

    /// Pause the contract, blocking all state-mutating operations.
    ///
    /// # Parameters
    /// - `admin`: Must hold [`ROLE_PAUSER`]; `require_auth()` is enforced.
    ///
    /// # Panics
    /// Panics with `"Missing role"` if `admin` does not hold `ROLE_PAUSER`.
    ///
    /// # Events
    /// Emits `("Paused", admin)`.
    pub fn pause(env: Env, admin: Address) {
        let pauser_role = Self::role_symbol(&env, ROLE_PAUSER);
        Self::require_role(&env, &pauser_role, &admin);
        env.storage().instance().set(&DataKey::Paused, &true);
        env.events().publish((symbol_short!("Paused"), admin), ());
    }

    /// Unpause the contract, re-enabling all state-mutating operations.
    ///
    /// # Parameters
    /// - `admin`: Must hold [`ROLE_PAUSER`]; `require_auth()` is enforced.
    ///
    /// # Panics
    /// Panics with `"Missing role"` if `admin` does not hold `ROLE_PAUSER`.
    ///
    /// # Events
    /// Emits `("Unpaused", admin)`.
    pub fn unpause(env: Env, admin: Address) {
        let pauser_role = Self::role_symbol(&env, ROLE_PAUSER);
        Self::require_role(&env, &pauser_role, &admin);
        env.storage().instance().set(&DataKey::Paused, &false);
        env.events().publish((symbol_short!("Unpaused"), admin), ());
    }

    /// Returns `true` if the contract is currently paused.
    pub fn is_paused(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false)
    }

    /// Get the admin address.
    ///
    /// # Panics
    /// Panics with `"Not initialized"` if [`initialize`] has not been called.
    pub fn get_admin(env: Env) -> Address {
        env.storage()
            .persistent()
            .get(&DataKey::Admin)
            .expect("Not initialized")
    }

    /// Set a new admin address. Caller must be the current admin.
    ///
    /// # Parameters
    /// - `new_admin`: The address that will become the new admin.
    ///
    /// # Panics
    /// - `"Not initialized"` if [`initialize`] has not been called.
    /// - `"Unauthorized"` if caller does not match the stored admin.
    pub fn set_admin(env: Env, new_admin: Address) {
        let current_admin = Self::get_admin(env.clone());
        current_admin.require_auth(); // Require auth from current admin
        
        // Update admin in persistent storage
        env.storage().persistent().set(&DataKey::Admin, &new_admin);
        
        // Update role membership: remove old admin from ADMIN role, add new admin
        let admin_role = Self::role_symbol(&env, ROLE_ADMIN);
        let members = Self::get_role_members(&env, &admin_role);
        // soroban_sdk::Vec has no `retain`; rebuild without the old admin.
        let mut updated: Vec<Address> = Vec::new(&env);
        for m in members.iter() {
            if m != current_admin {
                updated.push_back(m);
            }
        }
        if !updated.iter().any(|m| m == new_admin) {
            updated.push_back(new_admin.clone()); // Add new admin if not already present
        }
        env.storage().persistent().set(&DataKey::RoleMembers(role_to_id(&env, &admin_role)), &updated);
    }

    // -------------------------------------------------------------------------
    // Tip
    // -------------------------------------------------------------------------

    /// Send a direct tip to a worker.
    ///
    /// Deducts the protocol fee (`fee_bps`) from `amount` and transfers the remainder
    /// to `to`. If `fee_bps` is 0, the full amount goes to `to`.
    ///
    /// # Parameters
    /// - `from`: Payer address; `require_auth()` is enforced.
    /// - `to`: Worker address that receives the tip.
    /// - `token_addr`: The Stellar token contract address.
    /// - `amount`: Total amount to send (in the token's smallest unit).
    ///
    /// # Panics
    /// - `"Amount must be positive"` if `amount <= 0`.
    /// - `"Not initialized"` if [`initialize`] has not been called.
    ///
    /// # Events
    /// Emits `("TipSent", from, to)` with data `(token_addr, amount)`.
    pub fn tip(env: Env, from: Address, to: Address, token_addr: Address, amount: i128) {
        from.require_auth();
        Self::require_not_paused(&env);
        assert!(amount > 0, "Amount must be positive");

        let config: Config = env
            .storage()
            .instance()
            .get(&DataKey::Config)
            .expect("Not initialized");

        let client = token::Client::new(&env, &token_addr);

        let (fee, worker_amount) = split_fee(amount, config.fee_bps);
        client.transfer(&from, &to, &worker_amount);
        if fee > 0 {
            client.transfer(&from, &config.fee_recipient, &fee);
            env.events().publish(
                (symbol_short!("FeeTaken"),),
                (fee, config.fee_recipient),
            );
        }

        env.events().publish(
            (symbol_short!("TipSent"), from, to),
            (token_addr, amount),
        );    }

    // -------------------------------------------------------------------------
    // Escrow
    // -------------------------------------------------------------------------

    /// Create an escrow — locks tokens in the contract until released, cancelled, or expired.
    ///
    /// Transfers `amount` tokens from `from` to the contract address immediately.
    ///
    /// # Parameters
    /// - `id`: Caller-supplied unique identifier for this escrow.
    /// - `from`: Payer address; `require_auth()` is enforced.
    /// - `to`: Worker address that will receive funds on release.
    /// - `token_addr`: The Stellar token contract address.
    /// - `amount`: Amount to lock (must be > 0).
    /// - `expiry`: Unix timestamp after which `from` may cancel and reclaim funds.
    ///
    /// # Panics
    /// - `"Amount must be positive"` if `amount <= 0`.
    /// - `"Escrow id already exists"` if an escrow with the same `id` already exists.
    ///
    /// # Events
    /// Emits `("EscCrt", id, from)` with data `(to, token_addr, amount, expiry)`.
    pub fn create_escrow(
        env: Env,
        id: Symbol,
        from: Address,
        to: Address,
        token_addr: Address,
        amount: i128,
        expiry: u64,
    ) {
        from.require_auth();
        Self::require_not_paused(&env);
        assert!(amount > 0, "Amount must be positive");
        assert!(
            !env.storage().persistent().has(&DataKey::Escrow(id.clone())),
            "Escrow id already exists"
        );

        let contract_addr = env.current_contract_address();
        let client = token::Client::new(&env, &token_addr);
        client.transfer(&from, &contract_addr, &amount);

        let escrow = Escrow {
            from: from.clone(),
            to: to.clone(),
            token: token_addr.clone(),
            amount,
            expiry,
            released: false,
            cancelled: false,
            arbitration_requested: false,
        };
        env.storage().persistent().set(&DataKey::Escrow(id.clone()), &escrow);

        env.events().publish(
            (symbol_short!("EscCrt"), id, from),
            (to, token_addr, amount, expiry),
        );
    }

    /// Release escrowed funds to the worker.
    ///
    /// Callable by either `from` (payer approves) or `to` (worker claims).
    ///
    /// # Parameters
    /// - `id`: The escrow identifier.
    /// - `caller`: Must be either `from` or `to`; `require_auth()` is enforced.
    ///
    /// # Panics
    /// - `"Escrow not found"` if no escrow exists with the given `id`.
    /// - `"Not authorized"` if `caller` is neither `from` nor `to`.
    /// - `"Already released"` if the escrow has already been released.
    /// - `"Escrow cancelled"` if the escrow was previously cancelled.
    ///
    /// # Events
    /// Emits `("EscRel", id, escrow.to)` with data `escrow.amount`.
    pub fn release_escrow(env: Env, id: Symbol, caller: Address) {
        caller.require_auth();
        Self::require_not_paused(&env);
        let mut escrow: Escrow = env
            .storage()
            .persistent()
            .get(&DataKey::Escrow(id.clone()))
            .expect("Escrow not found");

        assert!(
            escrow.from == caller || escrow.to == caller,
            "Not authorized"
        );
        assert!(!escrow.released, "Already released");
        assert!(!escrow.cancelled, "Escrow cancelled");

        let contract_addr = env.current_contract_address();
        let client = token::Client::new(&env, &escrow.token);
        let config: Config = env
            .storage()
            .instance()
            .get(&DataKey::Config)
            .expect("Not initialized");
        let (fee, net) = split_fee(escrow.amount, config.fee_bps);
        client.transfer(&contract_addr, &escrow.to, &net);
        if fee > 0 {
            client.transfer(&contract_addr, &config.fee_recipient, &fee);
            env.events().publish(
                (symbol_short!("FeeTaken"),),
                (fee, config.fee_recipient.clone()),
            );
        }

        escrow.released = true;
        env.storage().persistent().set(&DataKey::Escrow(id.clone()), &escrow);

        env.events().publish(
            (symbol_short!("EscRel"), id, escrow.to),
            escrow.amount,
        );
    }

    /// Cancel escrow and refund the payer.
    ///
    /// Only callable by `from` (the payer), and only after `expiry` has passed.
    ///
    /// # Parameters
    /// - `id`: The escrow identifier.
    /// - `caller`: Must be `from`; `require_auth()` is enforced.
    ///
    /// # Panics
    /// - `"Escrow not found"` if no escrow exists with the given `id`.
    /// - `"Not authorized"` if `caller` is not `from`.
    /// - `"Already released"` if the escrow has already been released.
    /// - `"Already cancelled"` if the escrow was already cancelled.
    /// - `"Escrow not yet expired"` if the current ledger timestamp is before `expiry`.
    ///
    /// # Events
    /// Emits `("EscCnl", id, escrow.from)` with data `escrow.amount`.
    pub fn cancel_escrow(env: Env, id: Symbol, caller: Address) {
        caller.require_auth();
        Self::require_not_paused(&env);
        let mut escrow: Escrow = env
            .storage()
            .persistent()
            .get(&DataKey::Escrow(id.clone()))
            .expect("Escrow not found");

        assert!(escrow.from == caller, "Not authorized");
        assert!(!escrow.released, "Already released");
        assert!(!escrow.cancelled, "Already cancelled");

        let now = env.ledger().timestamp();
        assert!(now >= escrow.expiry, "Escrow not yet expired");

        let contract_addr = env.current_contract_address();
        let client = token::Client::new(&env, &escrow.token);
        client.transfer(&contract_addr, &escrow.from, &escrow.amount);

        escrow.cancelled = true;
        env.storage().persistent().set(&DataKey::Escrow(id.clone()), &escrow);

        env.events().publish(
            (symbol_short!("EscCnl"), id, escrow.from),
            escrow.amount,
        );
    }

    /// Fetch escrow details by id.
    ///
    /// # Parameters
    /// - `id`: The escrow identifier.
    ///
    /// # Returns
    /// `Some(Escrow)` if found, `None` otherwise.
    pub fn get_escrow(env: Env, id: Symbol) -> Option<Escrow> {
        env.storage().persistent().get(&DataKey::Escrow(id))
    }

    /// Release multiple escrows in one transaction. Callable by payer or worker.
    ///
    /// Skips escrows that are already released, cancelled, or where `caller` is
    /// not authorised — returns successfully released ids only.
    ///
    /// # Parameters
    /// - `caller`: Must be `from` or `to` on each escrow; `require_auth()` enforced once.
    /// - `ids`: Escrow identifiers to release (max 20).
    ///
    /// # Returns
    /// A `Vec<Symbol>` of ids that were successfully released.
    ///
    /// # Panics
    /// - `"Batch too large"` if `ids.len() > 20`.
    ///
    /// # Events
    /// Emits `("EscRel", id, to)` with data `amount` for each released escrow.
    pub fn batch_release_escrow(env: Env, caller: Address, ids: Vec<Symbol>) -> Vec<Symbol> {
        caller.require_auth();
        Self::require_not_paused(&env);
        assert!(ids.len() <= 20, "Batch too large");

        let contract_addr = env.current_contract_address();
        let mut released: Vec<Symbol> = Vec::new(&env);

        for id in ids.iter() {
            let key = DataKey::Escrow(id.clone());
            if let Some(mut escrow) = env.storage().persistent().get::<DataKey, Escrow>(&key) {
                if escrow.released || escrow.cancelled {
                    continue;
                }
                if escrow.from != caller && escrow.to != caller {
                    continue;
                }
                let client = token::Client::new(&env, &escrow.token);
                client.transfer(&contract_addr, &escrow.to, &escrow.amount);
                escrow.released = true;
                env.storage().persistent().set(&key, &escrow);
                env.events().publish(
                    (symbol_short!("EscRel"), id.clone(), escrow.to),
                    escrow.amount,
                );
                released.push_back(id);
            }
        }
        released
    }

    /// Return the current contract configuration.
    pub fn get_config(env: Env) -> Config {
        env.storage()
            .instance()
            .get(&DataKey::Config)
            .expect("Not initialized")
    }

    /// Return the event schema version.
    pub fn version(_env: Env) -> u32 {
        VERSION
    }

    /// Cancel an escrow that has passed its expiry. Callable by anyone.
    ///
    /// Refunds the full amount to `from` with no fee.
    ///
    /// # Parameters
    /// - `id`: The escrow identifier.
    ///
    /// # Panics
    /// - `"Escrow not found"` if no escrow exists with the given `id`.
    /// - `"Escrow not active"` if already released or cancelled.
    /// - `"Escrow not yet expired"` if current timestamp < expiry.
    ///
    /// # Events
    /// Emits `("EscExp", id, escrow.from)` with data `escrow.amount`.
    pub fn cancel_expired_escrow(env: Env, id: Symbol) {
        Self::require_not_paused(&env);
        let mut escrow: Escrow = env
            .storage()
            .persistent()
            .get(&DataKey::Escrow(id.clone()))
            .expect("Escrow not found");

        assert!(!escrow.released && !escrow.cancelled, "Escrow not active");
        assert!(env.ledger().timestamp() >= escrow.expiry, "Escrow not yet expired");

        let client = token::Client::new(&env, &escrow.token);
        client.transfer(&env.current_contract_address(), &escrow.from, &escrow.amount);

        escrow.cancelled = true;
        env.storage().persistent().set(&DataKey::Escrow(id.clone()), &escrow);

        env.events().publish(
            (symbol_short!("EscExp"), id, escrow.from),
            escrow.amount,
        );
    }

    // -------------------------------------------------------------------------
    // Multi-sig escrow (#337)
    // -------------------------------------------------------------------------

    /// Create a multi-signature escrow requiring `threshold` approvals before release.
    ///
    /// Transfers `amount` tokens from `from` to the contract immediately.
    ///
    /// # Parameters
    /// - `id`: Unique identifier for this multi-sig escrow.
    /// - `from`: Payer; `require_auth()` is enforced.
    /// - `to`: Worker that receives funds once threshold is met.
    /// - `token_addr`: Stellar token contract address.
    /// - `amount`: Amount to lock (must be > 0).
    /// - `expiry`: Unix timestamp after which `from` may cancel.
    /// - `signers`: Addresses authorised to approve release.
    /// - `threshold`: Number of approvals required (1 ≤ threshold ≤ signers.len()).
    ///
    /// # Panics
    /// - `"Amount must be positive"` if `amount <= 0`.
    /// - `"MultiSigEscrow id already exists"` on duplicate id.
    /// - `"Invalid threshold"` if threshold is 0 or exceeds signers count.
    ///
    /// # Events
    /// Emits `("MsEscCrt", id, from)` with data `(to, amount, threshold)`.
    pub fn create_multisig_escrow(
        env: Env,
        id: Symbol,
        from: Address,
        to: Address,
        token_addr: Address,
        amount: i128,
        expiry: u64,
        signers: Vec<Address>,
        threshold: u32,
    ) {
        from.require_auth();
        Self::require_not_paused(&env);
        assert!(amount > 0, "Amount must be positive");
        assert!(
            !env.storage().persistent().has(&DataKey::MultiSigEscrow(id.clone())),
            "MultiSigEscrow id already exists"
        );
        assert!(
            threshold > 0 && threshold <= signers.len(),
            "Invalid threshold"
        );

        let client = token::Client::new(&env, &token_addr);
        client.transfer(&from, &env.current_contract_address(), &amount);

        let escrow = MultiSigEscrow {
            from: from.clone(),
            to: to.clone(),
            token: token_addr,
            amount,
            expiry,
            signers,
            threshold,
            approvals: Vec::new(&env),
            released: false,
            cancelled: false,
        };
        env.storage().persistent().set(&DataKey::MultiSigEscrow(id.clone()), &escrow);

        env.events().publish(
            (symbol_short!("MsEscCrt"), id, from),
            (to, amount, threshold),
        );
    }

    /// Approve release of a multi-sig escrow. Funds are released automatically when
    /// the approval count reaches `threshold`.
    ///
    /// # Parameters
    /// - `id`: The multi-sig escrow identifier.
    /// - `caller`: Must be in `signers`; `require_auth()` is enforced.
    ///
    /// # Panics
    /// - `"MultiSigEscrow not found"` if no escrow exists with the given `id`.
    /// - `"Not a signer"` if `caller` is not in the signers list.
    /// - `"Already approved"` if `caller` has already approved.
    /// - `"Already released"` / `"Escrow cancelled"` if escrow is finalised.
    ///
    /// # Events
    /// Emits `("MsEscApv", id, caller)` with data `approvals_count`.
    /// Emits `("MsEscRel", id, to)` with data `amount` when threshold is reached.
    pub fn approve_multisig_release(env: Env, id: Symbol, caller: Address) {
        caller.require_auth();
        Self::require_not_paused(&env);
        let mut escrow: MultiSigEscrow = env
            .storage()
            .persistent()
            .get(&DataKey::MultiSigEscrow(id.clone()))
            .expect("MultiSigEscrow not found");

        assert!(!escrow.released, "Already released");
        assert!(!escrow.cancelled, "Escrow cancelled");
        assert!(
            escrow.signers.iter().any(|s| s == caller),
            "Not a signer"
        );
        assert!(
            escrow.approvals.iter().all(|a| a != caller),
            "Already approved"
        );

        escrow.approvals.push_back(caller.clone());
        let count = escrow.approvals.len();

        env.events().publish(
            (symbol_short!("MsEscApv"), id.clone(), caller),
            count,
        );

        if count >= escrow.threshold {
            let client = token::Client::new(&env, &escrow.token);
            client.transfer(&env.current_contract_address(), &escrow.to, &escrow.amount);
            escrow.released = true;
            env.events().publish(
                (symbol_short!("MsEscRel"), id.clone(), escrow.to.clone()),
                escrow.amount,
            );
        }

        env.storage().persistent().set(&DataKey::MultiSigEscrow(id), &escrow);
    }

    /// Cancel a multi-sig escrow and refund the payer (after expiry, payer only).
    ///
    /// # Panics
    /// - `"MultiSigEscrow not found"`, `"Not authorized"`, `"Already released"`,
    ///   `"Already cancelled"`, `"Escrow not yet expired"`.
    ///
    /// # Events
    /// Emits `("MsEscCnl", id, from)` with data `amount`.
    pub fn cancel_multisig_escrow(env: Env, id: Symbol, caller: Address) {
        caller.require_auth();
        Self::require_not_paused(&env);
        let mut escrow: MultiSigEscrow = env
            .storage()
            .persistent()
            .get(&DataKey::MultiSigEscrow(id.clone()))
            .expect("MultiSigEscrow not found");

        assert!(escrow.from == caller, "Not authorized");
        assert!(!escrow.released, "Already released");
        assert!(!escrow.cancelled, "Already cancelled");
        assert!(env.ledger().timestamp() >= escrow.expiry, "Escrow not yet expired");

        let client = token::Client::new(&env, &escrow.token);
        client.transfer(&env.current_contract_address(), &escrow.from, &escrow.amount);
        escrow.cancelled = true;
        env.storage().persistent().set(&DataKey::MultiSigEscrow(id.clone()), &escrow);

        env.events().publish(
            (symbol_short!("MsEscCnl"), id, escrow.from),
            escrow.amount,
        );
    }

    /// Fetch multi-sig escrow details by id.
    pub fn get_multisig_escrow(env: Env, id: Symbol) -> Option<MultiSigEscrow> {
        env.storage().persistent().get(&DataKey::MultiSigEscrow(id))
    }

    /// Request arbitration for a disputed multi-sig escrow.
    ///
    /// Either `from` or `to` may request arbitration on an active, unreleased,
    /// non-cancelled escrow. Calling this marks the escrow as disputed and records
    /// the arbitration, preventing normal approval-based release until resolved.
    ///
    /// # Parameters
    /// - `escrow_id`: The multi-sig escrow identifier.
    /// - `caller`: Must be `from` or `to`; `require_auth()` is enforced.
    /// - `arbitrator`: Must be a registered arbitrator address.
    /// - `fee`: Arbitration fee paid immediately from `caller` to `arbitrator`.
    ///
    /// # Panics
    /// - `"MultiSigEscrow not found"` if no escrow with the given `id` exists.
    /// - `"Not authorized"` if caller is neither `from` nor `to`.
    /// - `"Escrow finalized"` if already released or cancelled.
    /// - `"Arbitration already requested"` if arbitration is already active.
    /// - `"Invalid arbitrator"` if the arbitrator is not in the approved list.
    /// - `"Contract is paused"` if paused.
    ///
    /// # Events
    /// Emits `("MsArbReq", escrow_id, caller)` with data `(arbitrator, fee)`.
    pub fn request_multisig_arbitration(
        env: Env,
        escrow_id: Symbol,
        caller: Address,
        arbitrator: Address,
        fee: i128,
    ) {
        caller.require_auth();
        Self::require_not_paused(&env);

        let mut escrow: MultiSigEscrow = env
            .storage()
            .persistent()
            .get(&DataKey::MultiSigEscrow(escrow_id.clone()))
            .expect("MultiSigEscrow not found");

        assert!(
            escrow.from == caller || escrow.to == caller,
            "Not authorized"
        );
        assert!(!escrow.released && !escrow.cancelled, "Escrow finalized");

        // Re-use the Arbitration storage key — one record per escrow id.
        assert!(
            !env.storage().persistent().has(&DataKey::Arbitration(escrow_id.clone())),
            "Arbitration already requested"
        );

        let arbitrators: Vec<Address> = env
            .storage()
            .persistent()
            .get(&DataKey::Arbitrators)
            .unwrap_or(Vec::new(&env));
        assert!(
            arbitrators.iter().any(|a| a == arbitrator),
            "Invalid arbitrator"
        );

        // Pay arbitration fee immediately.
        if fee > 0 {
            let client = token::Client::new(&env, &escrow.token);
            client.transfer(&caller, &arbitrator, &fee);
        }

        // Mark escrow as disputed (borrow the cancelled flag semantics via a
        // dedicated Arbitration record; the escrow itself stays "active" so
        // the arbitrator can release it either way).
        escrow.cancelled = false; // ensure still active
        env.storage()
            .persistent()
            .set(&DataKey::MultiSigEscrow(escrow_id.clone()), &escrow);

        let arbitration = Arbitration {
            escrow_id: escrow_id.clone(),
            requester: caller.clone(),
            arbitrator: arbitrator.clone(),
            fee,
            resolved: false,
        };
        env.storage()
            .persistent()
            .set(&DataKey::Arbitration(escrow_id.clone()), &arbitration);

        env.events().publish(
            (symbol_short!("MsArbReq"), escrow_id, caller),
            (arbitrator, fee),
        );
    }

    /// Resolve arbitration for a disputed multi-sig escrow.
    ///
    /// The assigned arbitrator decides whether funds go to the worker (`to`)
    /// or are refunded to the payer (`from`).
    ///
    /// # Parameters
    /// - `escrow_id`: The multi-sig escrow identifier.
    /// - `arbitrator`: Must match the arbitrator on the record; `require_auth()` enforced.
    /// - `release_to_worker`: `true` → release to `to`; `false` → refund to `from`.
    ///
    /// # Panics
    /// - `"Arbitration not found"` if no arbitration record exists for this escrow.
    /// - `"Not the arbitrator"` if `arbitrator` does not match the record.
    /// - `"Already resolved"` if already resolved.
    /// - `"MultiSigEscrow not found"` if the underlying escrow is missing.
    /// - `"Contract is paused"` if paused.
    ///
    /// # Events
    /// Emits `("MsArbRes", escrow_id, arbitrator)` with data `release_to_worker`.
    pub fn resolve_multisig_arbitration(
        env: Env,
        escrow_id: Symbol,
        arbitrator: Address,
        release_to_worker: bool,
    ) {
        arbitrator.require_auth();
        Self::require_not_paused(&env);

        let mut arbitration: Arbitration = env
            .storage()
            .persistent()
            .get(&DataKey::Arbitration(escrow_id.clone()))
            .expect("Arbitration not found");

        assert!(arbitration.arbitrator == arbitrator, "Not the arbitrator");
        assert!(!arbitration.resolved, "Already resolved");

        let mut escrow: MultiSigEscrow = env
            .storage()
            .persistent()
            .get(&DataKey::MultiSigEscrow(escrow_id.clone()))
            .expect("MultiSigEscrow not found");

        assert!(!escrow.released && !escrow.cancelled, "Escrow finalized");

        let client = token::Client::new(&env, &escrow.token);
        let recipient = if release_to_worker {
            escrow.released = true;
            escrow.to.clone()
        } else {
            escrow.cancelled = true;
            escrow.from.clone()
        };
        client.transfer(&env.current_contract_address(), &recipient, &escrow.amount);

        arbitration.resolved = true;

        env.storage()
            .persistent()
            .set(&DataKey::MultiSigEscrow(escrow_id.clone()), &escrow);
        env.storage()
            .persistent()
            .set(&DataKey::Arbitration(escrow_id.clone()), &arbitration);

        env.events().publish(
            (symbol_short!("MsArbRes"), escrow_id, arbitrator),
            release_to_worker,
        );
    }

    /// Get arbitration details for a multi-sig escrow (re-uses Arbitration storage key).
    pub fn get_multisig_arbitration(env: Env, escrow_id: Symbol) -> Option<Arbitration> {
        env.storage()
            .persistent()
            .get(&DataKey::Arbitration(escrow_id))
    }

    // -------------------------------------------------------------------------
    // Arbitration (#377)
    // -------------------------------------------------------------------------

    /// Add an arbitrator address (admin only).
    ///
    /// # Parameters
    /// - `arbitrator`: Address to add as arbitrator.
    ///
    /// # Panics
    /// - `"Not initialized"` if [`initialize`] has not been called.
    /// - `"Unauthorized"` if caller does not match the stored admin.
    pub fn add_arbitrator(env: Env, arbitrator: Address) {
        let admin = Self::get_admin(env.clone());
        admin.require_auth();
        let mut arbitrators: Vec<Address> = env
            .storage()
            .persistent()
            .get(&DataKey::Arbitrators)
            .unwrap_or(Vec::new(&env));
        if arbitrators.iter().all(|a| a != arbitrator) {
            arbitrators.push_back(arbitrator.clone());
            env.storage().persistent().set(&DataKey::Arbitrators, &arbitrators);
        }
        env.events().publish((symbol_short!("ArbAdd"), admin, arbitrator), ());
    }

    /// Remove an arbitrator address (admin only).
    ///
    /// # Parameters
    /// - `arbitrator`: Address to remove as arbitrator.
    ///
    /// # Panics
    /// - `"Not initialized"` if [`initialize`] has not been called.
    /// - `"Unauthorized"` if caller does not match the stored admin.
    pub fn remove_arbitrator(env: Env, arbitrator: Address) {
        let admin = Self::get_admin(env.clone());
        admin.require_auth();
        let arbitrators: Vec<Address> = env
            .storage()
            .persistent()
            .get(&DataKey::Arbitrators)
            .unwrap_or(Vec::new(&env));
        let mut updated: Vec<Address> = Vec::new(&env);
        for a in arbitrators.iter() {
            if a != arbitrator {
                updated.push_back(a.clone());
            }
        }
        env.storage().persistent().set(&DataKey::Arbitrators, &updated);
        env.events().publish((symbol_short!("ArbRem"), admin, arbitrator), ());
    }

    /// Request arbitration for a disputed escrow.
    pub fn request_arbitration(
        env: Env,
        escrow_id: Symbol,
        caller: Address,
        arbitrator: Address,
        fee: i128,
    ) {
        caller.require_auth();
        let mut escrow: Escrow = env
            .storage()
            .persistent()
            .get(&DataKey::Escrow(escrow_id.clone()))
            .expect("Escrow not found");
        assert!(!escrow.released && !escrow.cancelled, "Escrow finalized");
        assert!(!escrow.arbitration_requested, "Arbitration already requested");
        assert!(escrow.from == caller || escrow.to == caller, "Not authorized");

        let arbitrators: Vec<Address> = env
            .storage()
            .persistent()
            .get(&DataKey::Arbitrators)
            .unwrap_or(Vec::new(&env));
        assert!(arbitrators.iter().any(|a| a == arbitrator), "Invalid arbitrator");

        let client = token::Client::new(&env, &escrow.token);
        client.transfer(&caller, &arbitrator, &fee);

        escrow.arbitration_requested = true;
        env.storage().persistent().set(&DataKey::Escrow(escrow_id.clone()), &escrow);

        let arbitration = Arbitration {
            escrow_id: escrow_id.clone(),
            requester: caller.clone(),
            arbitrator: arbitrator.clone(),
            fee,
            resolved: false,
        };
        env.storage().persistent().set(&DataKey::Arbitration(escrow_id.clone()), &arbitration);

        env.events().publish(
            (symbol_short!("ArbReq"), escrow_id, caller),
            (arbitrator, fee),
        );
    }

    /// Resolve arbitration by releasing funds to winner.
    pub fn resolve_arbitration(
        env: Env,
        escrow_id: Symbol,
        arbitrator: Address,
        release_to_worker: bool,
    ) {
        arbitrator.require_auth();
        let mut arbitration: Arbitration = env
            .storage()
            .persistent()
            .get(&DataKey::Arbitration(escrow_id.clone()))
            .expect("Arbitration not found");
        assert!(arbitration.arbitrator == arbitrator, "Not the arbitrator");
        assert!(!arbitration.resolved, "Already resolved");

        let mut escrow: Escrow = env
            .storage()
            .persistent()
            .get(&DataKey::Escrow(escrow_id.clone()))
            .expect("Escrow not found");

        let client = token::Client::new(&env, &escrow.token);
        let recipient = if release_to_worker { &escrow.to } else { &escrow.from };
        client.transfer(&env.current_contract_address(), recipient, &escrow.amount);

        if release_to_worker {
            escrow.released = true;
        } else {
            escrow.cancelled = true;
        }
        arbitration.resolved = true;

        env.storage().persistent().set(&DataKey::Escrow(escrow_id.clone()), &escrow);
        env.storage().persistent().set(&DataKey::Arbitration(escrow_id.clone()), &arbitration);

        env.events().publish(
            (symbol_short!("ArbRes"), escrow_id, arbitrator),
            release_to_worker,
        );
    }

    /// Get arbitration details for an escrow.
    pub fn get_arbitration(env: Env, escrow_id: Symbol) -> Option<Arbitration> {
        env.storage().persistent().get(&DataKey::Arbitration(escrow_id))
    }

    // -------------------------------------------------------------------------
    // Upgrade
    // -------------------------------------------------------------------------

    /// Upgrade the contract WASM in-place, preserving the contract ID and all storage.
    ///
    /// # Parameters
    /// - `new_wasm_hash`: The hash returned by `stellar contract install` for the new WASM.
    ///
    // -------------------------------------------------------------------------
    // Schema migration (#535)
    // -------------------------------------------------------------------------

    /// Return the current storage schema version.
    pub fn get_schema_version(env: Env) -> u32 {
        env.storage()
            .persistent()
            .get(&DataKey::SchemaVersion)
            .unwrap_or(1u32)
    }

    /// Run version-specific storage migration logic.
    ///
    /// Guards:
    /// - Caller must hold `ROLE_ADMIN`.
    /// - `expected_version` must equal the current schema version (prevents double-run
    ///   and out-of-order migrations).
    /// - After success the version is bumped to `expected_version + 1`.
    ///
    /// # Panics
    /// - `"Missing role"` if `admin` does not hold `ROLE_ADMIN`.
    /// - `"Wrong schema version"` if current version ≠ `expected_version`.
    ///
    /// # Events
    /// Emits `("Migrated",)` with data `(expected_version, expected_version + 1)`.
    pub fn migrate(env: Env, admin: Address, expected_version: u32) {
        Self::require_role(&env, &Symbol::new(&env, ROLE_ADMIN), &admin);

        let current: u32 = env
            .storage()
            .persistent()
            .get(&DataKey::SchemaVersion)
            .unwrap_or(1u32);

        assert!(current == expected_version, "Wrong schema version");

        // ---- version-specific migration logic --------------------------------
        if expected_version == 1 {
            // Example: no structural change needed for v1→v2 in this release.
        }
        // ----------------------------------------------------------------------

        let new_version = expected_version.checked_add(1).expect("Version overflow");
        env.storage().persistent().set(&DataKey::SchemaVersion, &new_version);

        env.events().publish(
            (symbol_short!("Migrated"),),
            (expected_version, new_version),
        );
    }

    // -------------------------------------------------------------------------
    // Upgrade
    // -------------------------------------------------------------------------

    /// Upgrade the contract WASM in-place, preserving the contract ID and all storage.
    ///
    /// # Parameters
    /// - `new_wasm_hash`: The hash returned by `stellar contract install` for the new WASM.
    ///
    /// # Panics
    /// - `"Not initialized"` if [`initialize`] has not been called.
    /// - `"Unauthorized"` if caller does not match the stored admin.
    pub fn upgrade(env: Env, new_wasm_hash: soroban_sdk::BytesN<32>) {
        let admin = Self::get_admin(env.clone());
        // `require_role` already enforces `admin.require_auth()`; calling it again
        // here would double-authorize the same frame and panic with an auth error.
        let upgrader_role = Self::role_symbol(&env, ROLE_UPGRADER);
        Self::require_role(&env, &upgrader_role, &admin);
        env.deployer().update_current_contract_wasm(new_wasm_hash);
    }
}

// =============================================================================
// Tests
// =============================================================================

// =============================================================================
// Optimised view helpers (issue #1148)
// =============================================================================

/// Per-call read-caching view helpers for all read-heavy query patterns.
/// See module docs for the storage-read budget table.
pub mod views;

// =============================================================================
// Storage-access benchmarks (issue #1148)
// =============================================================================

#[cfg(test)]
mod contract_benchmarks;

// Integration-style unit tests and the contract-upgrade testing framework
// live in `test.rs`; the `mod tests` block below holds the original inline tests.
#[cfg(test)]
mod test;

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::{
        testutils::{Address as _, Ledger, LedgerInfo},
        token::{Client as TokenClient, StellarAssetClient},
        Address, Env, Symbol, Vec,
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
        fn new() -> Self {
            let env = Env::default();
            env.mock_all_auths();

            let admin = Address::generate(&env);
            let payer = Address::generate(&env);
            let worker = Address::generate(&env);

            let token_id = env.register_stellar_asset_contract_v2(admin.clone());
            let token_addr = token_id.address();
            StellarAssetClient::new(&env, &token_addr).mint(&payer, &1_000_000);

            let contract_id = env.register_contract(None, MarketContract);
            MarketContractClient::new(&env, &contract_id).initialize(&admin, &0, &admin);

            // Grant all operational roles to the bootstrap admin for convenience in tests.
            let client = MarketContractClient::new(&env, &contract_id);
            client.grant_role(&admin, &Symbol::new(&env, ROLE_PAUSER), &admin);
            client.grant_role(&admin, &Symbol::new(&env, ROLE_FEE_MANAGER), &admin);
            client.grant_role(&admin, &Symbol::new(&env, ROLE_DISPUTE_MGR), &admin);
            client.grant_role(&admin, &Symbol::new(&env, ROLE_UPGRADER), &admin);

            TestEnv { env, contract_id, admin, payer, worker, token_addr }
        }

        fn client(&self) -> MarketContractClient {
            MarketContractClient::new(&self.env, &self.contract_id)
        }

        fn token_balance(&self, addr: &Address) -> i128 {
            TokenClient::new(&self.env, &self.token_addr).balance(addr)
        }

        fn id(&self) -> Symbol {
            Symbol::new(&self.env, "escrow1")
        }

        fn set_time(&self, ts: u64) {
            self.env.ledger().set(LedgerInfo {
                timestamp: ts,
                protocol_version: 22,
                sequence_number: 1,
                network_id: Default::default(),
                base_reserve: 10,
                min_temp_entry_ttl: 1,
                min_persistent_entry_ttl: 1,
                max_entry_ttl: 100_000,
            });
        }
    }

    #[test]
    fn test_tip_transfers_tokens() {
        let t = TestEnv::new();
        t.client().tip(&t.payer, &t.worker, &t.token_addr, &500_000);
        assert_eq!(t.token_balance(&t.worker), 500_000);
        assert_eq!(t.token_balance(&t.payer), 500_000);
    }

    // -------------------------------------------------------------------------
    // Fee mechanism tests (#532)
    // -------------------------------------------------------------------------

    #[test]
    fn test_tip_with_fee_deducts_correctly() {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let payer = Address::generate(&env);
        let worker = Address::generate(&env);
        let treasury = Address::generate(&env);

        let token_id = env.register_stellar_asset_contract_v2(admin.clone());
        let token_addr = token_id.address();
        StellarAssetClient::new(&env, &token_addr).mint(&payer, &1_000_000);

        let contract_id = env.register_contract(None, MarketContract);
        // Initialize with 100 bps (1%) fee, treasury as recipient
        MarketContractClient::new(&env, &contract_id).initialize(&admin, &100, &treasury);

        let client = MarketContractClient::new(&env, &contract_id);
        client.tip(&payer, &worker, &token_addr, &100_000);

        // fee = 100_000 * 100 / 10_000 = 1_000
        let token = TokenClient::new(&env, &token_addr);
        assert_eq!(token.balance(&worker), 99_000);
        assert_eq!(token.balance(&treasury), 1_000);
        assert_eq!(token.balance(&payer), 900_000);
    }

    #[test]
    fn test_tip_zero_fee_sends_full_amount() {
        let t = TestEnv::new(); // initialized with fee_bps = 0
        let treasury_before = t.token_balance(&t.admin); // admin is fee_recipient in TestEnv
        t.client().tip(&t.payer, &t.worker, &t.token_addr, &200_000);
        // No fee deducted — worker gets full amount
        assert_eq!(t.token_balance(&t.worker), 200_000);
        assert_eq!(t.token_balance(&t.payer), 800_000);
        // Treasury balance unchanged
        assert_eq!(t.token_balance(&t.admin), treasury_before);
    }

    #[test]
    fn test_set_treasury_updates_fee_recipient() {
        let t = TestEnv::new();
        let new_treasury = Address::generate(&t.env);

        // Update treasury (admin holds ROLE_ADMIN)
        t.client().set_treasury(&t.admin, &new_treasury);

        // Verify config reflects new treasury
        let config = t.client().get_config();
        assert_eq!(config.fee_recipient, new_treasury);
    }

    #[test]
    #[should_panic(expected = "Missing role")]
    fn test_set_treasury_non_admin_panics() {
        let t = TestEnv::new();
        let stranger = Address::generate(&t.env);
        let new_treasury = Address::generate(&t.env);
        t.client().set_treasury(&stranger, &new_treasury);
    }

    #[test]
    fn test_tip_fee_goes_to_updated_treasury() {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let payer = Address::generate(&env);
        let worker = Address::generate(&env);
        let old_treasury = Address::generate(&env);
        let new_treasury = Address::generate(&env);

        let token_id = env.register_stellar_asset_contract_v2(admin.clone());
        let token_addr = token_id.address();
        StellarAssetClient::new(&env, &token_addr).mint(&payer, &1_000_000);

        let contract_id = env.register_contract(None, MarketContract);
        MarketContractClient::new(&env, &contract_id).initialize(&admin, &200, &old_treasury);

        let client = MarketContractClient::new(&env, &contract_id);
        // Update treasury to new_treasury
        client.set_treasury(&admin, &new_treasury);

        client.tip(&payer, &worker, &token_addr, &100_000);

        // fee = 100_000 * 200 / 10_000 = 2_000
        let token = TokenClient::new(&env, &token_addr);
        assert_eq!(token.balance(&new_treasury), 2_000);
        assert_eq!(token.balance(&old_treasury), 0);
        assert_eq!(token.balance(&worker), 98_000);
    }

    #[test]
    fn test_create_escrow_locks_funds() {
        let t = TestEnv::new();
        let id = t.id();
        t.client().create_escrow(&id, &t.payer, &t.worker, &t.token_addr, &300_000, &9999);

        assert_eq!(t.token_balance(&t.payer), 700_000);
        assert_eq!(t.token_balance(&t.contract_id), 300_000);

        let escrow = t.client().get_escrow(&id).unwrap();
        assert_eq!(escrow.amount, 300_000);
        assert_eq!(escrow.expiry, 9999);
        assert!(!escrow.released);
        assert!(!escrow.cancelled);
    }

    #[test]
    #[should_panic(expected = "Escrow id already exists")]
    fn test_create_escrow_duplicate_id_panics() {
        let t = TestEnv::new();
        let id = t.id();
        t.client().create_escrow(&id, &t.payer, &t.worker, &t.token_addr, &100_000, &9999);
        t.client().create_escrow(&id, &t.payer, &t.worker, &t.token_addr, &100_000, &9999);
    }

    #[test]
    #[should_panic(expected = "Amount must be positive")]
    fn test_create_escrow_zero_amount_panics() {
        let t = TestEnv::new();
        let id = t.id();
        t.client().create_escrow(&id, &t.payer, &t.worker, &t.token_addr, &0, &9999);
    }

    #[test]
    fn test_release_by_payer() {
        let t = TestEnv::new();
        let id = t.id();
        let client = t.client();
        client.create_escrow(&id, &t.payer, &t.worker, &t.token_addr, &300_000, &9999);
        client.release_escrow(&id, &t.payer);

        assert_eq!(t.token_balance(&t.worker), 300_000);
        assert_eq!(t.token_balance(&t.contract_id), 0);
        assert!(client.get_escrow(&id).unwrap().released);
    }

    #[test]
    fn test_release_by_worker() {
        let t = TestEnv::new();
        let id = t.id();
        let client = t.client();
        client.create_escrow(&id, &t.payer, &t.worker, &t.token_addr, &300_000, &9999);
        client.release_escrow(&id, &t.worker);

        assert_eq!(t.token_balance(&t.worker), 300_000);
        assert!(client.get_escrow(&id).unwrap().released);
    }

    #[test]
    #[should_panic(expected = "Not authorized")]
    fn test_release_by_stranger_panics() {
        let t = TestEnv::new();
        let id = t.id();
        let stranger = Address::generate(&t.env);
        t.client().create_escrow(&id, &t.payer, &t.worker, &t.token_addr, &300_000, &9999);
        t.client().release_escrow(&id, &stranger);
    }

    #[test]
    #[should_panic(expected = "Already released")]
    fn test_release_twice_panics() {
        let t = TestEnv::new();
        let id = t.id();
        let client = t.client();
        client.create_escrow(&id, &t.payer, &t.worker, &t.token_addr, &300_000, &9999);
        client.release_escrow(&id, &t.payer);
        client.release_escrow(&id, &t.payer);
    }

    #[test]
    fn test_cancel_after_expiry_refunds_payer() {
        let t = TestEnv::new();
        let id = t.id();
        let client = t.client();

        t.set_time(1000);
        client.create_escrow(&id, &t.payer, &t.worker, &t.token_addr, &300_000, &2000);
        t.set_time(3000);
        client.cancel_escrow(&id, &t.payer);

        assert_eq!(t.token_balance(&t.payer), 1_000_000);
        assert_eq!(t.token_balance(&t.contract_id), 0);
        assert!(client.get_escrow(&id).unwrap().cancelled);
    }

    #[test]
    fn test_cancel_at_exact_expiry_succeeds() {
        let t = TestEnv::new();
        let id = t.id();
        let client = t.client();

        t.set_time(1000);
        client.create_escrow(&id, &t.payer, &t.worker, &t.token_addr, &300_000, &2000);
        t.set_time(2000);
        client.cancel_escrow(&id, &t.payer);

        assert!(client.get_escrow(&id).unwrap().cancelled);
    }

    #[test]
    #[should_panic(expected = "Escrow not yet expired")]
    fn test_cancel_before_expiry_panics() {
        let t = TestEnv::new();
        let id = t.id();

        t.set_time(500);
        t.client().create_escrow(&id, &t.payer, &t.worker, &t.token_addr, &300_000, &2000);
        t.client().cancel_escrow(&id, &t.payer);
    }

    #[test]
    #[should_panic(expected = "Not authorized")]
    fn test_cancel_by_worker_panics() {
        let t = TestEnv::new();
        let id = t.id();

        t.set_time(5000);
        t.client().create_escrow(&id, &t.payer, &t.worker, &t.token_addr, &300_000, &2000);
        t.client().cancel_escrow(&id, &t.worker);
    }

    #[test]
    #[should_panic(expected = "Already cancelled")]
    fn test_cancel_twice_panics() {
        let t = TestEnv::new();
        let id = t.id();
        let client = t.client();

        t.set_time(5000);
        client.create_escrow(&id, &t.payer, &t.worker, &t.token_addr, &300_000, &2000);
        client.cancel_escrow(&id, &t.payer);
        client.cancel_escrow(&id, &t.payer);
    }

    #[test]
    #[should_panic(expected = "Escrow cancelled")]
    fn test_release_after_cancel_panics() {
        let t = TestEnv::new();
        let id = t.id();
        let client = t.client();

        t.set_time(5000);
        client.create_escrow(&id, &t.payer, &t.worker, &t.token_addr, &300_000, &2000);
        client.cancel_escrow(&id, &t.payer);
        client.release_escrow(&id, &t.payer);
    }

    #[test]
    fn test_get_escrow_nonexistent_returns_none() {
        let t = TestEnv::new();
        let id = Symbol::new(&t.env, "nope");
        assert!(t.client().get_escrow(&id).is_none());
    }

    // -------------------------------------------------------------------------
    // Multi-sig escrow tests (#337)
    // -------------------------------------------------------------------------

    #[test]
    fn test_multisig_escrow_releases_at_threshold() {
        let t = TestEnv::new();
        let id = Symbol::new(&t.env, "ms1");
        let s1 = Address::generate(&t.env);
        let s2 = Address::generate(&t.env);
        let signers = soroban_sdk::vec![&t.env, s1.clone(), s2.clone()];

        t.client().create_multisig_escrow(&id, &t.payer, &t.worker, &t.token_addr, &200_000, &9999, &signers, &2);
        assert_eq!(t.token_balance(&t.contract_id), 200_000);

        t.client().approve_multisig_release(&id, &s1);
        // not yet released
        assert_eq!(t.token_balance(&t.worker), 0);

        t.client().approve_multisig_release(&id, &s2);
        // threshold reached
        assert_eq!(t.token_balance(&t.worker), 200_000);
        assert!(t.client().get_multisig_escrow(&id).unwrap().released);
    }

    #[test]
    #[should_panic(expected = "Not a signer")]
    fn test_multisig_approve_non_signer_panics() {
        let t = TestEnv::new();
        let id = Symbol::new(&t.env, "ms2");
        let s1 = Address::generate(&t.env);
        let signers = soroban_sdk::vec![&t.env, s1.clone()];
        t.client().create_multisig_escrow(&id, &t.payer, &t.worker, &t.token_addr, &100_000, &9999, &signers, &1);
        let stranger = Address::generate(&t.env);
        t.client().approve_multisig_release(&id, &stranger);
    }

    #[test]
    #[should_panic(expected = "Already approved")]
    fn test_multisig_double_approve_panics() {
        let t = TestEnv::new();
        let id = Symbol::new(&t.env, "ms3");
        let s1 = Address::generate(&t.env);
        let s2 = Address::generate(&t.env);
        let signers = soroban_sdk::vec![&t.env, s1.clone(), s2.clone()];
        t.client().create_multisig_escrow(&id, &t.payer, &t.worker, &t.token_addr, &100_000, &9999, &signers, &2);
        t.client().approve_multisig_release(&id, &s1);
        t.client().approve_multisig_release(&id, &s1);
    }

    #[test]
    fn test_multisig_cancel_after_expiry() {
        let t = TestEnv::new();
        let id = Symbol::new(&t.env, "ms4");
        let s1 = Address::generate(&t.env);
        let signers = soroban_sdk::vec![&t.env, s1.clone()];

         t.set_time(1000);
         t.client().create_multisig_escrow(&id, &t.payer, &t.worker, &t.token_addr, &100_000, &2000, &signers, &1);
         t.set_time(3000);
         t.client().cancel_multisig_escrow(&id, &t.payer);

         assert_eq!(t.token_balance(&t.payer), 1_000_000);
         assert!(t.client().get_multisig_escrow(&id).unwrap().cancelled);
     }

    // ── Additional multi-sig edge-case tests ─────────────────────────────────

    #[test]
    fn test_multisig_threshold_one_releases_immediately() {
        let t = TestEnv::new();
        let id = Symbol::new(&t.env, "ms5");
        let s1 = Address::generate(&t.env);
        let signers = soroban_sdk::vec![&t.env, s1.clone()];
        t.client().create_multisig_escrow(&id, &t.payer, &t.worker, &t.token_addr, &50_000, &9999, &signers, &1);

        t.client().approve_multisig_release(&id, &s1);

        assert_eq!(t.token_balance(&t.worker), 50_000);
        assert!(t.client().get_multisig_escrow(&id).unwrap().released);
    }

    #[test]
    #[should_panic(expected = "MultiSigEscrow id already exists")]
    fn test_multisig_duplicate_id_panics() {
        let t = TestEnv::new();
        let id = Symbol::new(&t.env, "ms6");
        let s1 = Address::generate(&t.env);
        let signers = soroban_sdk::vec![&t.env, s1.clone()];
        t.client().create_multisig_escrow(&id, &t.payer, &t.worker, &t.token_addr, &100_000, &9999, &signers, &1);
        t.client().create_multisig_escrow(&id, &t.payer, &t.worker, &t.token_addr, &100_000, &9999, &signers, &1);
    }

    #[test]
    #[should_panic(expected = "Amount must be positive")]
    fn test_multisig_zero_amount_panics() {
        let t = TestEnv::new();
        let id = Symbol::new(&t.env, "ms7");
        let s1 = Address::generate(&t.env);
        let signers = soroban_sdk::vec![&t.env, s1.clone()];
        t.client().create_multisig_escrow(&id, &t.payer, &t.worker, &t.token_addr, &0, &9999, &signers, &1);
    }

    #[test]
    #[should_panic(expected = "Invalid threshold")]
    fn test_multisig_threshold_exceeds_signers_panics() {
        let t = TestEnv::new();
        let id = Symbol::new(&t.env, "ms8");
        let s1 = Address::generate(&t.env);
        let signers = soroban_sdk::vec![&t.env, s1.clone()];
        // threshold 2 but only 1 signer
        t.client().create_multisig_escrow(&id, &t.payer, &t.worker, &t.token_addr, &100_000, &9999, &signers, &2);
    }

    #[test]
    #[should_panic(expected = "Invalid threshold")]
    fn test_multisig_zero_threshold_panics() {
        let t = TestEnv::new();
        let id = Symbol::new(&t.env, "ms9");
        let s1 = Address::generate(&t.env);
        let signers = soroban_sdk::vec![&t.env, s1.clone()];
        t.client().create_multisig_escrow(&id, &t.payer, &t.worker, &t.token_addr, &100_000, &9999, &signers, &0);
    }

    #[test]
    #[should_panic(expected = "Already released")]
    fn test_multisig_approve_after_release_panics() {
        let t = TestEnv::new();
        let id = Symbol::new(&t.env, "ms10");
        let s1 = Address::generate(&t.env);
        let signers = soroban_sdk::vec![&t.env, s1.clone()];
        t.client().create_multisig_escrow(&id, &t.payer, &t.worker, &t.token_addr, &100_000, &9999, &signers, &1);
        // First approval releases (threshold=1)
        t.client().approve_multisig_release(&id, &s1);
        // Attempt second approval on a released escrow
        t.client().approve_multisig_release(&id, &s1);
    }

    #[test]
    #[should_panic(expected = "Escrow not yet expired")]
    fn test_multisig_cancel_before_expiry_panics() {
        let t = TestEnv::new();
        let id = Symbol::new(&t.env, "ms11");
        let s1 = Address::generate(&t.env);
        let signers = soroban_sdk::vec![&t.env, s1.clone()];
        t.set_time(500);
        t.client().create_multisig_escrow(&id, &t.payer, &t.worker, &t.token_addr, &100_000, &2000, &signers, &1);
        // Try to cancel before expiry
        t.client().cancel_multisig_escrow(&id, &t.payer);
    }

    #[test]
    #[should_panic(expected = "Already cancelled")]
    fn test_multisig_cancel_twice_panics() {
        let t = TestEnv::new();
        let id = Symbol::new(&t.env, "ms12");
        let s1 = Address::generate(&t.env);
        let signers = soroban_sdk::vec![&t.env, s1.clone()];
        t.set_time(1000);
        t.client().create_multisig_escrow(&id, &t.payer, &t.worker, &t.token_addr, &100_000, &2000, &signers, &1);
        t.set_time(3000);
        t.client().cancel_multisig_escrow(&id, &t.payer);
        t.client().cancel_multisig_escrow(&id, &t.payer);
    }

    #[test]
    #[should_panic(expected = "Not authorized")]
    fn test_multisig_cancel_by_non_payer_panics() {
        let t = TestEnv::new();
        let id = Symbol::new(&t.env, "ms13");
        let s1 = Address::generate(&t.env);
        let signers = soroban_sdk::vec![&t.env, s1.clone()];
        t.set_time(1000);
        t.client().create_multisig_escrow(&id, &t.payer, &t.worker, &t.token_addr, &100_000, &2000, &signers, &1);
        t.set_time(3000);
        // worker tries to cancel — only payer (from) is allowed
        t.client().cancel_multisig_escrow(&id, &t.worker);
    }

    #[test]
    #[should_panic(expected = "Contract is paused")]
    fn test_multisig_create_while_paused_panics() {
        let t = TestEnv::new();
        t.client().pause(&t.admin);
        let id = Symbol::new(&t.env, "ms14");
        let s1 = Address::generate(&t.env);
        let signers = soroban_sdk::vec![&t.env, s1.clone()];
        t.client().create_multisig_escrow(&id, &t.payer, &t.worker, &t.token_addr, &100_000, &9999, &signers, &1);
    }

    #[test]
    #[should_panic(expected = "Contract is paused")]
    fn test_multisig_approve_while_paused_panics() {
        let t = TestEnv::new();
        let id = Symbol::new(&t.env, "ms15");
        let s1 = Address::generate(&t.env);
        let signers = soroban_sdk::vec![&t.env, s1.clone()];
        t.client().create_multisig_escrow(&id, &t.payer, &t.worker, &t.token_addr, &100_000, &9999, &signers, &1);
        t.client().pause(&t.admin);
        t.client().approve_multisig_release(&id, &s1);
    }

    // ── Multi-sig arbitration tests ──────────────────────────────────────────

    #[test]
    fn test_multisig_arbitration_releases_to_worker() {
        let t = TestEnv::new();
        let id = Symbol::new(&t.env, "msa1");
        let s1 = Address::generate(&t.env);
        let arbitrator = Address::generate(&t.env);
        let signers = soroban_sdk::vec![&t.env, s1.clone()];

        t.client().add_arbitrator(&arbitrator);
        t.client().create_multisig_escrow(&id, &t.payer, &t.worker, &t.token_addr, &200_000, &9999, &signers, &2);

        // Payer requests arbitration with 0 fee to keep balances simple
        t.client().request_multisig_arbitration(&id, &t.payer, &arbitrator, &0);

        let arb = t.client().get_multisig_arbitration(&id).unwrap();
        assert!(!arb.resolved);
        assert_eq!(arb.arbitrator, arbitrator);

        // Arbitrator resolves in worker's favour
        t.client().resolve_multisig_arbitration(&id, &arbitrator, &true);

        assert_eq!(t.token_balance(&t.worker), 200_000);
        assert!(t.client().get_multisig_escrow(&id).unwrap().released);
        assert!(t.client().get_multisig_arbitration(&id).unwrap().resolved);
    }

    #[test]
    fn test_multisig_arbitration_refunds_payer() {
        let t = TestEnv::new();
        let id = Symbol::new(&t.env, "msa2");
        let s1 = Address::generate(&t.env);
        let arbitrator = Address::generate(&t.env);
        let signers = soroban_sdk::vec![&t.env, s1.clone()];

        t.client().add_arbitrator(&arbitrator);
        t.client().create_multisig_escrow(&id, &t.payer, &t.worker, &t.token_addr, &100_000, &9999, &signers, &2);
        t.client().request_multisig_arbitration(&id, &t.payer, &arbitrator, &0);

        // Arbitrator resolves in payer's favour
        t.client().resolve_multisig_arbitration(&id, &arbitrator, &false);

        assert_eq!(t.token_balance(&t.payer), 1_000_000);
        assert!(t.client().get_multisig_escrow(&id).unwrap().cancelled);
    }

    #[test]
    #[should_panic(expected = "Arbitration already requested")]
    fn test_multisig_arbitration_duplicate_panics() {
        let t = TestEnv::new();
        let id = Symbol::new(&t.env, "msa3");
        let s1 = Address::generate(&t.env);
        let arbitrator = Address::generate(&t.env);
        let signers = soroban_sdk::vec![&t.env, s1.clone()];

        t.client().add_arbitrator(&arbitrator);
        t.client().create_multisig_escrow(&id, &t.payer, &t.worker, &t.token_addr, &100_000, &9999, &signers, &2);
        t.client().request_multisig_arbitration(&id, &t.payer, &arbitrator, &0);
        t.client().request_multisig_arbitration(&id, &t.payer, &arbitrator, &0);
    }

    #[test]
    #[should_panic(expected = "Not authorized")]
    fn test_multisig_arbitration_by_stranger_panics() {
        let t = TestEnv::new();
        let id = Symbol::new(&t.env, "msa4");
        let s1 = Address::generate(&t.env);
        let arbitrator = Address::generate(&t.env);
        let signers = soroban_sdk::vec![&t.env, s1.clone()];
        let stranger = Address::generate(&t.env);

        t.client().add_arbitrator(&arbitrator);
        t.client().create_multisig_escrow(&id, &t.payer, &t.worker, &t.token_addr, &100_000, &9999, &signers, &2);
        t.client().request_multisig_arbitration(&id, &stranger, &arbitrator, &0);
    }

    #[test]
    #[should_panic(expected = "Invalid arbitrator")]
    fn test_multisig_arbitration_unregistered_arbitrator_panics() {
        let t = TestEnv::new();
        let id = Symbol::new(&t.env, "msa5");
        let s1 = Address::generate(&t.env);
        let fake_arbitrator = Address::generate(&t.env);
        let signers = soroban_sdk::vec![&t.env, s1.clone()];

        t.client().create_multisig_escrow(&id, &t.payer, &t.worker, &t.token_addr, &100_000, &9999, &signers, &2);
        t.client().request_multisig_arbitration(&id, &t.payer, &fake_arbitrator, &0);
    }

    #[test]
    #[should_panic(expected = "Already resolved")]
    fn test_multisig_resolve_arbitration_twice_panics() {
        let t = TestEnv::new();
        let id = Symbol::new(&t.env, "msa6");
        let s1 = Address::generate(&t.env);
        let arbitrator = Address::generate(&t.env);
        let signers = soroban_sdk::vec![&t.env, s1.clone()];

        t.client().add_arbitrator(&arbitrator);
        t.client().create_multisig_escrow(&id, &t.payer, &t.worker, &t.token_addr, &100_000, &9999, &signers, &2);
        t.client().request_multisig_arbitration(&id, &t.payer, &arbitrator, &0);
        t.client().resolve_multisig_arbitration(&id, &arbitrator, &true);
        t.client().resolve_multisig_arbitration(&id, &arbitrator, &true);
    }

    // -------------------------------------------------------------------------
    // Migration tests (#535)
    // -------------------------------------------------------------------------

    #[test]
    fn test_initial_schema_version_is_1() {
        let t = TestEnv::new();
        assert_eq!(t.client().get_schema_version(), 1u32);
    }

    #[test]
    fn test_migrate_v1_to_v2_bumps_version() {
        let t = TestEnv::new();
        t.client().migrate(&t.admin, &1u32);
        assert_eq!(t.client().get_schema_version(), 2u32);
    }

    #[test]
    #[should_panic(expected = "Wrong schema version")]
    fn test_migrate_double_run_panics() {
        let t = TestEnv::new();
        t.client().migrate(&t.admin, &1u32);
        t.client().migrate(&t.admin, &1u32);
    }

    #[test]
    #[should_panic(expected = "Wrong schema version")]
    fn test_migrate_wrong_version_panics() {
        let t = TestEnv::new();
        t.client().migrate(&t.admin, &2u32);
    }

    #[test]
    #[should_panic(expected = "Missing role")]
    fn test_migrate_non_admin_panics() {
        let t = TestEnv::new();
        let stranger = Address::generate(&t.env);
        t.client().migrate(&stranger, &1u32);
    }

    #[test]
    fn test_migrate_sequential_versions() {
        let t = TestEnv::new();
        t.client().migrate(&t.admin, &1u32);
        assert_eq!(t.client().get_schema_version(), 2u32);
        t.client().migrate(&t.admin, &2u32);
        assert_eq!(t.client().get_schema_version(), 3u32);
    }
 }

// ---------------------------------------------------------------------------
// Admin access control & upgrade authorization tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod admin_tests {
    use super::*;
    use soroban_sdk::{testutils::Address as _, Address, BytesN, Env};

    fn setup() -> (Env, Address, Address) {
        let env = Env::default();
        env.mock_all_auths();
        let contract = env.register_contract(None, MarketContract);
        let admin = Address::generate(&env);
        let fee_recipient = Address::generate(&env);
        MarketContractClient::new(&env, &contract).initialize(&admin, &100, &fee_recipient);
        (env, contract, admin)
    }

    #[test]
    fn test_initialize_sets_admin() {
        let (env, contract, admin) = setup();
        assert_eq!(MarketContractClient::new(&env, &contract).get_admin(), admin);
    }

    #[test]
    #[should_panic(expected = "Not initialized")]
    fn test_get_admin_uninitialized_panics() {
        let env = Env::default();
        let contract = env.register_contract(None, MarketContract);
        MarketContractClient::new(&env, &contract).get_admin();
    }

    #[test]
    fn test_set_admin_success() {
        let (env, contract, admin_old) = setup();
        let client = MarketContractClient::new(&env, &contract);
        let admin_new = Address::generate(&env);
        assert_eq!(client.get_admin(), admin_old);
        client.set_admin(&admin_new);
        assert_eq!(client.get_admin(), admin_new);
    }

    #[test]
    #[should_panic]
    fn test_set_admin_unauthorized_fails() {
        // No auths mocked: set_admin's require_auth on the stored admin must fail.
        let env = Env::default();
        let contract = env.register_contract(None, MarketContract);
        let client = MarketContractClient::new(&env, &contract);
        let admin_old = Address::generate(&env);
        let admin_new = Address::generate(&env);
        let fee_recipient = Address::generate(&env);
        client.initialize(&admin_old, &100, &fee_recipient);
        client.set_admin(&admin_new);
    }

    /// `upgrade` rejects callers when the stored admin lacks ROLE_UPGRADER.
    /// The role check runs before any WASM install, so it is testable in-process
    /// (a real WASM-swap upgrade is exercised in test.rs behind a feature flag).
    #[test]
    #[should_panic(expected = "Missing role")]
    fn test_upgrade_requires_upgrader_role() {
        let (env, contract, _admin) = setup();
        let new_wasm_hash = BytesN::from_array(&env, &[0u8; 32]);
        MarketContractClient::new(&env, &contract).upgrade(&new_wasm_hash);
    }
}
