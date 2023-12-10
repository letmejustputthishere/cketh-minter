use crate::address::Address;
use crate::eth_logs::{EventSource, TransferEvent};
use crate::eth_rpc::BlockTag;

use crate::lifecycle::upgrade::UpgradeArg;
use crate::lifecycle::EthereumNetwork;
use crate::numeric::{BlockNumber, Wei};




use std::cell::RefCell;
use std::collections::{btree_map, BTreeMap, BTreeSet, HashSet};
use strum_macros::EnumIter;

pub mod audit;
pub mod event;

#[cfg(test)]
mod tests;

thread_local! {
    pub static STATE: RefCell<Option<State>> = RefCell::default();
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MintedEvent {
    pub transfer_event: TransferEvent,
}

impl MintedEvent {
    pub fn source(&self) -> EventSource {
        self.transfer_event.source()
    }
}

#[derive(Debug, PartialEq, Clone)]
pub struct State {
    pub ethereum_network: EthereumNetwork,
    pub ethereum_contract_address: Option<Address>,
    pub ethereum_block_height: BlockTag,
    pub first_scraped_block_number: BlockNumber,
    pub last_scraped_block_number: BlockNumber,
    pub last_observed_block_number: Option<BlockNumber>,
    pub events_to_mint: BTreeMap<EventSource, TransferEvent>,
    pub minted_events: BTreeMap<EventSource, MintedEvent>,
    pub invalid_events: BTreeMap<EventSource, String>,
    pub skipped_blocks: BTreeSet<BlockNumber>,

    /// Locks preventing concurrent execution timer tasks
    pub active_tasks: HashSet<TaskType>,

    /// Number of HTTP outcalls since the last upgrade.
    /// Used to correlate request and response in logs.
    pub http_request_counter: u64,
}

#[derive(Debug, Eq, PartialEq)]
pub enum InvalidStateError {
    InvalidTransactionNonce(String),
    InvalidEcdsaKeyName(String),
    InvalidLedgerId(String),
    InvalidEthereumContractAddress(String),
    InvalidMinimumWithdrawalAmount(String),
    InvalidLastScrapedBlockNumber(String),
}

impl State {
    pub fn validate_config(&self) -> Result<(), InvalidStateError> {
        if self
            .ethereum_contract_address
            .iter()
            .any(|address| address == &Address::ZERO)
        {
            return Err(InvalidStateError::InvalidEthereumContractAddress(
                "ethereum_contract_address cannot be the zero address".to_string(),
            ));
        }
        Ok(())
    }

    fn record_event_to_mint(&mut self, event: &TransferEvent) {
        let event_source = event.source();
        assert!(
            !self.events_to_mint.contains_key(&event_source),
            "there must be no two different events with the same source"
        );
        assert!(!self.minted_events.contains_key(&event_source));
        assert!(!self.invalid_events.contains_key(&event_source));

        self.events_to_mint.insert(event_source, event.clone());
    }

    pub fn has_events_to_mint(&self) -> bool {
        !self.events_to_mint.is_empty()
    }

    fn record_invalid_deposit(&mut self, source: EventSource, error: String) -> bool {
        assert!(
            !self.events_to_mint.contains_key(&source),
            "attempted to mark an accepted event as invalid"
        );
        assert!(
            !self.minted_events.contains_key(&source),
            "attempted to mark a minted event {source:?} as invalid"
        );

        match self.invalid_events.entry(source) {
            btree_map::Entry::Occupied(_) => false,
            btree_map::Entry::Vacant(entry) => {
                entry.insert(error);
                true
            }
        }
    }

    fn record_successful_mint(&mut self, source: EventSource) {
        assert!(
            !self.invalid_events.contains_key(&source),
            "attempted to mint an event previously marked as invalid {source:?}"
        );
        let transfer_event = match self.events_to_mint.remove(&source) {
            Some(event) => event,
            None => panic!("attempted to mint ckETH for an unknown event {source:?}"),
        };

        assert_eq!(
            self.minted_events
                .insert(source, MintedEvent { transfer_event }),
            None,
            "attempted to mint ckETH twice for the same event {source:?}"
        );
    }

    pub fn next_request_id(&mut self) -> u64 {
        let current_request_id = self.http_request_counter;
        // overflow is not an issue here because we only use `next_request_id` to correlate
        // requests and responses in logs.
        self.http_request_counter = self.http_request_counter.wrapping_add(1);
        current_request_id
    }

    pub fn record_skipped_block(&mut self, block_number: BlockNumber) {
        assert!(
            self.skipped_blocks.insert(block_number),
            "BUG: block {} was already skipped",
            block_number
        );
    }

    pub const fn ethereum_network(&self) -> EthereumNetwork {
        self.ethereum_network
    }

    pub const fn ethereum_block_height(&self) -> BlockTag {
        self.ethereum_block_height
    }

    fn upgrade(&mut self, upgrade_args: UpgradeArg) -> Result<(), InvalidStateError> {
        use std::str::FromStr;

        let UpgradeArg {
            ethereum_contract_address,
            ethereum_block_height,
        } = upgrade_args;
        if let Some(address) = ethereum_contract_address {
            let ethereum_contract_address = Address::from_str(&address).map_err(|e| {
                InvalidStateError::InvalidEthereumContractAddress(format!("ERROR: {}", e))
            })?;
            self.ethereum_contract_address = Some(ethereum_contract_address);
        }
        if let Some(block_height) = ethereum_block_height {
            self.ethereum_block_height = block_height.into();
        }
        self.validate_config()
    }

    /// Checks whether two states are equivalent.
    pub fn is_equivalent_to(&self, other: &Self) -> Result<(), String> {
        // We define the equivalence using the upgrade procedure.
        // Replaying the event log won't produce exactly the same state we had before the upgrade,
        // but a state that equivalent for all practical purposes.
        //
        // For example, we don't compare:
        // 1. Computed fields and caches, such as `ecdsa_public_key`.
        // 2. Transient fields, such as `active_tasks`.
        use ic_utils_ensure::ensure_eq;

        ensure_eq!(self.ethereum_network, other.ethereum_network);
        ensure_eq!(
            self.ethereum_contract_address,
            other.ethereum_contract_address
        );
        ensure_eq!(
            self.first_scraped_block_number,
            other.first_scraped_block_number
        );
        ensure_eq!(
            self.last_scraped_block_number,
            other.last_scraped_block_number
        );
        ensure_eq!(self.ethereum_block_height, other.ethereum_block_height);
        ensure_eq!(self.events_to_mint, other.events_to_mint);
        ensure_eq!(self.minted_events, other.minted_events);
        ensure_eq!(self.invalid_events, other.invalid_events);
        Ok(())
    }
}

pub fn read_state<R>(f: impl FnOnce(&State) -> R) -> R {
    STATE.with(|s| f(s.borrow().as_ref().expect("BUG: state is not initialized")))
}

/// Mutates (part of) the current state using `f`.
///
/// Panics if there is no state.
pub fn mutate_state<F, R>(f: F) -> R
where
    F: FnOnce(&mut State) -> R,
{
    STATE.with(|s| {
        f(s.borrow_mut()
            .as_mut()
            .expect("BUG: state is not initialized"))
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EthBalance {
    /// Amount of ETH controlled by the minter's address via tECDSA.
    /// Note that invalid deposits are not accounted for and so so this value
    /// might be less than what is displayed by Etherscan
    /// or retrieved by the JSON-RPC call `eth_getBalance`.
    /// Also some transactions may have gone directly to the minter's address
    /// without going via the helper smart contract.
    eth_balance: Wei,
    /// Total amount of fees across all finalized transactions ckETH -> ETH.
    total_effective_tx_fees: Wei,
    /// Total amount of fees that were charged to the user during the withdrawal
    /// but not consumed by the finalized transaction ckETH -> ETH
    total_unspent_tx_fees: Wei,
}

impl Default for EthBalance {
    fn default() -> Self {
        Self {
            eth_balance: Wei::ZERO,
            total_effective_tx_fees: Wei::ZERO,
            total_unspent_tx_fees: Wei::ZERO,
        }
    }
}

impl EthBalance {
    fn eth_balance_sub(&mut self, value: Wei) {
        self.eth_balance = self.eth_balance.checked_sub(value).unwrap_or_else(|| {
            panic!(
                "BUG: underflow when subtracting {} from {}",
                value, self.eth_balance
            )
        })
    }

    fn total_effective_tx_fees_add(&mut self, value: Wei) {
        self.total_effective_tx_fees = self
            .total_effective_tx_fees
            .checked_add(value)
            .unwrap_or_else(|| {
                panic!(
                    "BUG: overflow when adding {} to {}",
                    value, self.total_effective_tx_fees
                )
            })
    }

    fn total_unspent_tx_fees_add(&mut self, value: Wei) {
        self.total_unspent_tx_fees = self
            .total_unspent_tx_fees
            .checked_add(value)
            .unwrap_or_else(|| {
                panic!(
                    "BUG: overflow when adding {} to {}",
                    value, self.total_unspent_tx_fees
                )
            })
    }

    pub fn eth_balance(&self) -> Wei {
        self.eth_balance
    }
    pub fn total_effective_tx_fees(&self) -> Wei {
        self.total_effective_tx_fees
    }

    pub fn total_unspent_tx_fees(&self) -> Wei {
        self.total_unspent_tx_fees
    }
}

#[derive(Debug, Hash, Copy, Clone, PartialEq, Eq, EnumIter)]
pub enum TaskType {
    MintCkEth,
    RetrieveEth,
    ScrapEthLogs,
    Reimbursement,
}
