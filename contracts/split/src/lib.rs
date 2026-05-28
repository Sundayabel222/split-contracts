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

use soroban_sdk::{contract, contractimpl, symbol_short, token, Address, BytesN, Env, Symbol, Vec};
use types::{Invoice, InvoiceStatus, Payment};

// ---------------------------------------------------------------------------
// Storage helpers
// ---------------------------------------------------------------------------

/// Storage key for the auto-incrementing invoice counter.
fn counter_key() -> Symbol {
    symbol_short!("counter")
}

/// Storage key for the contract admin address.
fn admin_key() -> Symbol {
    symbol_short!("admin")
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

// ---------------------------------------------------------------------------
// Contract
// ---------------------------------------------------------------------------

#[contract]
pub struct SplitContract;

#[contractimpl]
impl SplitContract {
    /// Set the contract admin. Can only be called once.
    pub fn initialize(env: Env, admin: Address) {
        assert!(
            !env.storage().instance().has(&admin_key()),
            "already initialized"
        );
        env.storage().instance().set(&admin_key(), &admin);
    }

    /// Upgrade the contract WASM. Requires admin auth.
    pub fn upgrade(env: Env, new_wasm_hash: BytesN<32>) {
        let admin: Address = env
            .storage()
            .instance()
            .get(&admin_key())
            .expect("not initialized");
        admin.require_auth();
        env.deployer().update_current_contract_wasm(new_wasm_hash);
    }

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

        // Increment and persist the invoice counter.
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
            payments: Vec::new(&env),
        };

        save_invoice(&env, id, &invoice);
        events::invoice_created(&env, id, &creator, total);

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

        events::payment_received(&env, invoice_id, &payer, amount);

        // Auto-release if fully funded.
        if invoice.funded >= total {
            Self::_release(&env, invoice_id, &mut invoice);
        } else {
            save_invoice(&env, invoice_id, &invoice);
        }
    }

    /// Release funds to all recipients once the invoice is fully funded.
    ///
    /// Can be called by anyone; validates full funding internally.
    pub fn release(env: Env, invoice_id: u64) {
        let mut invoice = load_invoice(&env, invoice_id);

        assert!(
            invoice.status == InvoiceStatus::Pending,
            "invoice is not pending"
        );

        let total: i128 = invoice.amounts.iter().sum();
        assert!(invoice.funded >= total, "invoice not fully funded");

        Self::_release(&env, invoice_id, &mut invoice);
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
        events::invoice_refunded(&env, invoice_id);
    }

    /// Retrieve an invoice by ID.
    pub fn get_invoice(env: Env, invoice_id: u64) -> Invoice {
        load_invoice(&env, invoice_id)
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    /// Route funds to all recipients and mark the invoice as released.
    fn _release(env: &Env, invoice_id: u64, invoice: &mut Invoice) {
        let token_client = token::Client::new(env, &invoice.token);

        for (recipient, amount) in invoice.recipients.iter().zip(invoice.amounts.iter()) {
            token_client.transfer(&env.current_contract_address(), &recipient, &amount);
        }

        invoice.status = InvoiceStatus::Released;
        save_invoice(env, invoice_id, invoice);
        events::invoice_released(env, invoice_id, &invoice.recipients);
    }
}
