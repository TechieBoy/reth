use crate::{
    providers::state::{historical::HistoricalStateProvider, latest::LatestStateProvider},
    traits::{BlockSource, ReceiptProvider},
    BlockHashProvider, BlockNumProvider, BlockProvider, EvmEnvProvider, HeaderProvider,
    ProviderError, StageCheckpointProvider, StateProviderBox, TransactionsProvider,
    WithdrawalsProvider,
};
use reth_db::{cursor::DbCursorRO, database::Database, tables, transaction::DbTx};
use reth_interfaces::Result;
use reth_primitives::{
    stage::{StageCheckpoint, StageId},
    Block, BlockHash, BlockHashOrNumber, BlockNumber, ChainInfo, ChainSpec, Head, Header, Receipt,
    SealedBlock, SealedHeader, TransactionMeta, TransactionSigned, TxHash, TxNumber, Withdrawal,
    H256, U256,
};
use reth_revm_primitives::{
    config::revm_spec,
    env::{fill_block_env, fill_cfg_and_block_env, fill_cfg_env},
    primitives::{BlockEnv, CfgEnv, SpecId},
};
use std::{ops::RangeBounds, sync::Arc};
use tracing::trace;

/// A common provider that fetches data from a database.
///
/// This provider implements most provider or provider factory traits.
#[derive(Debug)]
pub struct ShareableDatabase<DB> {
    /// Database
    db: DB,
    /// Chain spec
    chain_spec: Arc<ChainSpec>,
}

impl<DB> ShareableDatabase<DB> {
    /// create new database provider
    pub fn new(db: DB, chain_spec: Arc<ChainSpec>) -> Self {
        Self { db, chain_spec }
    }
}

impl<DB: Clone> Clone for ShareableDatabase<DB> {
    fn clone(&self) -> Self {
        Self { db: self.db.clone(), chain_spec: Arc::clone(&self.chain_spec) }
    }
}

impl<DB: Database> ShareableDatabase<DB> {
    /// Storage provider for latest block
    pub fn latest(&self) -> Result<StateProviderBox<'_>> {
        trace!(target: "providers::db", "Returning latest state provider");
        Ok(Box::new(LatestStateProvider::new(self.db.tx()?)))
    }

    /// Storage provider for state at that given block
    pub fn history_by_block_number(
        &self,
        mut block_number: BlockNumber,
    ) -> Result<StateProviderBox<'_>> {
        let tx = self.db.tx()?;

        if is_latest_block_number(&tx, block_number)? {
            return Ok(Box::new(LatestStateProvider::new(tx)))
        }

        // +1 as the changeset that we want is the one that was applied after this block.
        block_number += 1;

        trace!(target: "providers::db", ?block_number, "Returning historical state provider for block number");
        Ok(Box::new(HistoricalStateProvider::new(tx, block_number)))
    }

    /// Storage provider for state at that given block hash
    pub fn history_by_block_hash(&self, block_hash: BlockHash) -> Result<StateProviderBox<'_>> {
        let tx = self.db.tx()?;
        // get block number
        let mut block_number = tx
            .get::<tables::HeaderNumbers>(block_hash)?
            .ok_or(ProviderError::BlockHashNotFound(block_hash))?;

        if is_latest_block_number(&tx, block_number)? {
            return Ok(Box::new(LatestStateProvider::new(tx)))
        }

        // +1 as the changeset that we want is the one that was applied after this block.
        // as the  changeset contains old values.
        block_number += 1;

        trace!(target: "providers::db", ?block_hash, "Returning historical state provider for block hash");
        Ok(Box::new(HistoricalStateProvider::new(tx, block_number)))
    }

    /// Reads the block's ommers blocks and withdrawals.
    ///
    /// Note: these are mutually exclusive, after shanghai, this only returns withdrawals. Before
    /// shanghai, this only returns ommers.
    #[allow(clippy::type_complexity)]
    fn read_block_ommers_and_withdrawals<'a, TX>(
        &self,
        tx: &TX,
        block_number: u64,
        timestamp: u64,
    ) -> std::result::Result<
        (Option<Vec<Header>>, Option<Vec<Withdrawal>>),
        reth_interfaces::db::DatabaseError,
    >
    where
        TX: DbTx<'a> + Send + Sync,
    {
        let mut ommers = None;
        let mut withdrawals = None;
        if self.chain_spec.is_shanghai_activated_at_timestamp(timestamp) {
            withdrawals = read_withdrawals_by_number(tx, block_number)?;
        } else {
            ommers = tx.get::<tables::BlockOmmers>(block_number)?.map(|o| o.ommers);
        }
        Ok((ommers, withdrawals))
    }
}

impl<DB: Database> HeaderProvider for ShareableDatabase<DB> {
    fn header(&self, block_hash: &BlockHash) -> Result<Option<Header>> {
        self.db.view(|tx| {
            if let Some(num) = tx.get::<tables::HeaderNumbers>(*block_hash)? {
                Ok(tx.get::<tables::Headers>(num)?)
            } else {
                Ok(None)
            }
        })?
    }

    fn header_by_number(&self, num: BlockNumber) -> Result<Option<Header>> {
        Ok(self.db.view(|tx| tx.get::<tables::Headers>(num))??)
    }

    fn header_td(&self, hash: &BlockHash) -> Result<Option<U256>> {
        self.db.view(|tx| {
            if let Some(num) = tx.get::<tables::HeaderNumbers>(*hash)? {
                Ok(tx.get::<tables::HeaderTD>(num)?.map(|td| td.0))
            } else {
                Ok(None)
            }
        })?
    }

    fn header_td_by_number(&self, number: BlockNumber) -> Result<Option<U256>> {
        self.db.view(|tx| Ok(tx.get::<tables::HeaderTD>(number)?.map(|td| td.0)))?
    }

    fn headers_range(&self, range: impl RangeBounds<BlockNumber>) -> Result<Vec<Header>> {
        self.db
            .view(|tx| {
                let mut cursor = tx.cursor_read::<tables::Headers>()?;
                cursor
                    .walk_range(range)?
                    .map(|result| result.map(|(_, header)| header).map_err(Into::into))
                    .collect::<Result<Vec<_>>>()
            })?
            .map_err(Into::into)
    }

    fn sealed_headers_range(
        &self,
        range: impl RangeBounds<BlockNumber>,
    ) -> Result<Vec<SealedHeader>> {
        self.db
            .view(|tx| -> Result<_> {
                let mut headers = vec![];
                for entry in tx.cursor_read::<tables::Headers>()?.walk_range(range)? {
                    let (num, header) = entry?;
                    let hash = read_header_hash(tx, num)?;
                    headers.push(header.seal(hash));
                }
                Ok(headers)
            })?
            .map_err(Into::into)
    }

    fn sealed_header(&self, number: BlockNumber) -> Result<Option<SealedHeader>> {
        self.db
            .view(|tx| -> Result<_> {
                if let Some(header) = tx.get::<tables::Headers>(number)? {
                    let hash = read_header_hash(tx, number)?;
                    Ok(Some(header.seal(hash)))
                } else {
                    Ok(None)
                }
            })?
            .map_err(Into::into)
    }
}

impl<DB: Database> BlockHashProvider for ShareableDatabase<DB> {
    fn block_hash(&self, number: u64) -> Result<Option<H256>> {
        self.db.view(|tx| tx.get::<tables::CanonicalHeaders>(number))?.map_err(Into::into)
    }

    fn canonical_hashes_range(&self, start: BlockNumber, end: BlockNumber) -> Result<Vec<H256>> {
        let range = start..end;
        self.db
            .view(|tx| {
                let mut cursor = tx.cursor_read::<tables::CanonicalHeaders>()?;
                cursor
                    .walk_range(range)?
                    .map(|result| result.map(|(_, hash)| hash).map_err(Into::into))
                    .collect::<Result<Vec<_>>>()
            })?
            .map_err(Into::into)
    }
}

impl<DB: Database> BlockNumProvider for ShareableDatabase<DB> {
    fn chain_info(&self) -> Result<ChainInfo> {
        let best_number = self.best_block_number()?;
        let best_hash = self.block_hash(best_number)?.unwrap_or_default();
        Ok(ChainInfo { best_hash, best_number })
    }

    fn best_block_number(&self) -> Result<BlockNumber> {
        Ok(self.db.view(|tx| best_block_number(tx))??.unwrap_or_default())
    }

    fn block_number(&self, hash: H256) -> Result<Option<BlockNumber>> {
        self.db.view(|tx| read_block_number(tx, hash))?.map_err(Into::into)
    }
}

impl<DB: Database> BlockProvider for ShareableDatabase<DB> {
    fn find_block_by_hash(&self, hash: H256, source: BlockSource) -> Result<Option<Block>> {
        if source.is_database() {
            self.block(hash.into())
        } else {
            Ok(None)
        }
    }

    fn block(&self, id: BlockHashOrNumber) -> Result<Option<Block>> {
        let tx = self.db.tx()?;
        if let Some(number) = convert_hash_or_number(&tx, id)? {
            if let Some(header) = read_header(&tx, number)? {
                // we check for shanghai first
                let (ommers, withdrawals) =
                    self.read_block_ommers_and_withdrawals(&tx, number, header.timestamp)?;

                let transactions = read_transactions_by_number(&tx, number)?
                    .ok_or(ProviderError::BlockBodyIndicesNotFound(number))?;

                return Ok(Some(Block {
                    header,
                    body: transactions,
                    ommers: ommers.unwrap_or_default(),
                    withdrawals,
                }))
            }
        }

        Ok(None)
    }

    fn pending_block(&self) -> Result<Option<SealedBlock>> {
        Ok(None)
    }

    fn ommers(&self, id: BlockHashOrNumber) -> Result<Option<Vec<Header>>> {
        let tx = self.db.tx()?;
        if let Some(number) = convert_hash_or_number(&tx, id)? {
            // TODO: this can be optimized to return empty Vec post-merge
            let ommers = tx.get::<tables::BlockOmmers>(number)?.map(|o| o.ommers);
            return Ok(ommers)
        }

        Ok(None)
    }
}

impl<DB: Database> TransactionsProvider for ShareableDatabase<DB> {
    fn transaction_id(&self, tx_hash: TxHash) -> Result<Option<TxNumber>> {
        self.db.view(|tx| tx.get::<tables::TxHashNumber>(tx_hash))?.map_err(Into::into)
    }

    fn transaction_by_id(&self, id: TxNumber) -> Result<Option<TransactionSigned>> {
        self.db
            .view(|tx| tx.get::<tables::Transactions>(id))?
            .map_err(Into::into)
            .map(|tx| tx.map(Into::into))
    }

    fn transaction_by_hash(&self, hash: TxHash) -> Result<Option<TransactionSigned>> {
        self.db
            .view(|tx| {
                if let Some(id) = tx.get::<tables::TxHashNumber>(hash)? {
                    tx.get::<tables::Transactions>(id)
                } else {
                    Ok(None)
                }
            })?
            .map_err(Into::into)
            .map(|tx| tx.map(Into::into))
    }

    fn transaction_by_hash_with_meta(
        &self,
        tx_hash: TxHash,
    ) -> Result<Option<(TransactionSigned, TransactionMeta)>> {
        self.db
            .view(|tx| -> Result<_> {
                if let Some(transaction_id) = tx.get::<tables::TxHashNumber>(tx_hash)? {
                    if let Some(transaction) = tx.get::<tables::Transactions>(transaction_id)? {
                        let mut transaction_cursor =
                            tx.cursor_read::<tables::TransactionBlock>()?;
                        if let Some(block_number) =
                            transaction_cursor.seek(transaction_id).map(|b| b.map(|(_, bn)| bn))?
                        {
                            if let Some((header, block_hash)) =
                                read_sealed_header(tx, block_number)?
                            {
                                if let Some(block_body) =
                                    tx.get::<tables::BlockBodyIndices>(block_number)?
                                {
                                    // the index of the tx in the block is the offset:
                                    // len([start..tx_id])
                                    // SAFETY: `transaction_id` is always `>=` the block's first
                                    // index
                                    let index = transaction_id - block_body.first_tx_num();

                                    let meta = TransactionMeta {
                                        tx_hash,
                                        index,
                                        block_hash,
                                        block_number,
                                        base_fee: header.base_fee_per_gas,
                                    };

                                    return Ok(Some((transaction.into(), meta)))
                                }
                            }
                        }
                    }
                }

                Ok(None)
            })?
            .map_err(Into::into)
    }

    fn transaction_block(&self, id: TxNumber) -> Result<Option<BlockNumber>> {
        self.db
            .view(|tx| {
                let mut cursor = tx.cursor_read::<tables::TransactionBlock>()?;
                cursor.seek(id).map(|b| b.map(|(_, bn)| bn))
            })?
            .map_err(Into::into)
    }

    fn transactions_by_block(
        &self,
        id: BlockHashOrNumber,
    ) -> Result<Option<Vec<TransactionSigned>>> {
        let tx = self.db.tx()?;
        if let Some(number) = convert_hash_or_number(&tx, id)? {
            return Ok(read_transactions_by_number(&tx, number)?)
        }
        Ok(None)
    }

    fn transactions_by_block_range(
        &self,
        range: impl RangeBounds<BlockNumber>,
    ) -> Result<Vec<Vec<TransactionSigned>>> {
        let tx = self.db.tx()?;
        let mut results = Vec::default();
        let mut body_cursor = tx.cursor_read::<tables::BlockBodyIndices>()?;
        let mut tx_cursor = tx.cursor_read::<tables::Transactions>()?;
        for entry in body_cursor.walk_range(range)? {
            let (_, body) = entry?;
            let tx_num_range = body.tx_num_range();
            if tx_num_range.is_empty() {
                results.push(Vec::default());
            } else {
                results.push(
                    tx_cursor
                        .walk_range(tx_num_range)?
                        .map(|result| result.map(|(_, tx)| tx.into()))
                        .collect::<std::result::Result<Vec<_>, _>>()?,
                );
            }
        }
        Ok(results)
    }
}

impl<DB: Database> ReceiptProvider for ShareableDatabase<DB> {
    fn receipt(&self, id: TxNumber) -> Result<Option<Receipt>> {
        self.db.view(|tx| tx.get::<tables::Receipts>(id))?.map_err(Into::into)
    }

    fn receipt_by_hash(&self, hash: TxHash) -> Result<Option<Receipt>> {
        self.db
            .view(|tx| {
                if let Some(id) = tx.get::<tables::TxHashNumber>(hash)? {
                    tx.get::<tables::Receipts>(id)
                } else {
                    Ok(None)
                }
            })?
            .map_err(Into::into)
    }

    fn receipts_by_block(&self, block: BlockHashOrNumber) -> Result<Option<Vec<Receipt>>> {
        let tx = self.db.tx()?;
        if let Some(number) = convert_hash_or_number(&tx, block)? {
            if let Some(body) = tx.get::<tables::BlockBodyIndices>(number)? {
                let tx_range = body.tx_num_range();
                return if tx_range.is_empty() {
                    Ok(Some(Vec::new()))
                } else {
                    let mut tx_cursor = tx.cursor_read::<tables::Receipts>()?;
                    let transactions = tx_cursor
                        .walk_range(tx_range)?
                        .map(|result| result.map(|(_, tx)| tx))
                        .collect::<std::result::Result<Vec<_>, _>>()?;
                    Ok(Some(transactions))
                }
            }
        }
        Ok(None)
    }
}

impl<DB: Database> WithdrawalsProvider for ShareableDatabase<DB> {
    fn withdrawals_by_block(
        &self,
        id: BlockHashOrNumber,
        timestamp: u64,
    ) -> Result<Option<Vec<Withdrawal>>> {
        if self.chain_spec.is_shanghai_activated_at_timestamp(timestamp) {
            let tx = self.db.tx()?;
            if let Some(number) = convert_hash_or_number(&tx, id)? {
                // If we are past shanghai, then all blocks should have a withdrawal list, even if
                // empty
                let withdrawals = read_withdrawals_by_number(&tx, number)?.unwrap_or_default();
                return Ok(Some(withdrawals))
            }
        }
        Ok(None)
    }

    fn latest_withdrawal(&self) -> Result<Option<Withdrawal>> {
        let latest_block_withdrawal =
            self.db.view(|tx| tx.cursor_read::<tables::BlockWithdrawals>()?.last())?;
        latest_block_withdrawal
            .map(|block_withdrawal_pair| {
                block_withdrawal_pair
                    .and_then(|(_, block_withdrawal)| block_withdrawal.withdrawals.last().cloned())
            })
            .map_err(Into::into)
    }
}

impl<DB: Database> StageCheckpointProvider for ShareableDatabase<DB> {
    fn get_stage_checkpoint(&self, id: StageId) -> Result<Option<StageCheckpoint>> {
        Ok(get_stage_checkpoint(&self.db.tx()?, id)?)
    }
}

impl<DB: Database> EvmEnvProvider for ShareableDatabase<DB> {
    fn fill_env_at(
        &self,
        cfg: &mut CfgEnv,
        block_env: &mut BlockEnv,
        at: BlockHashOrNumber,
    ) -> Result<()> {
        let hash = self.convert_number(at)?.ok_or(ProviderError::HeaderNotFound(at))?;
        let header = self.header(&hash)?.ok_or(ProviderError::HeaderNotFound(at))?;
        self.fill_env_with_header(cfg, block_env, &header)
    }

    fn fill_env_with_header(
        &self,
        cfg: &mut CfgEnv,
        block_env: &mut BlockEnv,
        header: &Header,
    ) -> Result<()> {
        let total_difficulty = self
            .header_td_by_number(header.number)?
            .ok_or_else(|| ProviderError::HeaderNotFound(header.number.into()))?;
        fill_cfg_and_block_env(cfg, block_env, &self.chain_spec, header, total_difficulty);
        Ok(())
    }

    fn fill_block_env_at(&self, block_env: &mut BlockEnv, at: BlockHashOrNumber) -> Result<()> {
        let hash = self.convert_number(at)?.ok_or(ProviderError::HeaderNotFound(at))?;
        let header = self.header(&hash)?.ok_or(ProviderError::HeaderNotFound(at))?;

        self.fill_block_env_with_header(block_env, &header)
    }

    fn fill_block_env_with_header(&self, block_env: &mut BlockEnv, header: &Header) -> Result<()> {
        let total_difficulty = self
            .header_td_by_number(header.number)?
            .ok_or_else(|| ProviderError::HeaderNotFound(header.number.into()))?;
        let spec_id = revm_spec(
            &self.chain_spec,
            Head {
                number: header.number,
                timestamp: header.timestamp,
                difficulty: header.difficulty,
                total_difficulty,
                // Not required
                hash: Default::default(),
            },
        );
        let after_merge = spec_id >= SpecId::MERGE;
        fill_block_env(block_env, &self.chain_spec, header, after_merge);
        Ok(())
    }

    fn fill_cfg_env_at(&self, cfg: &mut CfgEnv, at: BlockHashOrNumber) -> Result<()> {
        let hash = self.convert_number(at)?.ok_or(ProviderError::HeaderNotFound(at))?;
        let header = self.header(&hash)?.ok_or(ProviderError::HeaderNotFound(at))?;
        self.fill_cfg_env_with_header(cfg, &header)
    }

    fn fill_cfg_env_with_header(&self, cfg: &mut CfgEnv, header: &Header) -> Result<()> {
        let total_difficulty = self
            .header_td_by_number(header.number)?
            .ok_or_else(|| ProviderError::HeaderNotFound(header.number.into()))?;
        fill_cfg_env(cfg, &self.chain_spec, header, total_difficulty);
        Ok(())
    }
}

/// Returns the block number for the given block hash or number.
#[inline]
fn convert_hash_or_number<'a, TX>(
    tx: &TX,
    block: BlockHashOrNumber,
) -> std::result::Result<Option<BlockNumber>, reth_interfaces::db::DatabaseError>
where
    TX: DbTx<'a> + Send + Sync,
{
    match block {
        BlockHashOrNumber::Hash(hash) => read_block_number(tx, hash),
        BlockHashOrNumber::Number(number) => Ok(Some(number)),
    }
}

/// Reads the number for the given block hash.
#[inline]
fn read_block_number<'a, TX>(
    tx: &TX,
    hash: H256,
) -> std::result::Result<Option<BlockNumber>, reth_interfaces::db::DatabaseError>
where
    TX: DbTx<'a> + Send + Sync,
{
    tx.get::<tables::HeaderNumbers>(hash)
}

/// Reads the hash for the given block number
///
/// Returns an error if no matching entry is found.
#[inline]
fn read_header_hash<'a, TX>(
    tx: &TX,
    number: u64,
) -> std::result::Result<BlockHash, reth_interfaces::Error>
where
    TX: DbTx<'a> + Send + Sync,
{
    match tx.get::<tables::CanonicalHeaders>(number)? {
        Some(hash) => Ok(hash),
        None => Err(ProviderError::HeaderNotFound(number.into()).into()),
    }
}

/// Fetches the Withdrawals that belong to the given block number
#[inline]
fn read_transactions_by_number<'a, TX>(
    tx: &TX,
    block_number: u64,
) -> std::result::Result<Option<Vec<TransactionSigned>>, reth_interfaces::db::DatabaseError>
where
    TX: DbTx<'a> + Send + Sync,
{
    if let Some(body) = tx.get::<tables::BlockBodyIndices>(block_number)? {
        let tx_range = body.tx_num_range();
        return if tx_range.is_empty() {
            Ok(Some(Vec::new()))
        } else {
            let mut tx_cursor = tx.cursor_read::<tables::Transactions>()?;
            let transactions = tx_cursor
                .walk_range(tx_range)?
                .map(|result| result.map(|(_, tx)| tx.into()))
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(Some(transactions))
        }
    }

    Ok(None)
}

/// Fetches the Withdrawals that belong to the given block number
#[inline]
fn read_withdrawals_by_number<'a, TX>(
    tx: &TX,
    block_number: u64,
) -> std::result::Result<Option<Vec<Withdrawal>>, reth_interfaces::db::DatabaseError>
where
    TX: DbTx<'a> + Send + Sync,
{
    tx.get::<tables::BlockWithdrawals>(block_number).map(|w| w.map(|w| w.withdrawals))
}

/// Fetches the corresponding header
#[inline]
fn read_header<'a, TX>(
    tx: &TX,
    block_number: u64,
) -> std::result::Result<Option<Header>, reth_interfaces::db::DatabaseError>
where
    TX: DbTx<'a> + Send + Sync,
{
    tx.get::<tables::Headers>(block_number)
}

/// Fetches Header and its hash
#[inline]
fn read_sealed_header<'a, TX>(
    tx: &TX,
    block_number: u64,
) -> std::result::Result<Option<(Header, BlockHash)>, reth_interfaces::db::DatabaseError>
where
    TX: DbTx<'a> + Send + Sync,
{
    let block_hash = match tx.get::<tables::CanonicalHeaders>(block_number)? {
        Some(block_hash) => block_hash,
        None => return Ok(None),
    };
    match read_header(tx, block_number)? {
        Some(header) => Ok(Some((header, block_hash))),
        None => Ok(None),
    }
}

/// Fetches checks if the block number is the latest block number.
#[inline]
fn is_latest_block_number<'a, TX>(
    tx: &TX,
    block_number: BlockNumber,
) -> std::result::Result<bool, reth_interfaces::db::DatabaseError>
where
    TX: DbTx<'a> + Send + Sync,
{
    // check if the block number is the best block number
    // there's always at least one header in the database (genesis)
    let best = best_block_number(tx)?.unwrap_or_default();
    let last = last_canonical_header(tx)?.map(|(last, _)| last).unwrap_or_default();
    Ok(block_number == best && block_number == last)
}

/// Fetches the best block number from the database.
#[inline]
fn best_block_number<'a, TX>(
    tx: &TX,
) -> std::result::Result<Option<BlockNumber>, reth_interfaces::db::DatabaseError>
where
    TX: DbTx<'a> + Send + Sync,
{
    tx.get::<tables::SyncStage>("Finish".to_string()) // TODO:
        .map(|result| result.map(|checkpoint| checkpoint.block_number))
}

/// Fetches the last canonical header from the database.
#[inline]
fn last_canonical_header<'a, TX>(
    tx: &TX,
) -> std::result::Result<Option<(BlockNumber, BlockHash)>, reth_interfaces::db::DatabaseError>
where
    TX: DbTx<'a> + Send + Sync,
{
    tx.cursor_read::<tables::CanonicalHeaders>()?.last()
}

/// Get checkpoint for the given stage.
#[inline]
pub fn get_stage_checkpoint<'a, TX>(
    tx: &TX,
    id: StageId,
) -> std::result::Result<Option<StageCheckpoint>, reth_interfaces::db::DatabaseError>
where
    TX: DbTx<'a> + Send + Sync,
{
    tx.get::<tables::SyncStage>(id.to_string())
}

#[cfg(test)]
mod tests {
    use super::ShareableDatabase;
    use crate::BlockNumProvider;
    use reth_db::mdbx::{test_utils::create_test_db, EnvKind, WriteMap};
    use reth_primitives::{ChainSpecBuilder, H256};
    use std::sync::Arc;

    #[test]
    fn common_history_provider() {
        let chain_spec = ChainSpecBuilder::mainnet().build();
        let db = create_test_db::<WriteMap>(EnvKind::RW);
        let provider = ShareableDatabase::new(db, Arc::new(chain_spec));
        let _ = provider.latest();
    }

    #[test]
    fn default_chain_info() {
        let chain_spec = ChainSpecBuilder::mainnet().build();
        let db = create_test_db::<WriteMap>(EnvKind::RW);
        let provider = ShareableDatabase::new(db, Arc::new(chain_spec));

        let chain_info = provider.chain_info().expect("should be ok");
        assert_eq!(chain_info.best_number, 0);
        assert_eq!(chain_info.best_hash, H256::zero());
    }
}
