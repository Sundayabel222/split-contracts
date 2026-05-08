# split-contracts

![Rust](https://img.shields.io/badge/Rust-1.84+-orange?logo=rust)
![Soroban SDK](https://img.shields.io/badge/soroban--sdk-22.0.0-blueviolet)
![License](https://img.shields.io/badge/license-MIT-green)
![CI](https://github.com/stellar-split/split-contracts/actions/workflows/test.yml/badge.svg)

Soroban smart contracts powering **StellarSplit** — on-chain invoice & payment splitting on Stellar.

## What It Does

StellarSplit lets users create on-chain invoices where multiple payers each owe a share. When all shares are paid, the contract automatically routes USDC to each recipient. If the deadline passes unfunded, contributors are refunded.

**Use cases:** group bills, freelancer team payments, remittances across LATAM and Africa.

## Tech Stack

| Layer | Technology |
|-------|-----------|
| Language | Rust 1.84+ |
| Smart Contract SDK | soroban-sdk 22.0.0 |
| Test Runner | `cargo test` |
| Deploy Tool | stellar-cli |
| Network | Stellar Testnet / Mainnet |

## Local Setup

### Prerequisites

- Rust 1.84+ with `wasm32-unknown-unknown` target
- stellar-cli

```bash
# Install Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Add WASM target
rustup target add wasm32-unknown-unknown

# Install stellar-cli
cargo install --locked stellar-cli --features opt
```

### Clone & Build

```bash
git clone https://github.com/stellar-split/split-contracts.git
cd split-contracts
cargo build --target wasm32-unknown-unknown --release
```

### Run Tests

```bash
cargo test --workspace
```

## Contract Function Reference

### `create_invoice`

```rust
create_invoice(
    env: Env,
    creator: Address,
    recipients: Vec<Address>,
    amounts: Vec<i128>,
    token: Address,
    deadline: u64,
) -> u64
```

Creates a new invoice. Returns the invoice ID. `amounts[i]` is owed to `recipients[i]`. `deadline` is a Unix timestamp.

### `pay`

```rust
pay(env: Env, payer: Address, invoice_id: u64, amount: i128)
```

Transfers `amount` of the invoice token from `payer` to the contract. Auto-releases if fully funded.

### `release`

```rust
release(env: Env, invoice_id: u64)
```

Routes funds to all recipients. Callable by anyone once fully funded.

### `refund`

```rust
refund(env: Env, invoice_id: u64)
```

Refunds all payers. Callable by anyone after the deadline if not fully funded.

### `get_invoice`

```rust
get_invoice(env: Env, invoice_id: u64) -> Invoice
```

Returns the full invoice struct.

## Testnet Contract Address

```
PLACEHOLDER — update after deployment
```

## Contributing via Drips Wave

This project participates in the [Drips Wave Program](https://drips.network/wave) by the Stellar Development Foundation. Contributors can earn rewards by completing open issues.

See [CONTRIBUTING.md](./CONTRIBUTING.md) for the full contribution guide.

**Do not start coding until assigned to an issue by a maintainer.**
