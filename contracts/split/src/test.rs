#![cfg(test)]

use super::*;
use soroban_sdk::{
    testutils::{Address as _, Ledger},
    token::{Client as TokenClient, StellarAssetClient},
    Address, Env, Vec,
};

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

/// Deploy the split contract and a mock USDC token; return (env, contract_id, token_id).
fn setup() -> (Env, Address, Address) {
    let env = Env::default();
    env.mock_all_auths();

    let contract_id = env.register(SplitContract, ());
    let token_admin = Address::generate(&env);
    let token_id = env.register_stellar_asset_contract_v2(token_admin.clone()).address();

    // Mint tokens to test accounts via the admin interface.
    let stellar_asset = StellarAssetClient::new(&env, &token_id);
    stellar_asset.mint(&token_admin, &1_000_000_000);

    (env, contract_id, token_id)
}

fn client<'a>(env: &'a Env, contract_id: &Address) -> SplitContractClient<'a> {
    SplitContractClient::new(env, contract_id)
}

fn token_client<'a>(env: &'a Env, token_id: &Address) -> TokenClient<'a> {
    TokenClient::new(env, token_id)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn test_create_invoice() {
    let (env, contract_id, token_id) = setup();
    let c = client(&env, &contract_id);

    let creator = Address::generate(&env);
    let recipient = Address::generate(&env);

    let mut recipients = Vec::new(&env);
    recipients.push_back(recipient.clone());
    let mut amounts = Vec::new(&env);
    amounts.push_back(100_i128);

    // Set ledger time so deadline is in the future.
    env.ledger().set_timestamp(1_000);

    let id = c.create_invoice(&creator, &recipients, &amounts, &token_id, &2_000_u64);
    assert_eq!(id, 1);

    let invoice = c.get_invoice(&id);
    assert_eq!(invoice.status, InvoiceStatus::Pending);
    assert_eq!(invoice.funded, 0);
}

#[test]
fn test_pay_and_auto_release() {
    let (env, contract_id, token_id) = setup();
    let c = client(&env, &contract_id);
    let tk = token_client(&env, &token_id);

    let creator = Address::generate(&env);
    let payer = Address::generate(&env);
    let recipient = Address::generate(&env);

    // Fund payer.
    let stellar_asset = StellarAssetClient::new(&env, &token_id);
    stellar_asset.mint(&payer, &500);

    env.ledger().set_timestamp(1_000);

    let mut recipients = Vec::new(&env);
    recipients.push_back(recipient.clone());
    let mut amounts = Vec::new(&env);
    amounts.push_back(200_i128);

    let id = c.create_invoice(&creator, &recipients, &amounts, &token_id, &9_999_u64);

    // Pay full amount — should auto-release.
    c.pay(&payer, &id, &200_i128);

    let invoice = c.get_invoice(&id);
    assert_eq!(invoice.status, InvoiceStatus::Released);

    // Recipient should have received 200.
    assert_eq!(tk.balance(&recipient), 200);
}

#[test]
fn test_partial_pay_then_release() {
    let (env, contract_id, token_id) = setup();
    let c = client(&env, &contract_id);
    let tk = token_client(&env, &token_id);

    let creator = Address::generate(&env);
    let payer1 = Address::generate(&env);
    let payer2 = Address::generate(&env);
    let recipient = Address::generate(&env);

    let stellar_asset = StellarAssetClient::new(&env, &token_id);
    stellar_asset.mint(&payer1, &150);
    stellar_asset.mint(&payer2, &150);

    env.ledger().set_timestamp(1_000);

    let mut recipients = Vec::new(&env);
    recipients.push_back(recipient.clone());
    let mut amounts = Vec::new(&env);
    amounts.push_back(300_i128);

    let id = c.create_invoice(&creator, &recipients, &amounts, &token_id, &9_999_u64);

    c.pay(&payer1, &id, &150_i128);
    let invoice = c.get_invoice(&id);
    assert_eq!(invoice.status, InvoiceStatus::Pending);

    c.pay(&payer2, &id, &150_i128);
    let invoice = c.get_invoice(&id);
    assert_eq!(invoice.status, InvoiceStatus::Released);
    assert_eq!(tk.balance(&recipient), 300);
}

#[test]
fn test_refund_after_deadline() {
    let (env, contract_id, token_id) = setup();
    let c = client(&env, &contract_id);
    let tk = token_client(&env, &token_id);

    let creator = Address::generate(&env);
    let payer = Address::generate(&env);
    let recipient = Address::generate(&env);

    let stellar_asset = StellarAssetClient::new(&env, &token_id);
    stellar_asset.mint(&payer, &100);

    env.ledger().set_timestamp(1_000);

    let mut recipients = Vec::new(&env);
    recipients.push_back(recipient.clone());
    let mut amounts = Vec::new(&env);
    amounts.push_back(500_i128);

    let id = c.create_invoice(&creator, &recipients, &amounts, &token_id, &2_000_u64);

    // Partial payment.
    c.pay(&payer, &id, &100_i128);

    // Advance past deadline.
    env.ledger().set_timestamp(3_000);

    c.refund(&id);

    let invoice = c.get_invoice(&id);
    assert_eq!(invoice.status, InvoiceStatus::Refunded);
    // Payer should be refunded.
    assert_eq!(tk.balance(&payer), 100);
}

#[test]
#[should_panic(expected = "invoice deadline has passed")]
fn test_pay_after_deadline_panics() {
    let (env, contract_id, token_id) = setup();
    let c = client(&env, &contract_id);

    let creator = Address::generate(&env);
    let payer = Address::generate(&env);
    let recipient = Address::generate(&env);

    let stellar_asset = StellarAssetClient::new(&env, &token_id);
    stellar_asset.mint(&payer, &100);

    env.ledger().set_timestamp(1_000);

    let mut recipients = Vec::new(&env);
    recipients.push_back(recipient.clone());
    let mut amounts = Vec::new(&env);
    amounts.push_back(100_i128);

    let id = c.create_invoice(&creator, &recipients, &amounts, &token_id, &2_000_u64);

    env.ledger().set_timestamp(3_000);
    c.pay(&payer, &id, &100_i128);
}

#[test]
#[should_panic(expected = "payment exceeds remaining balance")]
fn test_overpayment_panics() {
    let (env, contract_id, token_id) = setup();
    let c = client(&env, &contract_id);

    let creator = Address::generate(&env);
    let payer = Address::generate(&env);
    let recipient = Address::generate(&env);

    let stellar_asset = StellarAssetClient::new(&env, &token_id);
    stellar_asset.mint(&payer, &1_000);

    env.ledger().set_timestamp(1_000);

    let mut recipients = Vec::new(&env);
    recipients.push_back(recipient.clone());
    let mut amounts = Vec::new(&env);
    amounts.push_back(100_i128);

    let id = c.create_invoice(&creator, &recipients, &amounts, &token_id, &9_999_u64);
    c.pay(&payer, &id, &200_i128);
}

#[test]
fn test_multi_recipient_release() {
    let (env, contract_id, token_id) = setup();
    let c = client(&env, &contract_id);
    let tk = token_client(&env, &token_id);

    let creator = Address::generate(&env);
    let payer = Address::generate(&env);
    let r1 = Address::generate(&env);
    let r2 = Address::generate(&env);
    let r3 = Address::generate(&env);

    let stellar_asset = StellarAssetClient::new(&env, &token_id);
    stellar_asset.mint(&payer, &600);

    env.ledger().set_timestamp(1_000);

    let mut recipients = Vec::new(&env);
    recipients.push_back(r1.clone());
    recipients.push_back(r2.clone());
    recipients.push_back(r3.clone());

    let mut amounts = Vec::new(&env);
    amounts.push_back(100_i128);
    amounts.push_back(200_i128);
    amounts.push_back(300_i128);

    let id = c.create_invoice(&creator, &recipients, &amounts, &token_id, &9_999_u64);
    c.pay(&payer, &id, &600_i128);

    assert_eq!(tk.balance(&r1), 100);
    assert_eq!(tk.balance(&r2), 200);
    assert_eq!(tk.balance(&r3), 300);
}

#[test]
fn test_audit_log() {
    let (env, contract_id, token_id) = setup();
    let c = client(&env, &contract_id);
    let stellar_asset = StellarAssetClient::new(&env, &token_id);

    let creator = Address::generate(&env);
    let payer = Address::generate(&env);
    let recipient = Address::generate(&env);

    stellar_asset.mint(&payer, &500);

    env.ledger().set_timestamp(1_000);

    let mut recipients = Vec::new(&env);
    recipients.push_back(recipient.clone());
    let mut amounts = Vec::new(&env);
    amounts.push_back(200_i128);

    let id = c.create_invoice(&creator, &recipients, &amounts, &token_id, &9_999_u64);

    // Perform action: pay (auto-release)
    c.pay(&payer, &id, &200_i128);

    let invoice = c.get_invoice(&id);
    assert_eq!(invoice.status, InvoiceStatus::Released);

    // Check audit log has 2 entries (pay and release)
    let log = c.get_audit_log(&id);
    assert_eq!(log.len(), 2);
    assert_eq!(log.get_unchecked(0).action, symbol_short!("pay"));
    assert_eq!(log.get_unchecked(1).action, symbol_short!("release"));
}

#[test]
fn test_audit_log_with_cancel() {
    let (env, contract_id, token_id) = setup();
    let c = client(&env, &contract_id);

    let creator = Address::generate(&env);
    let recipient = Address::generate(&env);

    env.ledger().set_timestamp(1_000);

    let mut recipients = Vec::new(&env);
    recipients.push_back(recipient.clone());
    let mut amounts = Vec::new(&env);
    amounts.push_back(100_i128);

    let id = c.create_invoice(&creator, &recipients, &amounts, &token_id, &9_999_u64);

    // Cancel the invoice (no payments, so status goes to Refunded)
    c.cancel_invoice(&creator, &id);

    // Check audit log has 1 entry (cancel)
    let log = c.get_audit_log(&id);
    assert_eq!(log.len(), 1);
    assert_eq!(log.get_unchecked(0).action, symbol_short!("cancel"));
    assert_eq!(log.get_unchecked(0).actor, creator);
}

#[test]
fn test_template_save_and_create_two_invoices() {
    let (env, contract_id, token_id) = setup();
    let c = client(&env, &contract_id);
    let tk = token_client(&env, &token_id);
    let stellar_asset = StellarAssetClient::new(&env, &token_id);

    let creator = Address::generate(&env);
    let payer = Address::generate(&env);
    let recipient = Address::generate(&env);

    stellar_asset.mint(&payer, &400);
    env.ledger().set_timestamp(1_000);

    let mut recipients = Vec::new(&env);
    recipients.push_back(recipient.clone());
    let mut amounts = Vec::new(&env);
    amounts.push_back(100_i128);

    let name = soroban_sdk::symbol_short!("bill");
    c.save_template(&creator, &name, &recipients, &amounts, &token_id);

    let id1 = c.create_from_template(&creator, &name, &5_000_u64);
    let id2 = c.create_from_template(&creator, &name, &6_000_u64);

    assert_ne!(id1, id2);

    c.pay(&payer, &id1, &100_i128);
    c.pay(&payer, &id2, &100_i128);

    assert_eq!(c.get_invoice(&id1).status, InvoiceStatus::Released);
    assert_eq!(c.get_invoice(&id2).status, InvoiceStatus::Released);
    assert_eq!(tk.balance(&recipient), 200);
}

#[test]
fn test_template_overwrite() {
    let (env, contract_id, token_id) = setup();
    let c = client(&env, &contract_id);

    let creator = Address::generate(&env);
    let r1 = Address::generate(&env);
    let r2 = Address::generate(&env);

    env.ledger().set_timestamp(1_000);

    let name = soroban_sdk::symbol_short!("tmpl");

    let mut recipients1 = Vec::new(&env);
    recipients1.push_back(r1.clone());
    let mut amounts1 = Vec::new(&env);
    amounts1.push_back(50_i128);
    c.save_template(&creator, &name, &recipients1, &amounts1, &token_id);

    // Overwrite with different recipient
    let mut recipients2 = Vec::new(&env);
    recipients2.push_back(r2.clone());
    let mut amounts2 = Vec::new(&env);
    amounts2.push_back(75_i128);
    c.save_template(&creator, &name, &recipients2, &amounts2, &token_id);

    let id = c.create_from_template(&creator, &name, &9_999_u64);
    let invoice = c.get_invoice(&id);
    // Should use the overwritten template (r2, 75)
    assert_eq!(invoice.recipients.get_unchecked(0), r2);
    assert_eq!(invoice.amounts.get_unchecked(0), 75_i128);
}

#[test]
fn test_cancel_with_refund() {
    let (env, contract_id, token_id) = setup();
    let c = client(&env, &contract_id);
    let tk = token_client(&env, &token_id);
    let stellar_asset = StellarAssetClient::new(&env, &token_id);

    let creator = Address::generate(&env);
    let payer = Address::generate(&env);
    let recipient = Address::generate(&env);

    stellar_asset.mint(&payer, &300);
    env.ledger().set_timestamp(1_000);

    let mut recipients = Vec::new(&env);
    recipients.push_back(recipient.clone());
    let mut amounts = Vec::new(&env);
    amounts.push_back(300_i128);

    let id = c.create_invoice(&creator, &recipients, &amounts, &token_id, &9_999_u64);

    // Partial payment before deadline
    c.pay(&payer, &id, &150_i128);
    assert_eq!(tk.balance(&payer), 150);

    // Creator cancels — payer should be refunded
    c.cancel_invoice(&creator, &id);

    let invoice = c.get_invoice(&id);
    assert_eq!(invoice.status, InvoiceStatus::Refunded);
    assert_eq!(tk.balance(&payer), 300);
}

#[test]
#[should_panic(expected = "invoice is not pending")]
fn test_cancel_non_pending_panics() {
    let (env, contract_id, token_id) = setup();
    let c = client(&env, &contract_id);
    let stellar_asset = StellarAssetClient::new(&env, &token_id);

    let creator = Address::generate(&env);
    let payer = Address::generate(&env);
    let recipient = Address::generate(&env);

    stellar_asset.mint(&payer, &100);
    env.ledger().set_timestamp(1_000);

    let mut recipients = Vec::new(&env);
    recipients.push_back(recipient.clone());
    let mut amounts = Vec::new(&env);
    amounts.push_back(100_i128);

    let id = c.create_invoice(&creator, &recipients, &amounts, &token_id, &9_999_u64);
    c.pay(&payer, &id, &100_i128); // auto-releases
    c.cancel_invoice(&creator, &id); // should panic
}

#[test]
fn test_get_payer_total() {
    let (env, contract_id, token_id) = setup();
    let c = client(&env, &contract_id);
    let stellar_asset = StellarAssetClient::new(&env, &token_id);

    let creator = Address::generate(&env);
    let payer = Address::generate(&env);
    let other = Address::generate(&env);
    let recipient = Address::generate(&env);

    stellar_asset.mint(&payer, &500);
    env.ledger().set_timestamp(1_000);

    let mut recipients = Vec::new(&env);
    recipients.push_back(recipient.clone());
    let mut amounts = Vec::new(&env);
    amounts.push_back(500_i128);

    let id = c.create_invoice(&creator, &recipients, &amounts, &token_id, &9_999_u64);

    // Zero before any payment
    assert_eq!(c.get_payer_total(&id, &payer), 0);
    // Address that never paid
    assert_eq!(c.get_payer_total(&id, &other), 0);

    c.pay(&payer, &id, &200_i128);
    assert_eq!(c.get_payer_total(&id, &payer), 200);

    c.pay(&payer, &id, &150_i128);
    assert_eq!(c.get_payer_total(&id, &payer), 350);
}

#[test]
fn test_audit_log_with_extend() {
    let (env, contract_id, token_id) = setup();
    let c = client(&env, &contract_id);

    let creator = Address::generate(&env);
    let recipient = Address::generate(&env);

    env.ledger().set_timestamp(1_000);

    let mut recipients = Vec::new(&env);
    recipients.push_back(recipient.clone());
    let mut amounts = Vec::new(&env);
    amounts.push_back(100_i128);

    let id = c.create_invoice(&creator, &recipients, &amounts, &token_id, &2_000_u64);

    // Extend the deadline
    c.extend_deadline(&creator, &id, &9_999_u64);

    // Check audit log has 1 entry (extend)
    let log = c.get_audit_log(&id);
    assert_eq!(log.len(), 1);
    assert_eq!(log.get_unchecked(0).action, symbol_short!("extend"));
    assert_eq!(log.get_unchecked(0).actor, creator);
}

#[test]
fn test_create_subscription() {
    let (env, contract_id, token_id) = setup();
    let c = client(&env, &contract_id);
    let tk = token_client(&env, &token_id);
    let stellar_asset = StellarAssetClient::new(&env, &token_id);

    let creator = Address::generate(&env);
    let payer = Address::generate(&env);
    let recipient = Address::generate(&env);

    stellar_asset.mint(&payer, &500);

    env.ledger().set_timestamp(1_000);

    let mut recipients = Vec::new(&env);
    recipients.push_back(recipient.clone());
    let mut amounts = Vec::new(&env);
    amounts.push_back(200_i128);

    // Create a 3-month subscription
    let id = c.create_subscription(&creator, &recipients, &amounts, &token_id, &3_u32);
    assert_eq!(id, 1);

    // Pay and auto-release first invoice
    c.pay(&payer, &id, &200_i128);

    let invoice = c.get_invoice(&id);
    assert_eq!(invoice.status, InvoiceStatus::Released);

    // Second invoice should be created automatically (30 days after release)
    let second_invoice = c.get_invoice(&2);
    assert_eq!(second_invoice.status, InvoiceStatus::Pending);

    // Recipient should have received 200 from first invoice
    assert_eq!(tk.balance(&recipient), 200);
}