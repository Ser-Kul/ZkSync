use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};

use futures::channel::mpsc;
use web3::contract::{tokens::Tokenize, Options};
use web3::types::{H256, U256};

use eth_client::SignedCallResult;
use models::Operation;

use super::ETHSender;
use crate::eth_sender::database::DatabaseAccess;
use crate::eth_sender::ethereum_interface::EthereumInterface;
use crate::eth_sender::transactions::{ExecutedTxStatus, OperationETHState, TransactionETHState};

const CHANNEL_CAPACITY: usize = 16;

#[derive(Debug, Default)]
pub(super) struct MockDatabase {
    restore_state: VecDeque<OperationETHState>,
    unconfirmed_operations: RefCell<HashMap<H256, TransactionETHState>>,
    confirmed_operations: RefCell<HashMap<H256, TransactionETHState>>,
}

impl MockDatabase {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_restorable_state(
        restore_state: impl IntoIterator<Item = OperationETHState>,
    ) -> Self {
        Self {
            restore_state: restore_state.into_iter().collect(),
            ..Default::default()
        }
    }

    /// Ensures that the provided transaction is stored in the database and not confirmed yet.
    pub fn assert_stored(&self, tx: &TransactionETHState) {
        assert_eq!(
            self.unconfirmed_operations.borrow().get(&tx.signed_tx.hash),
            Some(tx)
        );

        assert!(self
            .confirmed_operations
            .borrow()
            .get(&tx.signed_tx.hash)
            .is_none());
    }

    pub fn assert_not_stored(&self, tx: &TransactionETHState) {
        assert!(self
            .confirmed_operations
            .borrow()
            .get(&tx.signed_tx.hash)
            .is_none());

        assert!(self
            .unconfirmed_operations
            .borrow()
            .get(&tx.signed_tx.hash)
            .is_none());
    }

    /// Ensures that the provided transaction is stored as confirmed.
    pub fn assert_confirmed(&self, tx: &TransactionETHState) {
        assert_eq!(
            self.confirmed_operations.borrow().get(&tx.signed_tx.hash),
            Some(tx)
        );

        assert!(self
            .unconfirmed_operations
            .borrow()
            .get(&tx.signed_tx.hash)
            .is_none());
    }
}

impl DatabaseAccess for MockDatabase {
    fn restore_state(&self) -> Result<VecDeque<OperationETHState>, failure::Error> {
        Ok(self.restore_state.clone())
    }

    fn save_unconfirmed_operation(&self, tx: &TransactionETHState) -> Result<(), failure::Error> {
        self.unconfirmed_operations
            .borrow_mut()
            .insert(tx.signed_tx.hash, tx.clone());

        Ok(())
    }

    fn confirm_operation(&self, hash: &H256) -> Result<(), failure::Error> {
        let mut unconfirmed_operations = self.unconfirmed_operations.borrow_mut();
        assert!(
            unconfirmed_operations.contains_key(hash),
            "Request to confirm operation that was not stored"
        );

        let operation = unconfirmed_operations.remove(hash).unwrap();
        self.confirmed_operations
            .borrow_mut()
            .insert(*hash, operation);

        Ok(())
    }
}

#[derive(Debug)]
pub(super) struct MockEthereum {
    pub block_number: u64,
    pub nonce: U256,
    pub gas_price: U256,
    pub tx_statuses: RefCell<HashMap<H256, ExecutedTxStatus>>,
    pub sent_txs: RefCell<HashMap<H256, SignedCallResult>>,
}

impl Default for MockEthereum {
    fn default() -> Self {
        Self {
            block_number: 1,
            nonce: Default::default(),
            gas_price: 100.into(),
            tx_statuses: Default::default(),
            sent_txs: Default::default(),
        }
    }
}

impl MockEthereum {
    /// A fake `sha256` hasher, which calculates an `std::hash` instead.
    /// This is done for simplicity and it's also much faster.
    pub fn fake_sha256(data: &[u8]) -> H256 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::Hasher;

        let mut hasher = DefaultHasher::new();
        hasher.write(data);

        let result = hasher.finish();

        H256::from_low_u64_ne(result)
    }

    pub fn assert_sent(&self, tx: &TransactionETHState) {
        assert_eq!(
            self.sent_txs.borrow().get(&tx.signed_tx.hash),
            Some(&tx.signed_tx)
        );
    }

    /// Increments the blocks by a provided `confirmations` and marks the sent transaction
    /// as a success.
    pub fn add_successfull_execution(&mut self, tx: &TransactionETHState, confirmations: u64) {
        self.block_number += confirmations;
        self.nonce += 1.into();

        let status = ExecutedTxStatus {
            confirmations,
            success: true,
            receipt: None,
        };
        self.tx_statuses
            .borrow_mut()
            .insert(tx.signed_tx.hash, status);
    }
}

impl EthereumInterface for MockEthereum {
    fn get_tx_status(&self, hash: &H256) -> Result<Option<ExecutedTxStatus>, failure::Error> {
        Ok(self.tx_statuses.borrow().get(hash).cloned())
    }

    fn block_number(&self) -> Result<u64, failure::Error> {
        Ok(self.block_number)
    }

    fn gas_price(&self) -> Result<U256, failure::Error> {
        Ok(self.gas_price)
    }

    fn current_nonce(&self) -> Result<U256, failure::Error> {
        Ok(self.nonce)
    }

    fn send_tx(&self, signed_tx: &SignedCallResult) -> Result<(), failure::Error> {
        self.sent_txs
            .borrow_mut()
            .insert(signed_tx.hash, signed_tx.clone());

        Ok(())
    }

    fn sign_call_tx<P: Tokenize>(
        &self,
        _func: &str,
        params: P,
        options: Options,
    ) -> Result<SignedCallResult, failure::Error> {
        let raw_tx = ethabi::encode(params.into_tokens().as_ref());
        let hash = Self::fake_sha256(raw_tx.as_ref()); // Okay for test purposes.

        Ok(SignedCallResult {
            raw_tx,
            gas_price: options.gas_price.unwrap_or(self.gas_price),
            nonce: options.nonce.unwrap_or(self.nonce),
            hash,
        })
    }
}

/// Creates a default `ETHSender` with mock Ethereum connection and database.
/// Return the `ETHSender` itself along with communication channels to interact with it.
pub(super) fn default_eth_sender() -> (
    ETHSender<MockEthereum, MockDatabase>,
    mpsc::Sender<Operation>,
    mpsc::Receiver<Operation>,
) {
    let ethereum = MockEthereum::default();
    let db = MockDatabase::new();

    let (operation_sender, operation_receiver) = mpsc::channel(CHANNEL_CAPACITY);
    let (notify_sender, notify_receiver) = mpsc::channel(CHANNEL_CAPACITY);

    (
        ETHSender::new(db, ethereum, operation_receiver, notify_sender),
        operation_sender,
        notify_receiver,
    )
}
