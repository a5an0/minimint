mod conflictfilter;
mod unzip_consensus;

use crate::consensus::conflictfilter::ConflictFilterable;
use crate::consensus::unzip_consensus::{ConsensusItems, UnzipConsensus};
use crate::database::{
    AllConsensusItemsKeyPrefix, AllPartialSignaturesKey, ConsensusItemKeyPrefix,
    PartialSignatureKey, TransactionOutputOutcomeKey, TransactionStatusKey,
};
use crate::rng::RngGenerator;
use config::ServerConfig;
use database::batch::{BatchItem, BatchTx, DbBatch};
use database::{BincodeSerialized, Database, DatabaseError, RawDatabase};
use fedimint::{FediMint, MintError};
use fediwallet::{Wallet, WalletConsensusItem, WalletError};
use hbbft::honey_badger::Batch;
use itertools::Itertools;
use mint_api::outcome::{OutputOutcome, TransactionStatus};
use mint_api::transaction::{BlindToken, Input, Output, Transaction, TransactionError};
use mint_api::{Amount, Coins, PartialSigResponse, SignRequest, TransactionId};
use rand::{CryptoRng, RngCore};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use thiserror::Error;
use tracing::{debug, error, info, trace, warn};

#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum ConsensusItem {
    Transaction(Transaction),
    // TODO: define type for tuple
    PartiallySignedRequest(TransactionId, usize, mint_api::PartialSigResponse),
    Wallet(WalletConsensusItem),
}

pub type HoneyBadgerMessage = hbbft::honey_badger::Message<u16>;
pub type ConsensusOutcome = Batch<Vec<ConsensusItem>, u16>;

pub struct FediMintConsensus<R, M>
where
    R: RngCore + CryptoRng,
    M: FediMint + Sync,
{
    /// Cryptographic random number generator used for everything
    pub rng_gen: Box<dyn RngGenerator<Rng = R>>,
    /// Configuration describing the federation and containing our secrets
    pub cfg: ServerConfig, // TODO: make custom config

    /// Our local mint
    pub mint: M, //TODO: box dyn trait
    pub wallet: Wallet,

    /// KV Database into which all state is persisted to recover from in case of a crash
    pub db: Arc<dyn RawDatabase>,
}

impl<R, M> FediMintConsensus<R, M>
where
    R: RngCore + CryptoRng,
    M: FediMint + Sync,
{
    pub fn submit_transaction(
        &self,
        transaction: Transaction,
    ) -> Result<(), TransactionSubmissionError> {
        let tx_hash = transaction.tx_hash();
        debug!("Received mint transaction {}", tx_hash);

        transaction.validate_funding(&self.cfg.fee_consensus)?;
        transaction.validate_signature()?;

        for input in &transaction.inputs {
            match input {
                Input::Coins(coins) => {
                    self.mint
                        .validate(&self.db, coins)
                        .map_err(TransactionSubmissionError::InputCoinError)?;
                }
                Input::PegIn(peg_in) => {
                    self.wallet
                        .validate_peg_in(peg_in)
                        .map_err(TransactionSubmissionError::InputPegIn)?;
                }
            }
        }

        for output in &transaction.outputs {
            match output {
                Output::Coins(coins) => {
                    self.mint
                        .validate_tiers(coins)
                        .map_err(TransactionSubmissionError::OutputCoinError)?;
                }
                Output::PegOut(peg_out) => {
                    self.wallet
                        .validate_peg_out(peg_out)
                        .map_err(TransactionSubmissionError::OutputPegOut)?;
                }
            }
        }

        let new = self
            .db
            .insert_entry(&ConsensusItem::Transaction(transaction), &())
            .expect("DB error");

        if new.is_some() {
            warn!("Added consensus item was already in consensus queue");
        } else {
            // TODO: unify with consensus stuff
            self.db
                .insert_entry(
                    &TransactionStatusKey(tx_hash),
                    &BincodeSerialized::owned(TransactionStatus::AwaitingConsensus),
                )
                .expect("DB error");
        }

        Ok(())
    }

    pub async fn process_consensus_outcome(
        &self,
        consensus_outcome: ConsensusOutcome,
    ) -> WalletConsensusItem {
        info!("Processing output of epoch {}", consensus_outcome.epoch);

        let mut db_batch = DbBatch::new();

        let ConsensusItems {
            transactions: transaction_cis,
            wallet: wallet_cis,
            mint: mint_cis,
        } = consensus_outcome
            .contributions
            .into_iter()
            .flat_map(|(peer, cis)| cis.into_iter().map(move |ci| (peer, ci)))
            .unzip_consensus();

        let (wallet_ci, wallet_sig_ci) = self
            .wallet
            .process_consensus_proposals(db_batch.transaction(), wallet_cis, self.rng_gen.get_rng())
            .await
            .expect("wallet error"); // TODO: check why this is not to be expected

        if let Some(wci) = wallet_sig_ci {
            db_batch.autocommit(|tx| tx.append_insert_new(ConsensusItem::Wallet(wci), ()));
        }

        // Since the changes to the database will happen all at once we won't be able to handle
        // conflicts between consensus items in one batch there. Thus we need to make sure that
        // all items in a batch are consistent/deterministically filter out inconsistent ones.
        // There are two item types that need checking:
        //  * peg-ins that each peg-in tx is only used to issue coins once
        //  * coin spends to avoid double spends in one batch
        let filtered_transactions = transaction_cis
            .into_iter()
            .filter_conflicts(|(_, tx)| tx)
            .collect::<Vec<_>>();

        // TODO: implement own parallel execution to avoid allocations and get rid of rayon
        let par_db_batches = filtered_transactions
            .into_par_iter()
            .map(|(peer, transaction)| {
                trace!(
                    "Processing transaction {:?} from peer {}",
                    transaction,
                    peer
                );
                let mut db_batch = DbBatch::new();
                db_batch.autocommit(|batch_tx| {
                    batch_tx.append_maybe_delete(ConsensusItem::Transaction(transaction.clone()))
                });
                // TODO: use borrowed transaction
                match self.process_transaction(db_batch.transaction(), peer, transaction.clone()) {
                    Ok(()) => {
                        db_batch.autocommit(|batch_tx| {
                            batch_tx.append_insert(
                                TransactionStatusKey(transaction.tx_hash()),
                                BincodeSerialized::owned(TransactionStatus::Accepted),
                            );
                            for (idx, output) in transaction.outputs.iter().enumerate() {
                                // TODO: writing this here will be unnecessary after saving the entire tx permanently
                                let outcome = match output {
                                    // TODO: propagate back inclusion of peg-out tx
                                    Output::PegOut(_) => Some(OutputOutcome::PegOut),
                                    Output::Coins(_) => None,
                                };
                                batch_tx.append_insert(
                                    TransactionOutputOutcomeKey(transaction.tx_hash(), idx),
                                    BincodeSerialized::owned(outcome),
                                );
                            }
                        });
                    }
                    Err(e) => {
                        db_batch.autocommit(|batch_tx| {
                            batch_tx.append_insert(
                                TransactionStatusKey(transaction.tx_hash()),
                                BincodeSerialized::owned(TransactionStatus::Error(e.to_string())),
                            )
                        });
                    }
                }

                db_batch
            })
            .collect::<Vec<_>>();
        db_batch.autocommit(|tx| tx.append_from_accumulators(par_db_batches.into_iter()));

        // TODO: move signature processing to mint module
        for (peer, id, out_idx, psig) in mint_cis {
            self.process_partial_signature(db_batch.transaction(), peer, (id, out_idx), psig);
        }

        // Apply all consensus-critical changes atomically to the DB
        self.db.apply_batch(db_batch).expect("DB error");

        // Now that we have updated the DB with the epoch results also try to combine signatures
        let mut db_batch = DbBatch::new();
        self.finalize_signatures(db_batch.transaction());
        self.db.apply_batch(db_batch).expect("DB error");

        wallet_ci
    }

    pub async fn get_consensus_proposal(
        &self,
        wallet_consensus: WalletConsensusItem,
    ) -> Vec<ConsensusItem> {
        debug!("Wallet proposal: {:?}", wallet_consensus);

        // Fetch long lived CIs and concatenate with transient wallet CI
        self.db
            .find_by_prefix(&AllConsensusItemsKeyPrefix)
            .map(|res| res.map(|(ci, ())| ci))
            .chain(std::iter::once(Ok(ConsensusItem::Wallet(wallet_consensus))))
            .collect::<Result<_, DatabaseError>>()
            .expect("DB error")
    }

    fn process_transaction(
        &self,
        mut batch: BatchTx,
        peer: u16,
        transaction: Transaction,
    ) -> Result<(), TransactionSubmissionError> {
        transaction.validate_funding(&self.cfg.fee_consensus)?;
        transaction.validate_signature()?;

        let tx_hash = transaction.tx_hash();

        for input in transaction.inputs {
            match input {
                Input::Coins(coins) => {
                    self.mint
                        .spend(&self.db, batch.subtransaction(), coins)
                        .map_err(TransactionSubmissionError::InputCoinError)?;
                }
                Input::PegIn(peg_in) => {
                    self.wallet
                        .claim_pegin(batch.subtransaction(), &peg_in)
                        .map_err(TransactionSubmissionError::InputPegIn)?;
                }
            }
        }

        for (idx, output) in transaction.outputs.into_iter().enumerate() {
            match output {
                Output::Coins(new_tokens) => {
                    let partial_sig = self
                        .mint
                        .issue(to_sign_request(new_tokens))
                        .map_err(TransactionSubmissionError::OutputCoinError)?;
                    // TODO: move consensus proposal handling into respective module
                    batch.append_insert_new(
                        ConsensusItem::PartiallySignedRequest(tx_hash, idx, partial_sig),
                        (),
                    );
                }
                Output::PegOut(peg_out) => {
                    self.wallet
                        .queue_pegout(
                            batch.subtransaction(),
                            tx_hash,
                            peg_out.recipient,
                            to_btc_amount_round_down(peg_out.amount),
                        )
                        .map_err(TransactionSubmissionError::OutputPegOut)?;
                }
            }
        }

        batch.commit();
        Ok(())
    }

    // TODO: move to fedimint
    fn process_partial_signature(
        &self,
        mut batch: BatchTx,
        peer: u16,
        req_id: (TransactionId, usize), // TODO: introduce output id
        partial_sig: PartialSigResponse,
    ) {
        match self
            .db
            .get_value::<_, BincodeSerialized<Option<OutputOutcome>>>(&TransactionOutputOutcomeKey(
                req_id.0, req_id.1,
            ))
            .expect("DB error")
            .map(|bcd| bcd.into_owned())
        {
            Some(Some(OutputOutcome::Coins { .. })) => {
                trace!(
                    "Received sig share for finalized issuance {}:{}, ignoring",
                    req_id.0,
                    req_id.1
                );
                return;
            }
            Some(None) => {}
            None | Some(Some(_)) => {
                warn!(
                    "Received sig share for output that does not exist or has wrong type ({}:{})",
                    req_id.0, req_id.1
                );
                return;
            }
        };

        debug!(
            "Received sig share from peer {} for issuance {}:{}",
            peer, req_id.0, req_id.1
        );
        batch.append_insert_new(
            PartialSignatureKey {
                request_id: req_id,
                peer_id: peer,
            },
            BincodeSerialized::owned(partial_sig),
        );
        batch.commit();
    }

    fn finalize_signatures(&self, mut batch: BatchTx) {
        let req_psigs = self
            .db
            .find_by_prefix::<_, PartialSignatureKey, BincodeSerialized<PartialSigResponse>>(
                &AllPartialSignaturesKey,
            )
            .map(|entry_res| {
                let (key, value) = entry_res.expect("DB error");
                (key.request_id, (key.peer_id as usize, value.into_owned()))
            })
            .into_group_map();

        // TODO: use own par iter impl that allows efficient use of accumulators
        let par_batches = req_psigs
            .into_par_iter()
            .filter_map(|(issuance_id, shares)| {
                let mut batch = DbBatch::new();
                let mut batch_tx = batch.transaction();

                if shares.len() > self.tbs_threshold() {
                    debug!(
                        "Trying to combine sig shares for issuance request {}:{}",
                        issuance_id.0, issuance_id.1
                    );
                    let (bsig, errors) = self.mint.combine(shares.clone());
                    // FIXME: validate shares before writing to DB to make combine infallible
                    if !errors.0.is_empty() {
                        warn!("Peer sent faulty share: {:?}", errors);
                    }

                    match bsig {
                        Ok(blind_signature) => {
                            debug!(
                                "Successfully combined signature shares for issuance request {}:{}",
                                issuance_id.0, issuance_id.1
                            );

                            // FIXME: probably remove this deduplication logic, otherwise fix/replace
                            batch_tx.append_from_iter(
                                self.db
                                    .find_by_prefix::<_, ConsensusItem, ()>(
                                        &ConsensusItemKeyPrefix(issuance_id.0),
                                    )
                                    .map(|res| {
                                        let key = res.expect("DB error").0;
                                        BatchItem::delete(key)
                                    }),
                            );

                            batch_tx.append_from_iter(shares.into_iter().map(|(peer, _)| {
                                BatchItem::delete(PartialSignatureKey {
                                    request_id: issuance_id,
                                    peer_id: peer as u16,
                                })
                            }));

                            let sig_key = TransactionOutputOutcomeKey(issuance_id.0, issuance_id.1);
                            let sig_value = BincodeSerialized::owned(Some(OutputOutcome::Coins {
                                blind_signature,
                            }));
                            batch_tx.append_insert(sig_key, sig_value);
                            batch_tx.commit();
                            Some(batch)
                        }
                        Err(e) => {
                            error!("Could not combine shares: {}", e);
                            None
                        }
                    }
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        batch.append_from_accumulators(par_batches.into_iter());
        batch.commit();
    }

    fn tbs_threshold(&self) -> usize {
        self.cfg.peers.len() - self.cfg.max_faulty() - 1
    }
}

fn to_sign_request(coins: Coins<BlindToken>) -> SignRequest {
    SignRequest(
        coins
            .into_iter()
            .map(|(amt, token)| (amt, token.0))
            .collect(),
    )
}

fn to_btc_amount_round_down(amt: Amount) -> bitcoin::Amount {
    bitcoin::Amount::from_sat(amt.milli_sat / 1000)
}

#[derive(Debug, Error)]
pub enum TransactionSubmissionError {
    #[error("High level transaction error: {0}")]
    TransactionError(TransactionError),
    #[error("Input coin error: {0}")]
    InputCoinError(MintError),
    #[error("Input peg-in error: {0}")]
    InputPegIn(WalletError),
    #[error("Output coin error: {0}")]
    OutputCoinError(MintError),
    #[error("Output coin error: {0}")]
    OutputPegOut(WalletError),
}

impl From<TransactionError> for TransactionSubmissionError {
    fn from(e: TransactionError) -> Self {
        TransactionSubmissionError::TransactionError(e)
    }
}
