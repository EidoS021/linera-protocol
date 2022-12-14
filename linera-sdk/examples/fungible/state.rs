// Copyright (c) Zefchain Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use linera_sdk::{crypto::PublicKey, ApplicationId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The application state.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct FungibleToken {
    accounts: BTreeMap<AccountOwner, u128>,
    nonces: BTreeMap<AccountOwner, Nonce>,
}

/// An account owner.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub enum AccountOwner {
    /// An account protected by a private key.
    Key(PublicKey),
    /// An account for an application.
    Application(ApplicationId),
}

/// A single-use number to prevent replay attacks.
#[derive(Debug, Default, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct Nonce(u64);

#[allow(dead_code)]
impl FungibleToken {
    /// Initialize the application state with some accounts with initial balances.
    pub(crate) fn initialize_accounts(&mut self, accounts: BTreeMap<AccountOwner, u128>) {
        self.accounts = accounts;
    }

    /// Obtain the balance for an `account`.
    pub(crate) fn balance(&self, account: &AccountOwner) -> u128 {
        self.accounts.get(&account).copied().unwrap_or(0)
    }

    /// Credit an `account` with the provided `amount`.
    pub(crate) fn credit(&mut self, account: AccountOwner, amount: u128) {
        *self.accounts.entry(account).or_default() += amount;
    }
}

/// Alias to the application type, so that the boilerplate module can reference it.
pub type ApplicationState = FungibleToken;
