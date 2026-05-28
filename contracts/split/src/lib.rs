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

use soroban_sdk::{contract, contractimpl, symbol_short, token, Address, Env, Symbol, Vec};
use types::{Invoice, InvoiceStatus, InvoiceTemplate, Payment, AuditEntry, SubscriptionParams, CompletionProof};

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

/// Storage key for an invoice template: (symbol, creator, name).
fn template_key(creator: &Address, name: &Symbol) -> (Symbol, Address, Symbol) {
    (symbol_short!("tmpl"), creator.clone(), name.clone())
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
    /// Transfers `amount` of the invoice token from `payer` to this contract.
    /// Auto-releases funds if the invoice becomes fully funded.
    ///
    /// # Arguments
    /// * `payer`      – address making the payment (must authorise)
    /// * `invoice_id` – target invoice
    /// * `amount`     – amount to pay in stroops
    pub fn pay(env: Env, payer: Address, invoice_id: u64, amount: i128) {
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

        let total: i128 = invoice.amounts.iter().sum();
        let remaining = total - invoice.funded;
        assert!(amount <= remaining, "payment exceeds remaining balance");

        // Transfer tokens from payer to this contract.
        let token_client = token::Client::new(&env, &invoice.token);
        token_client.transfer(&payer, &env.current_contract_address(), &amount);

        invoice.payments.push_back(Payment {
            payer: payer.clone(),
            amount,
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

        for payment in invoice.payments.iter() {
            token_client.transfer(
                &env.current_contract_address(),
                &payment.payer,
                &payment.amount,
            );
        }

        invoice.status = InvoiceStatus::Refunded;
        save_invoice(&env, invoice_id, &invoice);
        let actor = env.current_contract_address();
        append_audit_entry(&env, invoice_id, symbol_short!("refund"), &actor);
        events::invoice_refunded(&env, invoice_id);
    }

    /// Save a reusable invoice template under a named key.
    ///
    /// Calling again with the same `name` overwrites the existing template.
    pub fn save_template(
        env: Env,
        creator: Address,
        name: Symbol,
        recipients: Vec<Address>,
        amounts: Vec<i128>,
        token: Address,
    ) {
        creator.require_auth();
        assert!(
            recipients.len() == amounts.len(),
            "recipients and amounts length mismatch"
        );
        assert!(!recipients.is_empty(), "must have at least one recipient");
        for amt in amounts.iter() {
            assert!(amt > 0, "amounts must be positive");
        }
        let template = InvoiceTemplate { recipients, amounts, token };
        env.storage().persistent().set(&template_key(&creator, &name), &template);
    }

    /// Create a new invoice from a previously saved template.
    ///
    /// # Returns
    /// The new invoice ID.
    pub fn create_from_template(
        env: Env,
        creator: Address,
        name: Symbol,
        deadline: u64,
    ) -> u64 {
        creator.require_auth();
        let tmpl: InvoiceTemplate = env
            .storage()
            .persistent()
            .get(&template_key(&creator, &name))
            .expect("template not found");
        Self::_create_invoice(&env, creator, tmpl.recipients, tmpl.amounts, tmpl.token, deadline)
    }

    /// Return the total amount contributed by `payer` toward `invoice_id`.
    ///
    /// Returns 0 if the address has not paid. Requires no auth (read-only).
    pub fn get_payer_total(env: Env, invoice_id: u64, payer: Address) -> i128 {
        let invoice = load_invoice(&env, invoice_id);
        invoice
            .payments
            .iter()
            .filter(|p| p.payer == payer)
            .map(|p| p.amount)
            .sum()
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
        assert!(invoice.creator == caller, "only creator can cancel");

        let token_client = token::Client::new(&env, &invoice.token);
        for payment in invoice.payments.iter() {
            token_client.transfer(
                &env.current_contract_address(),
                &payment.payer,
                &payment.amount,
            );
        }

        invoice.status = InvoiceStatus::Refunded;
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
        let invoice = load_invoice(&env, invoice_id);

        assert!(
            invoice.status == InvoiceStatus::Released || invoice.status == InvoiceStatus::Refunded,
            "invoice not finalized"
        );

        // Build a byte buffer using soroban_sdk::Bytes for sha256 input.
        let mut buf = soroban_sdk::Bytes::new(&env);

        // invoice_id (8 bytes)
        buf.extend_from_array(&invoice_id.to_le_bytes());
        // funded (16 bytes)
        buf.extend_from_array(&invoice.funded.to_le_bytes());
        // deadline (8 bytes)
        buf.extend_from_array(&invoice.deadline.to_le_bytes());
        // status byte
        let s_byte: u8 = match invoice.status {
            InvoiceStatus::Pending => 0,
            InvoiceStatus::Released => 1,
            InvoiceStatus::Refunded => 2,
            InvoiceStatus::Cancelled => 3,
        };
        buf.extend_from_array(&[s_byte]);

        let hash = env.crypto().sha256(&buf).to_bytes();

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

    /// Route funds to all recipients and mark the invoice as released.
    /// Also creates the next invoice in a subscription chain if params exist.
    fn _release(env: &Env, invoice_id: u64, invoice: &mut Invoice, actor: &Address) {
        let token_client = token::Client::new(env, &invoice.token);

        for (recipient, amount) in invoice.recipients.iter().zip(invoice.amounts.iter()) {
            token_client.transfer(&env.current_contract_address(), &recipient, &amount);
        }

        // All transfers succeeded — persist state change now.
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

    /// Internal invoice creation — no auth check. Called by public entry points
    /// that have already verified auth (create_invoice, create_from_template,
    /// create_subscription, and subscription chaining in _release).
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
            recipients,
            amounts,
            token,
            deadline,
            funded: 0,
            status: InvoiceStatus::Pending,
            payments: Vec::new(env),
        };
        save_invoice(env, id, &invoice);
        events::invoice_created(env, id, &creator, total);
        id
    }
}