#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, panic_with_error, symbol_short, token,
    Address, Env, Map, String, Vec,
};

#[cfg(test)]
extern crate std;

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TipWithMessage {
    pub sender: Address,
    pub creator: Address,
    pub amount: i128,
    pub message: String,
    pub metadata: Map<String, String>,
    pub timestamp: u64,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Milestone {
    pub id: u64,
    pub creator: Address,
    pub goal_amount: i128,
    pub current_amount: i128,
    pub description: String,
    pub deadline: Option<u64>,
    pub completed: bool,
}

/// A sponsor-funded matching program for a creator.
///
/// `match_ratio` is expressed in basis points of the tip amount:
///   100 = 1:1 (match equals tip), 200 = 2:1 (match is double the tip), etc.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MatchingProgram {
    pub id: u64,
    pub sponsor: Address,
    pub creator: Address,
    pub token: Address,
    pub match_ratio: u32,       // bps: 100 = 1:1, 200 = 2:1
    pub max_match_amount: i128, // total sponsor budget deposited
    pub current_matched: i128,  // how much has been matched so far
    pub active: bool,
}

/// Tracks an individual tip for refund purposes.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TipRecord {
    pub id: u64,
    pub sender: Address,
    pub creator: Address,
    pub token: Address,
    pub amount: i128,
    pub timestamp: u64,
    pub refunded: bool,
    pub refund_requested: bool,
}

/// Storage layout for persistent contract data.
#[derive(Clone)]
#[contracttype]
pub enum DataKey {
    /// Token contract address whitelist state (bool).
    TokenWhitelist(Address),
    /// Creator's currently withdrawable balance held by this contract per token.
    CreatorBalance(Address, Address), // (creator, token)
    /// Historical total tips ever received by creator per token.
    CreatorTotal(Address, Address),   // (creator, token)
    /// Emergency pause state (bool).
    Paused,
    /// Contract administrator (Address).
    Admin,
    /// Messages appended for a creator.
    CreatorMessages(Address),
    /// Current number of milestones for a creator (used for ID).
    MilestoneCounter(Address),
    /// Data for a specific milestone.
    Milestone(Address, u64),
    /// Active milestone IDs for a creator to track.
    ActiveMilestones(Address),
    /// Global tip counter (used as tip ID).
    TipCounter,
    /// Individual tip record by ID.
    TipRecord(u64),
    /// Global matching program counter.
    MatchingCounter,
    /// Individual matching program by ID.
    MatchingProgram(u64),
    /// Active matching program IDs for a creator.
    CreatorMatchingPrograms(Address),
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum TipJarError {
    AlreadyInitialized = 1,
    TokenNotWhitelisted = 2,
    InvalidAmount = 3,
    NothingToWithdraw = 4,
    MessageTooLong = 5,
    MilestoneNotFound = 6,
    MilestoneAlreadyCompleted = 7,
    InvalidGoalAmount = 8,
    Unauthorized = 9,
    TipNotFound = 10,
    AlreadyRefunded = 11,
    RefundNotRequested = 12,
    GracePeriodExpired = 13,
    MatchingProgramNotFound = 14,
    MatchingProgramInactive = 15,
    InvalidMatchRatio = 16,
}

/// Grace period for automatic refunds: 24 hours in seconds.
const GRACE_PERIOD_SECS: u64 = 86_400;

#[contract]
pub struct TipJarContract;

#[contractimpl]
impl TipJarContract {
    /// One-time setup to choose the administrator for the TipJar.
    pub fn init(env: Env, admin: Address) {
        if env.storage().instance().has(&DataKey::Admin) {
            panic_with_error!(&env, TipJarError::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Paused, &false);
    }

    /// Adds a token to the whitelist (Admin only).
    pub fn add_token(env: Env, admin: Address, token: Address) {
        admin.require_auth();
        let stored_admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        if admin != stored_admin {
            panic_with_error!(&env, TipJarError::Unauthorized);
        }
        env.storage()
            .instance()
            .set(&DataKey::TokenWhitelist(token), &true);
    }

    /// Removes a token from the whitelist (Admin only).
    pub fn remove_token(env: Env, admin: Address, token: Address) {
        admin.require_auth();
        let stored_admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        if admin != stored_admin {
            panic_with_error!(&env, TipJarError::Unauthorized);
        }
        env.storage()
            .instance()
            .set(&DataKey::TokenWhitelist(token), &false);
    }

    /// Moves `amount` tokens from `sender` into contract escrow for `creator`.
    /// Returns the tip ID for use in refund requests.
    pub fn tip(env: Env, sender: Address, creator: Address, token: Address, amount: i128) -> u64 {
        if Self::is_paused(&env) {
            panic!("Contract is paused");
        }
        if amount <= 0 {
            panic_with_error!(&env, TipJarError::InvalidAmount);
        }
        if !Self::is_whitelisted(env.clone(), token.clone()) {
            panic_with_error!(&env, TipJarError::TokenNotWhitelisted);
        }

        sender.require_auth();

        let token_client = token::Client::new(&env, &token);
        let contract_address = env.current_contract_address();

        token_client.transfer(&sender, &contract_address, &amount);

        let creator_balance_key = DataKey::CreatorBalance(creator.clone(), token.clone());
        let creator_total_key = DataKey::CreatorTotal(creator.clone(), token.clone());

        let next_balance: i128 = env.storage().persistent().get(&creator_balance_key).unwrap_or(0) + amount;
        let next_total: i128 = env.storage().persistent().get(&creator_total_key).unwrap_or(0) + amount;

        env.storage().persistent().set(&creator_balance_key, &next_balance);
        env.storage().persistent().set(&creator_total_key, &next_total);

        // Record the tip for refund tracking.
        let tip_id = Self::next_tip_id(&env);
        let record = TipRecord {
            id: tip_id,
            sender: sender.clone(),
            creator: creator.clone(),
            token: token.clone(),
            amount,
            timestamp: env.ledger().timestamp(),
            refunded: false,
            refund_requested: false,
        };
        env.storage().persistent().set(&DataKey::TipRecord(tip_id), &record);

        env.events()
            .publish((symbol_short!("tip"), creator, token), (sender, amount, tip_id));

        tip_id
    }

    /// Allows supporters to attach a note and metadata to a tip.
    /// Returns the tip ID for use in refund requests.
    pub fn tip_with_message(
        env: Env,
        sender: Address,
        creator: Address,
        token: Address,
        amount: i128,
        message: String,
        metadata: Map<String, String>,
    ) -> u64 {
        if Self::is_paused(&env) {
            panic!("Contract is paused");
        }
        if amount <= 0 {
            panic_with_error!(&env, TipJarError::InvalidAmount);
        }
        if message.len() > 280 {
            panic_with_error!(&env, TipJarError::MessageTooLong);
        }

        sender.require_auth();

        let token_client = token::Client::new(&env, &token);
        let contract_address = env.current_contract_address();

        token_client.transfer(&sender, &contract_address, &amount);

        let creator_balance_key = DataKey::CreatorBalance(creator.clone(), token.clone());
        let creator_total_key = DataKey::CreatorTotal(creator.clone(), token.clone());
        let msgs_key = DataKey::CreatorMessages(creator.clone());

        let current_balance: i128 = env.storage().persistent().get(&creator_balance_key).unwrap_or(0);
        let current_total: i128 = env.storage().persistent().get(&creator_total_key).unwrap_or(0);

        env.storage().persistent().set(&creator_balance_key, &(current_balance + amount));
        env.storage().persistent().set(&creator_total_key, &(current_total + amount));

        let timestamp = env.ledger().timestamp();
        let payload = TipWithMessage {
            sender: sender.clone(),
            creator: creator.clone(),
            amount,
            message: message.clone(),
            metadata: metadata.clone(),
            timestamp,
        };
        let mut messages: Vec<TipWithMessage> = env
            .storage()
            .persistent()
            .get(&msgs_key)
            .unwrap_or_else(|| Vec::new(&env));
        messages.push_back(payload);
        env.storage().persistent().set(&msgs_key, &messages);

        // Record the tip for refund tracking.
        let tip_id = Self::next_tip_id(&env);
        let record = TipRecord {
            id: tip_id,
            sender: sender.clone(),
            creator: creator.clone(),
            token: token.clone(),
            amount,
            timestamp,
            refunded: false,
            refund_requested: false,
        };
        env.storage().persistent().set(&DataKey::TipRecord(tip_id), &record);

        env.events().publish(
            (symbol_short!("tip_msg"), creator.clone()),
            (sender, amount, message, metadata, tip_id),
        );

        tip_id
    }

    /// Sender requests a refund for a tip.
    ///
    /// - Within the grace period (24 h): refund is issued immediately.
    /// - After the grace period: a refund request is recorded and the creator
    ///   must call `approve_refund` to release the funds.
    pub fn request_refund(env: Env, sender: Address, tip_id: u64) {
        if Self::is_paused(&env) {
            panic!("Contract is paused");
        }
        sender.require_auth();

        let mut record: TipRecord = env
            .storage()
            .persistent()
            .get(&DataKey::TipRecord(tip_id))
            .unwrap_or_else(|| panic_with_error!(&env, TipJarError::TipNotFound));

        if record.sender != sender {
            panic_with_error!(&env, TipJarError::Unauthorized);
        }
        if record.refunded {
            panic_with_error!(&env, TipJarError::AlreadyRefunded);
        }
        if record.refund_requested {
            // Already pending — nothing to do.
            return;
        }

        let now = env.ledger().timestamp();
        let within_grace = now <= record.timestamp + GRACE_PERIOD_SECS;

        if within_grace {
            // Automatic refund: deduct from creator balance and transfer back.
            Self::execute_refund(&env, &mut record);
            env.events().publish(
                (symbol_short!("refund"), record.creator.clone(), record.token.clone()),
                (sender, tip_id, record.amount, true), // true = auto
            );
        } else {
            // Mark as pending; creator must approve.
            record.refund_requested = true;
            env.storage().persistent().set(&DataKey::TipRecord(tip_id), &record);
            env.events().publish(
                (symbol_short!("ref_req"), record.creator.clone(), record.token.clone()),
                (sender, tip_id, record.amount),
            );
        }
    }

    /// Creator approves a pending refund request (post-grace-period).
    pub fn approve_refund(env: Env, creator: Address, tip_id: u64) {
        if Self::is_paused(&env) {
            panic!("Contract is paused");
        }
        creator.require_auth();

        let mut record: TipRecord = env
            .storage()
            .persistent()
            .get(&DataKey::TipRecord(tip_id))
            .unwrap_or_else(|| panic_with_error!(&env, TipJarError::TipNotFound));

        if record.creator != creator {
            panic_with_error!(&env, TipJarError::Unauthorized);
        }
        if record.refunded {
            panic_with_error!(&env, TipJarError::AlreadyRefunded);
        }
        if !record.refund_requested {
            panic_with_error!(&env, TipJarError::RefundNotRequested);
        }

        let sender = record.sender.clone();
        let amount = record.amount;
        let token = record.token.clone();

        Self::execute_refund(&env, &mut record);

        env.events().publish(
            (symbol_short!("refund"), creator, token),
            (sender, tip_id, amount, false), // false = creator-approved
        );
    }

    /// Returns the tip record for a given tip ID.
    pub fn get_tip_record(env: Env, tip_id: u64) -> TipRecord {
        env.storage()
            .persistent()
            .get(&DataKey::TipRecord(tip_id))
            .unwrap_or_else(|| panic_with_error!(&env, TipJarError::TipNotFound))
    }

    /// Returns total historical tips for a creator.
    pub fn get_total_tips(env: Env, creator: Address, token: Address) -> i128 {
        env.storage().persistent().get(&DataKey::CreatorTotal(creator, token)).unwrap_or(0)
    }

    /// Returns stored messages for a creator.
    pub fn get_messages(env: Env, creator: Address) -> Vec<TipWithMessage> {
        env.storage()
            .persistent()
            .get(&DataKey::CreatorMessages(creator))
            .unwrap_or_else(|| Vec::new(&env))
    }

    /// Returns currently withdrawable escrowed tips for a creator per token.
    pub fn get_withdrawable_balance(env: Env, creator: Address, token: Address) -> i128 {
        env.storage().persistent().get(&DataKey::CreatorBalance(creator, token)).unwrap_or(0)
    }

    /// Allows creator to withdraw their accumulated escrowed tips for a specific token.
    pub fn withdraw(env: Env, creator: Address, token: Address) {
        if Self::is_paused(&env) {
            panic!("Contract is paused");
        }
        creator.require_auth();

        let key = DataKey::CreatorBalance(creator.clone(), token.clone());
        let amount: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        if amount <= 0 {
            panic_with_error!(&env, TipJarError::NothingToWithdraw);
        }

        let token_client = token::Client::new(&env, &token);
        let contract_address = env.current_contract_address();

        token_client.transfer(&contract_address, &creator, &amount);
        env.storage().persistent().set(&key, &0i128);

        env.events()
            .publish((symbol_short!("withdraw"), creator, token), amount);
    }

    pub fn is_whitelisted(env: Env, token: Address) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::TokenWhitelist(token))
            .unwrap_or(false)
    }

    /// Emergency pause to stop all state-changing activities (Admin only).
    pub fn pause(env: Env, admin: Address) {
        admin.require_auth();
        let stored_admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap_or_else(|| panic!("Not initialized"));
        if admin != stored_admin {
            panic_with_error!(&env, TipJarError::Unauthorized);
        }
        env.storage().instance().set(&DataKey::Paused, &true);
    }

    /// Resume contract activities after an emergency pause (Admin only).
    pub fn unpause(env: Env, admin: Address) {
        admin.require_auth();
        let stored_admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap_or_else(|| panic!("Not initialized"));
        if admin != stored_admin {
            panic_with_error!(&env, TipJarError::Unauthorized);
        }
        env.storage().instance().set(&DataKey::Paused, &false);
    }

    /// Sponsor creates a matching program and deposits `max_match_amount` tokens
    /// into the contract as the matching budget.
    ///
    /// `match_ratio` is in basis points: 100 = 1:1, 200 = 2:1.
    pub fn create_matching_program(
        env: Env,
        sponsor: Address,
        creator: Address,
        token: Address,
        match_ratio: u32,
        max_match_amount: i128,
    ) -> u64 {
        if Self::is_paused(&env) {
            panic!("Contract is paused");
        }
        if match_ratio == 0 {
            panic_with_error!(&env, TipJarError::InvalidMatchRatio);
        }
        if max_match_amount <= 0 {
            panic_with_error!(&env, TipJarError::InvalidAmount);
        }
        if !Self::is_whitelisted(env.clone(), token.clone()) {
            panic_with_error!(&env, TipJarError::TokenNotWhitelisted);
        }

        sponsor.require_auth();

        // Sponsor deposits the full matching budget upfront.
        let token_client = token::Client::new(&env, &token);
        token_client.transfer(&sponsor, &env.current_contract_address(), &max_match_amount);

        let program_id = Self::next_matching_id(&env);
        let program = MatchingProgram {
            id: program_id,
            sponsor: sponsor.clone(),
            creator: creator.clone(),
            token: token.clone(),
            match_ratio,
            max_match_amount,
            current_matched: 0,
            active: true,
        };
        env.storage()
            .persistent()
            .set(&DataKey::MatchingProgram(program_id), &program);

        let mut programs: Vec<u64> = env
            .storage()
            .persistent()
            .get(&DataKey::CreatorMatchingPrograms(creator.clone()))
            .unwrap_or_else(|| Vec::new(&env));
        programs.push_back(program_id);
        env.storage()
            .persistent()
            .set(&DataKey::CreatorMatchingPrograms(creator.clone()), &programs);

        env.events().publish(
            (symbol_short!("match_new"), creator, token),
            (sponsor, program_id, match_ratio, max_match_amount),
        );

        program_id
    }

    /// Send a tip that is automatically matched by the first active matching
    /// program for this creator/token pair (if any budget remains).
    ///
    /// Returns `(tip_id, matched_amount)`.
    pub fn tip_with_match(
        env: Env,
        sender: Address,
        creator: Address,
        token: Address,
        amount: i128,
    ) -> (u64, i128) {
        if Self::is_paused(&env) {
            panic!("Contract is paused");
        }
        if amount <= 0 {
            panic_with_error!(&env, TipJarError::InvalidAmount);
        }
        if !Self::is_whitelisted(env.clone(), token.clone()) {
            panic_with_error!(&env, TipJarError::TokenNotWhitelisted);
        }

        sender.require_auth();

        let token_client = token::Client::new(&env, &token);
        token_client.transfer(&sender, &env.current_contract_address(), &amount);

        // Find the first active matching program with remaining budget.
        let mut matched_amount: i128 = 0;
        let programs: Vec<u64> = env
            .storage()
            .persistent()
            .get(&DataKey::CreatorMatchingPrograms(creator.clone()))
            .unwrap_or_else(|| Vec::new(&env));

        for program_id in programs.iter() {
            let key = DataKey::MatchingProgram(program_id);
            let mut program: MatchingProgram = match env.storage().persistent().get(&key) {
                Some(p) => p,
                None => continue,
            };

            if !program.active || program.token != token {
                continue;
            }

            let remaining = program.max_match_amount - program.current_matched;
            if remaining <= 0 {
                continue;
            }

            // match_amount = tip * ratio / 100, capped at remaining budget.
            let raw_match = amount
                .checked_mul(program.match_ratio as i128)
                .unwrap_or(i128::MAX)
                / 100;
            matched_amount = if raw_match > remaining { remaining } else { raw_match };

            program.current_matched += matched_amount;
            if program.current_matched >= program.max_match_amount {
                program.active = false;
            }
            env.storage().persistent().set(&key, &program);
            break;
        }

        let total_credit = amount + matched_amount;
        let balance_key = DataKey::CreatorBalance(creator.clone(), token.clone());
        let total_key = DataKey::CreatorTotal(creator.clone(), token.clone());
        let new_balance: i128 = env.storage().persistent().get(&balance_key).unwrap_or(0) + total_credit;
        let new_total: i128 = env.storage().persistent().get(&total_key).unwrap_or(0) + total_credit;
        env.storage().persistent().set(&balance_key, &new_balance);
        env.storage().persistent().set(&total_key, &new_total);

        let tip_id = Self::next_tip_id(&env);
        let record = TipRecord {
            id: tip_id,
            sender: sender.clone(),
            creator: creator.clone(),
            token: token.clone(),
            amount,
            timestamp: env.ledger().timestamp(),
            refunded: false,
            refund_requested: false,
        };
        env.storage()
            .persistent()
            .set(&DataKey::TipRecord(tip_id), &record);

        env.events().publish(
            (symbol_short!("tip_match"), creator.clone(), token.clone()),
            (sender, tip_id, amount, matched_amount),
        );

        (tip_id, matched_amount)
    }

    /// Sponsor cancels their matching program and reclaims unspent budget.
    pub fn cancel_matching_program(env: Env, sponsor: Address, program_id: u64) {
        if Self::is_paused(&env) {
            panic!("Contract is paused");
        }
        sponsor.require_auth();

        let key = DataKey::MatchingProgram(program_id);
        let mut program: MatchingProgram = env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or_else(|| panic_with_error!(&env, TipJarError::MatchingProgramNotFound));

        if program.sponsor != sponsor {
            panic_with_error!(&env, TipJarError::Unauthorized);
        }
        if !program.active {
            panic_with_error!(&env, TipJarError::MatchingProgramInactive);
        }

        let unspent = program.max_match_amount - program.current_matched;
        program.active = false;
        env.storage().persistent().set(&key, &program);

        if unspent > 0 {
            let token_client = token::Client::new(&env, &program.token);
            token_client.transfer(&env.current_contract_address(), &sponsor, &unspent);
        }

        env.events().publish(
            (symbol_short!("match_end"), program.creator, program.token),
            (sponsor, program_id, unspent),
        );
    }

    /// Returns a matching program by ID.
    pub fn get_matching_program(env: Env, program_id: u64) -> MatchingProgram {
        env.storage()
            .persistent()
            .get(&DataKey::MatchingProgram(program_id))
            .unwrap_or_else(|| panic_with_error!(&env, TipJarError::MatchingProgramNotFound))
    }

    // ── Internal helpers ────────────────────────────────────────────────────

    fn is_paused(env: &Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false)
    }

    /// Increments and returns the next tip ID.
    fn next_tip_id(env: &Env) -> u64 {
        let id: u64 = env.storage().instance().get(&DataKey::TipCounter).unwrap_or(0);
        let next = id + 1;
        env.storage().instance().set(&DataKey::TipCounter, &next);
        next
    }

    /// Increments and returns the next matching program ID.
    fn next_matching_id(env: &Env) -> u64 {
        let id: u64 = env.storage().instance().get(&DataKey::MatchingCounter).unwrap_or(0);
        let next = id + 1;
        env.storage().instance().set(&DataKey::MatchingCounter, &next);
        next
    }

    /// Deducts the tip amount from the creator's balance and transfers it back
    /// to the sender. Marks the record as refunded.
    fn execute_refund(env: &Env, record: &mut TipRecord) {
        let balance_key = DataKey::CreatorBalance(record.creator.clone(), record.token.clone());
        let balance: i128 = env.storage().persistent().get(&balance_key).unwrap_or(0);
        let new_balance = if balance >= record.amount { balance - record.amount } else { 0 };
        env.storage().persistent().set(&balance_key, &new_balance);

        let token_client = token::Client::new(env, &record.token);
        token_client.transfer(&env.current_contract_address(), &record.sender, &record.amount);

        record.refunded = true;
        record.refund_requested = false;
        env.storage().persistent().set(&DataKey::TipRecord(record.id), record);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::{testutils::Address as _, testutils::Ledger as _, token, Address, Env};

    fn setup() -> (Env, Address, Address, Address, Address) {
        let env = Env::default();
        env.mock_all_auths();

        let token_admin = Address::generate(&env);
        let token_id_1 = env
            .register_stellar_asset_contract_v2(token_admin.clone())
            .address();
        let token_id_2 = env
            .register_stellar_asset_contract_v2(token_admin.clone())
            .address();

        let admin = Address::generate(&env);
        let contract_id = env.register(TipJarContract, ());
        let tipjar_client = TipJarContractClient::new(&env, &contract_id);
        tipjar_client.init(&admin);
        tipjar_client.add_token(&admin, &token_id_1);

        (env, contract_id, token_id_1, token_id_2, admin)
    }

    #[test]
    fn test_tipping_functionality_multi_token() {
        let (env, contract_id, token_id_1, token_id_2, admin) = setup();
        let tipjar_client = TipJarContractClient::new(&env, &contract_id);
        let token_client_1 = token::Client::new(&env, &token_id_1);
        let token_admin_client_1 = token::StellarAssetClient::new(&env, &token_id_1);
        let token_admin_client_2 = token::StellarAssetClient::new(&env, &token_id_2);

        let sender = Address::generate(&env);
        let creator = Address::generate(&env);

        token_admin_client_1.mint(&sender, &1_000);
        token_admin_client_2.mint(&sender, &1_000);

        // Success for whitelisted token 1
        tipjar_client.tip(&sender, &creator, &token_id_1, &250);
        assert_eq!(token_client_1.balance(&sender), 750);
        assert_eq!(token_client_1.balance(&contract_id), 250);
        assert_eq!(tipjar_client.get_total_tips(&creator, &token_id_1), 250);

        // Failure for non-whitelisted token 2
        let result = tipjar_client.try_tip(&sender, &creator, &token_id_2, &100);
        assert!(result.is_err());

        // Success after whitelisting token 2
        tipjar_client.add_token(&admin, &token_id_2);
        tipjar_client.tip(&sender, &creator, &token_id_2, &300);
        assert_eq!(tipjar_client.get_total_tips(&creator, &token_id_2), 300);
    }

    #[test]
    fn test_balance_tracking_and_withdraw() {
        let (env, contract_id, token_id, _, _) = setup();
        let tipjar_client = TipJarContractClient::new(&env, &contract_id);
        let token_client = token::Client::new(&env, &token_id);
        let token_admin_client = token::StellarAssetClient::new(&env, &token_id);
        let sender_a = Address::generate(&env);
        let sender_b = Address::generate(&env);
        let creator = Address::generate(&env);

        token_admin_client.mint(&sender_a, &1_000);
        token_admin_client.mint(&sender_b, &1_000);

        tipjar_client.tip(&sender_a, &creator, &token_id, &100);
        tipjar_client.tip(&sender_b, &creator, &token_id, &300);

        assert_eq!(tipjar_client.get_total_tips(&creator, &token_id), 400);
        assert_eq!(tipjar_client.get_withdrawable_balance(&creator, &token_id), 400);

        tipjar_client.withdraw(&creator, &token_id);

        assert_eq!(tipjar_client.get_withdrawable_balance(&creator, &token_id), 0);
        assert_eq!(token_client.balance(&creator), 400);
    }

    #[test]
    #[should_panic]
    fn test_invalid_tip_amount() {
        let (env, contract_id, token_id_1, _, _) = setup();
        let tipjar_client = TipJarContractClient::new(&env, &contract_id);
        let token_admin_client = token::StellarAssetClient::new(&env, &token_id_1);
        let sender = Address::generate(&env);
        let creator = Address::generate(&env);

        token_admin_client.mint(&sender, &100);
        tipjar_client.tip(&sender, &creator, &token_id_1, &0);
    }

    #[test]
    fn test_pause_unpause() {
        let (env, contract_id, token_id_1, _, admin) = setup();
        let tipjar_client = TipJarContractClient::new(&env, &contract_id);

        tipjar_client.pause(&admin);

        let sender = Address::generate(&env);
        let creator = Address::generate(&env);

        let result = tipjar_client.try_tip(&sender, &creator, &token_id_1, &100);
        assert!(result.is_err());

        tipjar_client.unpause(&admin);

        let token_admin_client = token::StellarAssetClient::new(&env, &token_id_1);
        token_admin_client.mint(&sender, &100);
        tipjar_client.tip(&sender, &creator, &token_id_1, &100);
        assert_eq!(tipjar_client.get_total_tips(&creator, &token_id_1), 100);
    }

    #[test]
    #[should_panic]
    fn test_pause_admin_only() {
        let (env, contract_id, _, _, _) = setup();
        let tipjar_client = TipJarContractClient::new(&env, &contract_id);
        let non_admin = Address::generate(&env);

        tipjar_client.pause(&non_admin);
    }

    // ── Refund tests ─────────────────────────────────────────────────────────

    #[test]
    fn test_refund_within_grace_period() {
        let (env, contract_id, token_id, _, _) = setup();
        let tipjar_client = TipJarContractClient::new(&env, &contract_id);
        let token_admin_client = token::StellarAssetClient::new(&env, &token_id);
        let token_client = token::Client::new(&env, &token_id);
        let sender = Address::generate(&env);
        let creator = Address::generate(&env);

        token_admin_client.mint(&sender, &500);

        // Set ledger timestamp to a known value.
        env.ledger().set_timestamp(1_000);
        let tip_id = tipjar_client.tip(&sender, &creator, &token_id, &200);

        assert_eq!(tipjar_client.get_withdrawable_balance(&creator, &token_id), 200);

        // Still within 24 h grace period.
        env.ledger().set_timestamp(1_000 + GRACE_PERIOD_SECS - 1);
        tipjar_client.request_refund(&sender, &tip_id);

        // Creator balance reduced, sender got money back.
        assert_eq!(tipjar_client.get_withdrawable_balance(&creator, &token_id), 0);
        assert_eq!(token_client.balance(&sender), 500); // full balance restored

        // Record marked as refunded.
        let record = tipjar_client.get_tip_record(&tip_id);
        assert!(record.refunded);
        assert!(!record.refund_requested);
    }

    #[test]
    fn test_refund_after_grace_period_requires_approval() {
        let (env, contract_id, token_id, _, _) = setup();
        let tipjar_client = TipJarContractClient::new(&env, &contract_id);
        let token_admin_client = token::StellarAssetClient::new(&env, &token_id);
        let token_client = token::Client::new(&env, &token_id);
        let sender = Address::generate(&env);
        let creator = Address::generate(&env);

        token_admin_client.mint(&sender, &500);

        env.ledger().set_timestamp(1_000);
        let tip_id = tipjar_client.tip(&sender, &creator, &token_id, &200);

        // Past grace period.
        env.ledger().set_timestamp(1_000 + GRACE_PERIOD_SECS + 1);
        tipjar_client.request_refund(&sender, &tip_id);

        // Funds still held; refund_requested flag set.
        assert_eq!(tipjar_client.get_withdrawable_balance(&creator, &token_id), 200);
        let record = tipjar_client.get_tip_record(&tip_id);
        assert!(!record.refunded);
        assert!(record.refund_requested);

        // Creator approves.
        tipjar_client.approve_refund(&creator, &tip_id);

        assert_eq!(tipjar_client.get_withdrawable_balance(&creator, &token_id), 0);
        assert_eq!(token_client.balance(&sender), 500);

        let record = tipjar_client.get_tip_record(&tip_id);
        assert!(record.refunded);
        assert!(!record.refund_requested);
    }

    #[test]
    #[should_panic]
    fn test_double_refund_prevented() {
        let (env, contract_id, token_id, _, _) = setup();
        let tipjar_client = TipJarContractClient::new(&env, &contract_id);
        let token_admin_client = token::StellarAssetClient::new(&env, &token_id);
        let sender = Address::generate(&env);
        let creator = Address::generate(&env);

        token_admin_client.mint(&sender, &500);

        env.ledger().set_timestamp(1_000);
        let tip_id = tipjar_client.tip(&sender, &creator, &token_id, &200);

        // First refund within grace period — succeeds.
        tipjar_client.request_refund(&sender, &tip_id);

        // Second attempt — should panic with AlreadyRefunded.
        tipjar_client.request_refund(&sender, &tip_id);
    }

    #[test]
    #[should_panic]
    fn test_approve_refund_without_request_fails() {
        let (env, contract_id, token_id, _, _) = setup();
        let tipjar_client = TipJarContractClient::new(&env, &contract_id);
        let token_admin_client = token::StellarAssetClient::new(&env, &token_id);
        let sender = Address::generate(&env);
        let creator = Address::generate(&env);

        token_admin_client.mint(&sender, &500);

        env.ledger().set_timestamp(1_000);
        let tip_id = tipjar_client.tip(&sender, &creator, &token_id, &200);

        // No request was made — creator cannot approve.
        tipjar_client.approve_refund(&creator, &tip_id);
    }

    #[test]
    #[should_panic]
    fn test_wrong_sender_cannot_request_refund() {
        let (env, contract_id, token_id, _, _) = setup();
        let tipjar_client = TipJarContractClient::new(&env, &contract_id);
        let token_admin_client = token::StellarAssetClient::new(&env, &token_id);
        let sender = Address::generate(&env);
        let attacker = Address::generate(&env);
        let creator = Address::generate(&env);

        token_admin_client.mint(&sender, &500);

        env.ledger().set_timestamp(1_000);
        let tip_id = tipjar_client.tip(&sender, &creator, &token_id, &200);

        // Different address tries to refund.
        tipjar_client.request_refund(&attacker, &tip_id);
    }

    #[test]
    fn test_refund_balance_updates_correctly_with_multiple_tips() {
        let (env, contract_id, token_id, _, _) = setup();
        let tipjar_client = TipJarContractClient::new(&env, &contract_id);
        let token_admin_client = token::StellarAssetClient::new(&env, &token_id);
        let token_client = token::Client::new(&env, &token_id);
        let sender = Address::generate(&env);
        let creator = Address::generate(&env);

        token_admin_client.mint(&sender, &1_000);

        env.ledger().set_timestamp(1_000);
        let tip_id_1 = tipjar_client.tip(&sender, &creator, &token_id, &300);
        let tip_id_2 = tipjar_client.tip(&sender, &creator, &token_id, &200);

        assert_eq!(tipjar_client.get_withdrawable_balance(&creator, &token_id), 500);

        // Refund only the first tip within grace period.
        tipjar_client.request_refund(&sender, &tip_id_1);

        assert_eq!(tipjar_client.get_withdrawable_balance(&creator, &token_id), 200);
        assert_eq!(token_client.balance(&sender), 800); // 1000 - 500 + 300

        // Second tip still intact.
        let record2 = tipjar_client.get_tip_record(&tip_id_2);
        assert!(!record2.refunded);
    }

    // ── Matching program tests ────────────────────────────────────────────────

    #[test]
    fn test_match_calculation_1_to_1() {
        let (env, contract_id, token_id, _, _) = setup();
        let tipjar_client = TipJarContractClient::new(&env, &contract_id);
        let token_admin_client = token::StellarAssetClient::new(&env, &token_id);
        let token_client = token::Client::new(&env, &token_id);
        let sponsor = Address::generate(&env);
        let sender = Address::generate(&env);
        let creator = Address::generate(&env);

        token_admin_client.mint(&sponsor, &1_000);
        token_admin_client.mint(&sender, &500);

        // 1:1 match, budget = 500
        let program_id = tipjar_client.create_matching_program(
            &sponsor, &creator, &token_id, &100u32, &500i128,
        );
        assert_eq!(token_client.balance(&sponsor), 500); // 500 deposited

        let (_, matched) = tipjar_client.tip_with_match(&sender, &creator, &token_id, &200);

        assert_eq!(matched, 200); // 1:1
        assert_eq!(tipjar_client.get_withdrawable_balance(&creator, &token_id), 400);
        assert_eq!(tipjar_client.get_total_tips(&creator, &token_id), 400);

        let program = tipjar_client.get_matching_program(&program_id);
        assert_eq!(program.current_matched, 200);
        assert!(program.active);
    }

    #[test]
    fn test_match_calculation_2_to_1() {
        let (env, contract_id, token_id, _, _) = setup();
        let tipjar_client = TipJarContractClient::new(&env, &contract_id);
        let token_admin_client = token::StellarAssetClient::new(&env, &token_id);
        let sponsor = Address::generate(&env);
        let sender = Address::generate(&env);
        let creator = Address::generate(&env);

        token_admin_client.mint(&sponsor, &1_000);
        token_admin_client.mint(&sender, &500);

        // 2:1 match (ratio=200), budget = 1000
        tipjar_client.create_matching_program(
            &sponsor, &creator, &token_id, &200u32, &1_000i128,
        );

        let (_, matched) = tipjar_client.tip_with_match(&sender, &creator, &token_id, &100);

        assert_eq!(matched, 200); // 2:1
        assert_eq!(tipjar_client.get_withdrawable_balance(&creator, &token_id), 300);
    }

    #[test]
    fn test_match_limit_caps_at_budget() {
        let (env, contract_id, token_id, _, _) = setup();
        let tipjar_client = TipJarContractClient::new(&env, &contract_id);
        let token_admin_client = token::StellarAssetClient::new(&env, &token_id);
        let sponsor = Address::generate(&env);
        let sender = Address::generate(&env);
        let creator = Address::generate(&env);

        token_admin_client.mint(&sponsor, &150);
        token_admin_client.mint(&sender, &1_000);

        // 1:1 match but only 150 budget
        tipjar_client.create_matching_program(
            &sponsor, &creator, &token_id, &100u32, &150i128,
        );

        // Tip of 200 — match capped at 150
        let (_, matched) = tipjar_client.tip_with_match(&sender, &creator, &token_id, &200);
        assert_eq!(matched, 150);
        assert_eq!(tipjar_client.get_withdrawable_balance(&creator, &token_id), 350);
    }

    #[test]
    fn test_program_exhaustion_deactivates() {
        let (env, contract_id, token_id, _, _) = setup();
        let tipjar_client = TipJarContractClient::new(&env, &contract_id);
        let token_admin_client = token::StellarAssetClient::new(&env, &token_id);
        let sponsor = Address::generate(&env);
        let sender = Address::generate(&env);
        let creator = Address::generate(&env);

        token_admin_client.mint(&sponsor, &100);
        token_admin_client.mint(&sender, &1_000);

        let program_id = tipjar_client.create_matching_program(
            &sponsor, &creator, &token_id, &100u32, &100i128,
        );

        // Exhaust the budget in one tip
        let (_, matched) = tipjar_client.tip_with_match(&sender, &creator, &token_id, &100);
        assert_eq!(matched, 100);

        let program = tipjar_client.get_matching_program(&program_id);
        assert!(!program.active); // exhausted

        // Next tip gets no match
        let (_, matched2) = tipjar_client.tip_with_match(&sender, &creator, &token_id, &100);
        assert_eq!(matched2, 0);
    }

    #[test]
    fn test_cancel_matching_program_returns_unspent() {
        let (env, contract_id, token_id, _, _) = setup();
        let tipjar_client = TipJarContractClient::new(&env, &contract_id);
        let token_admin_client = token::StellarAssetClient::new(&env, &token_id);
        let token_client = token::Client::new(&env, &token_id);
        let sponsor = Address::generate(&env);
        let sender = Address::generate(&env);
        let creator = Address::generate(&env);

        token_admin_client.mint(&sponsor, &500);
        token_admin_client.mint(&sender, &200);

        let program_id = tipjar_client.create_matching_program(
            &sponsor, &creator, &token_id, &100u32, &500i128,
        );

        // Use 100 of the budget
        tipjar_client.tip_with_match(&sender, &creator, &token_id, &100);

        // Cancel — should return 400 unspent
        tipjar_client.cancel_matching_program(&sponsor, &program_id);
        assert_eq!(token_client.balance(&sponsor), 400);

        let program = tipjar_client.get_matching_program(&program_id);
        assert!(!program.active);
    }

    #[test]
    fn test_multiple_concurrent_matching_programs() {
        let (env, contract_id, token_id, _, _) = setup();
        let tipjar_client = TipJarContractClient::new(&env, &contract_id);
        let token_admin_client = token::StellarAssetClient::new(&env, &token_id);
        let sponsor1 = Address::generate(&env);
        let sponsor2 = Address::generate(&env);
        let sender = Address::generate(&env);
        let creator = Address::generate(&env);

        token_admin_client.mint(&sponsor1, &200);
        token_admin_client.mint(&sponsor2, &300);
        token_admin_client.mint(&sender, &1_000);

        // Two programs — first one (sponsor1) used first
        tipjar_client.create_matching_program(&sponsor1, &creator, &token_id, &100u32, &200i128);
        tipjar_client.create_matching_program(&sponsor2, &creator, &token_id, &100u32, &300i128);

        // Tip 300 — sponsor1 covers up to 200 (its full budget)
        let (_, matched) = tipjar_client.tip_with_match(&sender, &creator, &token_id, &300);
        assert_eq!(matched, 200); // sponsor1 exhausted

        // Next tip picks up sponsor2
        let (_, matched2) = tipjar_client.tip_with_match(&sender, &creator, &token_id, &100);
        assert_eq!(matched2, 100);
    }
}
