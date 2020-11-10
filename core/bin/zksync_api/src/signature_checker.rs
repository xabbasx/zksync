//! `signature_checker` module provides a detached thread routine
//! dedicated for checking the signatures of incoming transactions.
//! Main routine of this module operates a multithreaded event loop,
//! which is used to spawn concurrent tasks to efficiently check the
//! transactions signatures.

// External uses
use futures::{
    channel::{mpsc, oneshot},
    StreamExt,
};
use tokio::runtime::{Builder, Handle};
// Workspace uses
use zksync_types::{
    tx::{BatchSignData, TxEthSignature},
    SignedZkSyncTx, ZkSyncTx,
};
// Local uses
use crate::{eth_checker::EthereumChecker, tx_error::TxAddError};
use zksync_config::ConfigurationOptions;
use zksync_types::tx::EthSignData;
use zksync_utils::panic_notify::ThreadPanicNotify;

/// Represents yet unverified transaction with the corresponding
/// Ethereum signature and the message.
#[derive(Debug, Clone)]
pub struct TxWithSignData {
    pub tx: ZkSyncTx,
    /// `eth_sign_data` is a tuple of the Ethereum signature and the message
    /// which user should have signed with their private key.
    /// Can be `None` if the Ethereum signature is not required.
    pub eth_sign_data: Option<EthSignData>,
}

/// `TxVariant` is used to form a verify request. It is possible to wrap
/// either a single transaction, or the transaction batch.
#[derive(Debug, Clone)]
pub enum TxVariant {
    Tx(TxWithSignData),
    Batch(Vec<TxWithSignData>, BatchSignData),
}

#[derive(Debug, Clone)]
pub enum SignedTxVariant {
    Tx(SignedZkSyncTx),
    Batch(Vec<SignedZkSyncTx>, TxEthSignature),
}

/// Wrapper on a `SignedTxVariant` which guarantees that (a batch of)
/// transaction(s) was checked and signatures associated with
/// this transactions are correct.
///
/// Underlying `SignedTxVariant` is a private field, thus no such
/// object can be created without verification.
#[derive(Debug, Clone)]
pub struct VerifiedTx(SignedTxVariant);

impl VerifiedTx {
    /// Checks the (batch of) transaction(s) correctness by verifying its
    /// Ethereum signature (if required) and `ZKSync` signature.
    pub async fn verify(
        request: &mut VerifyTxSignatureRequest,
        eth_checker: &EthereumChecker<web3::transports::Http>,
    ) -> Result<Self, TxAddError> {
        verify_eth_signature(&request, eth_checker)
            .await
            .and_then(|_| verify_tx_correctness(&mut request.tx))
            .map(|_| match &request.tx {
                TxVariant::Tx(tx) => Self(SignedTxVariant::Tx(SignedZkSyncTx {
                    tx: tx.tx.clone(),
                    eth_sign_data: tx.eth_sign_data.clone(),
                })),
                TxVariant::Batch(txs, batch_sign_data) => {
                    let txs = txs
                        .iter()
                        .map(|tx| SignedZkSyncTx {
                            tx: tx.tx.clone(),
                            eth_sign_data: tx.eth_sign_data.clone(),
                        })
                        .collect::<Vec<_>>();
                    Self(SignedTxVariant::Batch(
                        txs,
                        batch_sign_data.0.signature.clone(),
                    ))
                }
            })
    }

    /// Takes the `SignedZkSyncTx` out of the wrapper.
    pub fn unwrap_tx(self) -> SignedZkSyncTx {
        match self.0 {
            SignedTxVariant::Tx(tx) => tx,
            SignedTxVariant::Batch(_, _) => panic!("called `unwrap_tx` on a `Batch` value"),
        }
    }

    /// Takes the Vec of `SignedZkSyncTx` and the verified signature out of the wrapper.
    pub fn unwrap_batch(self) -> (Vec<SignedZkSyncTx>, TxEthSignature) {
        match self.0 {
            SignedTxVariant::Batch(txs, eth_signature) => (txs, eth_signature),
            SignedTxVariant::Tx(_) => panic!("called `unwrap_batch` on a `Tx` value"),
        }
    }
}

/// Verifies the Ethereum signature of the (batch of) transaction(s).
async fn verify_eth_signature(
    request: &VerifyTxSignatureRequest,
    eth_checker: &EthereumChecker<web3::transports::Http>,
) -> Result<(), TxAddError> {
    match &request.tx {
        TxVariant::Tx(tx) => {
            verify_eth_signature_single_tx(tx, eth_checker).await?;
        }
        TxVariant::Batch(txs, batch_sign_data) => {
            verify_eth_signature_txs_batch(txs, batch_sign_data, eth_checker).await?;
            // In case there're signatures provided for some of transactions
            // we still verify them.
            for tx in txs {
                verify_eth_signature_single_tx(tx, eth_checker).await?;
            }
        }
    }

    Ok(())
}

async fn verify_eth_signature_single_tx(
    tx: &TxWithSignData,
    eth_checker: &EthereumChecker<web3::transports::Http>,
) -> Result<(), TxAddError> {
    // Check if the tx is a `ChangePubKey` operation without an Ethereum signature.
    if let ZkSyncTx::ChangePubKey(change_pk) = &tx.tx {
        if change_pk.eth_signature.is_none() {
            // Check that user is allowed to perform this operation.
            let is_authorized = eth_checker
                .is_new_pubkey_hash_authorized(
                    change_pk.account,
                    change_pk.nonce,
                    &change_pk.new_pk_hash,
                )
                .await
                .expect("Unable to check onchain ChangePubKey Authorization");

            if !is_authorized {
                return Err(TxAddError::ChangePkNotAuthorized);
            }
        }
    }

    // Check the signature.
    if let Some(sign_data) = &tx.eth_sign_data {
        match &sign_data.signature {
            TxEthSignature::EthereumSignature(packed_signature) => {
                let signer_account = packed_signature
                    .signature_recover_signer(&sign_data.message)
                    .or(Err(TxAddError::IncorrectEthSignature))?;

                if signer_account != tx.tx.account() {
                    return Err(TxAddError::IncorrectEthSignature);
                }
            }
            TxEthSignature::EIP1271Signature(signature) => {
                let signature_correct = eth_checker
                    .is_eip1271_signature_correct(
                        tx.tx.account(),
                        &sign_data.message,
                        signature.clone(),
                    )
                    .await
                    .expect("Unable to check EIP1271 signature");

                if !signature_correct {
                    return Err(TxAddError::IncorrectTx);
                }
            }
        };
    }

    Ok(())
}

async fn verify_eth_signature_txs_batch(
    txs: &[TxWithSignData],
    batch_sign_data: &BatchSignData,
    eth_checker: &EthereumChecker<web3::transports::Http>,
) -> Result<(), TxAddError> {
    match &batch_sign_data.0.signature {
        TxEthSignature::EthereumSignature(packed_signature) => {
            let signer_account = packed_signature
                .signature_recover_signer(&batch_sign_data.0.message)
                .or(Err(TxAddError::IncorrectEthSignature))?;

            if txs.iter().any(|tx| tx.tx.account() != signer_account) {
                return Err(TxAddError::IncorrectEthSignature);
            }
        }
        TxEthSignature::EIP1271Signature(signature) => {
            for tx in txs {
                let signature_correct = eth_checker
                    .is_eip1271_signature_correct(
                        tx.tx.account(),
                        &batch_sign_data.0.message,
                        signature.clone(),
                    )
                    .await
                    .expect("Unable to check EIP1271 signature");

                if !signature_correct {
                    return Err(TxAddError::IncorrectTx);
                }
            }
        }
    };

    Ok(())
}

/// Verifies the correctness of the ZKSync transaction(s) (including the
/// signature check).
fn verify_tx_correctness(tx: &mut TxVariant) -> Result<(), TxAddError> {
    match tx {
        TxVariant::Tx(tx) => {
            if !tx.tx.check_correctness() {
                return Err(TxAddError::IncorrectTx);
            }
        }
        TxVariant::Batch(batch, _) => {
            if batch.iter_mut().any(|tx| !tx.tx.check_correctness()) {
                return Err(TxAddError::IncorrectTx);
            }
        }
    }
    Ok(())
}

/// Request for the signature check.
#[derive(Debug)]
pub struct VerifyTxSignatureRequest {
    pub tx: TxVariant,
    /// Channel for sending the check response.
    pub response: oneshot::Sender<Result<VerifiedTx, TxAddError>>,
}

/// Main routine of the concurrent signature checker.
/// See the module documentation for details.
pub fn start_sign_checker_detached(
    config_options: ConfigurationOptions,
    input: mpsc::Receiver<VerifyTxSignatureRequest>,
    panic_notify: mpsc::Sender<bool>,
) {
    let transport = web3::transports::Http::new(&config_options.web3_url).unwrap();
    let web3 = web3::Web3::new(transport);

    let eth_checker = EthereumChecker::new(web3, config_options.contract_eth_addr);

    /// Main signature check requests handler.
    /// Basically it receives the requests through the channel and verifies signatures,
    /// notifying the request sender about the check result.
    async fn checker_routine(
        handle: Handle,
        mut input: mpsc::Receiver<VerifyTxSignatureRequest>,
        eth_checker: EthereumChecker<web3::transports::Http>,
    ) {
        while let Some(mut request) = input.next().await {
            let eth_checker = eth_checker.clone();
            handle.spawn(async move {
                let resp = VerifiedTx::verify(&mut request, &eth_checker).await;

                request.response.send(resp).unwrap_or_default();
            });
        }
    }

    std::thread::Builder::new()
        .name("Signature checker thread".to_string())
        .spawn(move || {
            let _panic_sentinel = ThreadPanicNotify(panic_notify.clone());

            let mut runtime = Builder::new()
                .enable_all()
                .threaded_scheduler()
                .build()
                .expect("failed to build runtime for signature processor");
            let handle = runtime.handle().clone();
            runtime.block_on(checker_routine(handle, input, eth_checker));
        })
        .expect("failed to start signature checker thread");
}
