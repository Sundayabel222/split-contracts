//! StellarSplit — on-chain invoice & payment splitting contract.
//!
//! Allows a creator to define an invoice with multiple recipients and amounts.
//! Payers contribute funds; once fully funded the contract auto-routes USDC to
//! each recipient. If the deadline passes unfunded, payers are refunded.

#![no_std]

mod events;
mod types;

#[cfg(test)]
mod test;

use soroban_sdk::{contract, contractimpl, symbol_short, token, Address, Env, Map, Symbol, Vec};
use types::{Invoice, InvoiceStatus, Payment, AuditEntry, SubscriptionParams, CompletionProof};

// ---------------------------------------------------------------------------
// Storage helpers
// ---------------------------------------------------------------------------

/// Storage key for the auto-incrementing invoice counter.
fn counter_key() -> Symbol {
    symbol_short!("counter")
}

/// Composite storage key for an invoice: (symbol, id).
fn invoice_key(id: u64) -> (Symbol, u64) {
    (symbol_short!("inv"), id)
}

fn load_invoice(env: &Env, id: u64) -> Invoice {
    env.storage()
        .persistent()
        .get(&invoice_key(id))
        .expect("invoice not found")
}

fn save_invoice(env: &Env, id: u64, invoice: &Invoice) {
    env.storage()
        .persistent()
        .set(&invoice_key(id), invoice);
}

/// Storage key for the audit log: (symbol, invoice_id).
fn audit_log_key(id: u64) -> (Symbol, u64) {
    (symbol_short!("log"), id)
}

/// Storage key for subscription params: (symbol, parent_invoice_id).
fn subscription_params_key(id: u64) -> (Symbol, u64) {
    (symbol_short!("sub"), id)
}

/// Storage key for the recipient invoice index: (symbol, recipient).
fn recip_idx_key(recipient: &Address) -> (Symbol, Address) {
    (symbol_short!("recip_idx"), recipient.clone())
}

/// Append an audit entry to the log for an invoice.
fn append_audit_entry(env: &Env, id: u64, action: Symbol, actor: &Address) {
    let timestamp = env.ledger().timestamp();
    let entry = AuditEntry {
        action,
        actor: actor.clone(),
        timestamp,
    };

    // Try to load existing log, create new one if not present
    let mut log: Vec<AuditEntry> = env
        .storage()
        .persistent()
        .get(&audit_log_key(id))
        .unwrap_or_else(|| Vec::new(env));

    log.push_back(entry);
    env.storage().persistent().set(&audit_log_key(id), &log);
}

/// Retrieve the audit log for an invoice.
pub fn get_audit_log(env: &Env, id: u64) -> Vec<AuditEntry> {
    env.storage()
        .persistent()
        .get(&audit_log_key(id))
        .unwrap_or_else(|| Vec::new(env))
}

// ---------------------------------------------------------------------------
// Contract
// ---------------------------------------------------------------------------

#[contract]
pub struct SplitContract;

#[contractimpl]
impl SplitContract {
    /// Create a new invoice.
    ///
    /// # Arguments
    /// * `creator`    – address that owns the invoice (must authorise)
    /// * `recipients` – ordered list of recipient addresses
    /// * `amounts`    – amount owed to each recipient (parallel to `recipients`)
    /// * `token`      – USDC token contract address
    /// * `deadline`   – Unix timestamp; after this refunds become available
    ///
    /// # Returns
    /// The new invoice ID (monotonically increasing u64).
    pub fn create_invoice(
        env: Env,
        creator: Address,
        recipients: Vec<Address>,
        amounts: Vec<i128>,
        token: Address,
        deadline: u64,
    ) -> u64 {
        creator.require_auth();
        Self::_create_invoice(&env, creator, recipients, amounts, token, deadline)
    }

    /// Create a subscription chain of invoices for recurring monthly billing.
    ///
    /// Creates the first invoice immediately and schedules subsequent invoices
    /// to be created automatically on each release.
    ///
    /// # Arguments
    /// * `creator`    – address that owns the subscription (must authorise)
    /// * `recipients` – ordered list of recipient addresses
    /// * `amounts`    – amount owed to each recipient (parallel to `recipients`)
    /// * `token`      – USDC token contract address
    /// * `months`     – number of months (capped at 12)
    ///
    /// # Returns
    /// The ID of the first invoice created.
    pub fn create_subscription(
        env: Env,
        creator: Address,
        recipients: Vec<Address>,
        amounts: Vec<i128>,
        token: Address,
        months: u32,
    ) -> u64 {
        creator.require_auth();

        assert!(
            recipients.len() == amounts.len(),
            "recipients and amounts length mismatch"
        );
        assert!(!recipients.is_empty(), "must have at least one recipient");
        assert!(months > 0 && months <= 12, "months must be between 1 and 12");

        for amt in amounts.iter() {
            assert!(amt > 0, "amounts must be positive");
        }

        // Create first invoice with deadline 30 days in future (in seconds)
        let deadline = env.ledger().timestamp() + 30 * 24 * 60 * 60;
        let id = Self::_create_invoice(
            &env,
            creator.clone(),
            recipients.clone(),
            amounts.clone(),
            token.clone(),
            deadline,
        );

        // Store subscription params if more invoices needed
        if months > 1 {
            let params = SubscriptionParams {
                creator: creator.clone(),
                recipients: recipients.clone(),
                amounts: amounts.clone(),
                token: token.clone(),
            };
            env.storage()
                .persistent()
                .set(&subscription_params_key(id), &params);
        }

        id
    }

    /// Pay toward an invoice.
    ///
    /// Transfers `amount + tip` of the invoice token from `payer` to this contract.
    /// Auto-releases funds if the invoice becomes fully funded.
    ///
    /// # Arguments
    /// * `payer`      – address making the payment (must authorise)
    /// * `invoice_id` – target invoice
    /// * `amount`     – amount to pay in stroops
    /// * `tip`        – optional tip in stroops (0 = no tip); split equally among recipients at release
    pub fn pay(env: Env, payer: Address, invoice_id: u64, amount: i128, tip: i128) {
        payer.require_auth();

        let mut invoice = load_invoice(&env, invoice_id);

        assert!(
            invoice.status == InvoiceStatus::Pending,
            "invoice is not pending"
        );
        assert!(
            env.ledger().timestamp() <= invoice.deadline,
            "invoice deadline has passed"
        );
        assert!(amount > 0, "payment amount must be positive");
        assert!(tip >= 0, "tip must be non-negative");

        let total: i128 = invoice.amounts.iter().sum();
        let remaining = total - invoice.funded;
        assert!(amount <= remaining, "payment exceeds remaining balance");

        // Transfer tokens (amount + tip) from payer to this contract.
        let token_client = token::Client::new(&env, &invoice.token);
        token_client.transfer(&payer, &env.current_contract_address(), &(amount + tip));

        invoice.payments.push_back(Payment {
            payer: payer.clone(),
            amount,
            tip,
        });
        invoice.funded += amount;

        append_audit_entry(&env, invoice_id, symbol_short!("pay"), &payer);
        events::payment_received(&env, invoice_id, &payer, amount);

        // Auto-release if fully funded.
        if invoice.funded >= total {
            let creator = invoice.creator.clone();
            Self::_release(&env, invoice_id, &mut invoice, &creator);
        } else {
            save_invoice(&env, invoice_id, &invoice);
        }
    }

    /// Release funds to all recipients once the invoice is fully funded.
    ///
    /// Can be called by anyone; validates full funding internally.
    pub fn release(env: Env, invoice_id: u64) {
        let caller = env.current_contract_address();
        let mut invoice = load_invoice(&env, invoice_id);

        assert!(
            invoice.status == InvoiceStatus::Pending,
            "invoice is not pending"
        );

        let total: i128 = invoice.amounts.iter().sum();
        assert!(invoice.funded >= total, "invoice not fully funded");

        Self::_release(&env, invoice_id, &mut invoice, &caller);
    }

    /// Refund all payers if the deadline has passed and the invoice is not fully funded.
    ///
    /// Can be called by anyone after the deadline.
    pub fn refund(env: Env, invoice_id: u64) {
        let mut invoice = load_invoice(&env, invoice_id);

        assert!(
            invoice.status == InvoiceStatus::Pending,
            "invoice is not pending"
        );
        assert!(
            env.ledger().timestamp() > invoice.deadline,
            "deadline has not passed"
        );

        let token_client = token::Client::new(&env, &invoice.token);

        // Aggregate total owed per unique payer (amount + tip).
        let mut totals: Map<Address, i128> = Map::new(&env);
        for payment in invoice.payments.iter() {
            let prev = totals.get(payment.payer.clone()).unwrap_or(0);
            totals.set(payment.payer.clone(), prev + payment.amount + payment.tip);
        }

        // One transfer + one event per unique payer.
        for (payer, amount) in totals.iter() {
            token_client.transfer(&env.current_contract_address(), &payer, &amount);
            events::payer_refunded(&env, invoice_id, &payer, amount);
        }

        invoice.status = InvoiceStatus::Refunded;
        save_invoice(&env, invoice_id, &invoice);
        let actor = env.current_contract_address();
        append_audit_entry(&env, invoice_id, symbol_short!("refund"), &actor);
        events::invoice_refunded(&env, invoice_id);
    }

    /// Cancel an invoice before any payments are made.
    ///
    /// Only the creator can cancel, and it must be before payments start.
    ///
    /// # Arguments
    /// * `caller`     – must be the invoice creator (must authorise)
    /// * `invoice_id` – target invoice
    pub fn cancel_invoice(env: Env, caller: Address, invoice_id: u64) {
        caller.require_auth();

        let mut invoice = load_invoice(&env, invoice_id);

        assert!(
            invoice.status == InvoiceStatus::Pending,
            "invoice is not pending"
        );
        assert!(
            invoice.creator == caller,
            "only creator can cancel"
        );
        assert!(
            invoice.funded == 0,
            "cannot cancel invoice with payments"
        );

        invoice.status = InvoiceStatus::Cancelled;
        save_invoice(&env, invoice_id, &invoice);
        append_audit_entry(&env, invoice_id, symbol_short!("cancel"), &caller);
    }

    /// Extend the deadline for an invoice.
    ///
    /// Only the creator can extend, and the new deadline must be in the future.
    ///
    /// # Arguments
    /// * `caller`     – must be the invoice creator (must authorise)
    /// * `invoice_id` – target invoice
    /// * `new_deadline` – new Unix timestamp for the deadline
    pub fn extend_deadline(env: Env, caller: Address, invoice_id: u64, new_deadline: u64) {
        caller.require_auth();

        let mut invoice = load_invoice(&env, invoice_id);

        assert!(
            invoice.status == InvoiceStatus::Pending,
            "invoice is not pending"
        );
        assert!(
            invoice.creator == caller,
            "only creator can extend deadline"
        );
        assert!(
            new_deadline > env.ledger().timestamp(),
            "new deadline must be in the future"
        );

        invoice.deadline = new_deadline;
        save_invoice(&env, invoice_id, &invoice);
        append_audit_entry(&env, invoice_id, symbol_short!("extend"), &caller);
    }

    /// Clone an existing invoice with a new deadline.
    ///
    /// Copies recipients, amounts, and token from the source invoice.
    /// Only the original creator may clone.
    ///
    /// # Arguments
    /// * `creator`      – must be the source invoice's creator (must authorise)
    /// * `source_id`    – invoice to clone
    /// * `new_deadline` – deadline for the new invoice (must be in the future)
    ///
    /// # Returns
    /// The new invoice ID.
    pub fn clone_invoice(env: Env, creator: Address, source_id: u64, new_deadline: u64) -> u64 {
        creator.require_auth();

        let source = load_invoice(&env, source_id);
        assert!(source.creator == creator, "only creator can clone");
        assert!(
            new_deadline > env.ledger().timestamp(),
            "deadline must be in the future"
        );

        Self::_create_invoice(
            &env,
            creator,
            source.recipients,
            source.amounts,
            source.token,
            new_deadline,
        )
    }

    /// Return all invoice IDs where `recipient` is listed as a recipient.
    pub fn get_invoices_by_recipient(env: Env, recipient: Address) -> Vec<u64> {
        env.storage()
            .persistent()
            .get(&recip_idx_key(&recipient))
            .unwrap_or_else(|| Vec::new(&env))
    }

    /// Retrieve an invoice by ID.
    pub fn get_invoice(env: Env, invoice_id: u64) -> Invoice {
        load_invoice(&env, invoice_id)
    }

    /// Retrieve the audit log for an invoice.
    pub fn get_audit_log(env: Env, invoice_id: u64) -> Vec<AuditEntry> {
        get_audit_log(&env, invoice_id)
    }

    /// Generate a completion proof for a finalized invoice.
    ///
    /// Returns a proof containing ID, status, funded amount, timestamp,
    /// and SHA-256 hash for off-chain verification.
    ///
    /// # Arguments
    /// * `invoice_id` – target invoice
    ///
    /// # Returns
    /// CompletionProof with invoice data and hash
    pub fn get_completion_proof(env: Env, invoice_id: u64) -> CompletionProof {
        use soroban_sdk::Bytes;

        let invoice = load_invoice(&env, invoice_id);

        // Only return proof for finalized invoices
        assert!(
            invoice.status == InvoiceStatus::Released || invoice.status == InvoiceStatus::Refunded,
            "invoice not finalized"
        );

        // Build a deterministic byte payload using soroban_sdk::Bytes.
        let mut bytes = Bytes::new(&env);

        // invoice_id (8 bytes)
        bytes.extend_from_array(&invoice_id.to_le_bytes());
        // funded (16 bytes)
        bytes.extend_from_array(&invoice.funded.to_le_bytes());
        // deadline (8 bytes)
        bytes.extend_from_array(&invoice.deadline.to_le_bytes());
        // status byte
        let s_byte: u8 = match invoice.status {
            InvoiceStatus::Pending => 0,
            InvoiceStatus::Released => 1,
            InvoiceStatus::Refunded => 2,
            InvoiceStatus::Cancelled => 3,
        };
        bytes.extend_from_array(&[s_byte]);

        let hash = env.crypto().sha256(&bytes).to_bytes();

        CompletionProof {
            id: invoice_id,
            status: invoice.status,
            funded: invoice.funded,
            timestamp: env.ledger().timestamp(),
            hash,
        }
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    /// Internal invoice creation — no auth check. Called by create_invoice,
    /// clone_invoice, create_subscription, and _release (subscription chain).
    fn _create_invoice(
        env: &Env,
        creator: Address,
        recipients: Vec<Address>,
        amounts: Vec<i128>,
        token: Address,
        deadline: u64,
    ) -> u64 {
        assert!(
            recipients.len() == amounts.len(),
            "recipients and amounts length mismatch"
        );
        assert!(!recipients.is_empty(), "must have at least one recipient");
        assert!(
            deadline > env.ledger().timestamp(),
            "deadline must be in the future"
        );
        for amt in amounts.iter() {
            assert!(amt > 0, "amounts must be positive");
        }

        let id: u64 = env
            .storage()
            .persistent()
            .get(&counter_key())
            .unwrap_or(0u64)
            + 1;
        env.storage().persistent().set(&counter_key(), &id);

        let total: i128 = amounts.iter().sum();

        let invoice = Invoice {
            creator: creator.clone(),
            recipients: recipients.clone(),
            amounts,
            token,
            deadline,
            funded: 0,
            status: InvoiceStatus::Pending,
            payments: Vec::new(env),
        };

        save_invoice(env, id, &invoice);
        events::invoice_created(env, id, &creator, total);

        for recipient in recipients.iter() {
            let key = recip_idx_key(&recipient);
            let mut idx: Vec<u64> = env
                .storage()
                .persistent()
                .get(&key)
                .unwrap_or_else(|| Vec::new(env));
            idx.push_back(id);
            env.storage().persistent().set(&key, &idx);
        }

        id
    }

    /// Route funds to all recipients and mark the invoice as released.
    /// Also creates the next invoice in a subscription chain if params exist.
    fn _release(env: &Env, invoice_id: u64, invoice: &mut Invoice, actor: &Address) {
        let token_client = token::Client::new(env, &invoice.token);

        // Sum all tips and split equally among recipients.
        let total_tips: i128 = invoice.payments.iter().map(|p| p.tip).sum();
        let n = invoice.recipients.len() as i128;
        let tip_per_recipient = if n > 0 { total_tips / n } else { 0 };

        for (recipient, amount) in invoice.recipients.iter().zip(invoice.amounts.iter()) {
            token_client.transfer(
                &env.current_contract_address(),
                &recipient,
                &(amount + tip_per_recipient),
            );
        }

        invoice.status = InvoiceStatus::Released;
        save_invoice(env, invoice_id, invoice);
        append_audit_entry(env, invoice_id, symbol_short!("release"), actor);
        events::invoice_released(env, invoice_id, &invoice.recipients);

        // Check for subscription params and create next invoice if exists
        if let Some(params) = env
            .storage()
            .persistent()
            .get::<_, SubscriptionParams>(&subscription_params_key(invoice_id))
        {
            // Create next invoice with deadline 30 days after current release
            let next_deadline = env.ledger().timestamp() + 30 * 24 * 60 * 60;
            let _next_id = Self::_create_invoice(
                env,
                params.creator.clone(),
                params.recipients.clone(),
                params.amounts.clone(),
                params.token.clone(),
                next_deadline,
            );

            // Remove the params storage key (subscription complete)
            env.storage()
                .persistent()
                .remove(&subscription_params_key(invoice_id));
        }
    }
}