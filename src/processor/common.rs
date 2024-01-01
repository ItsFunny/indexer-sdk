use crate::client::event::ClientEvent;
use crate::dispatcher::event::DispatchEvent;
use crate::error::IndexerResult;
use crate::event::{AddressType, BalanceType, IndexerEvent, TxIdType};
use crate::storage::prefix::DeltaStatus;
use crate::storage::StorageProcessor;
use crate::types::delta::TransactionDelta;
use crate::{Component, HookComponent, IndexProcessor};
use bitcoincore_rpc::bitcoin::consensus::{deserialize, serialize};
use bitcoincore_rpc::bitcoin::{Transaction, Txid};
use bitcoincore_rpc::RpcApi;
use log::{error, info};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use wg::AsyncWaitGroup;

#[derive(Clone)]
pub struct IndexerProcessorImpl<T: StorageProcessor> {
    tx: async_channel::Sender<ClientEvent>,
    storage: T,
    btc_client: Arc<bitcoincore_rpc::Client>,

    flag: Arc<AtomicBool>,
    wg: AsyncWaitGroup,
}

unsafe impl<T: StorageProcessor> Send for IndexerProcessorImpl<T> {}

unsafe impl<T: StorageProcessor> Sync for IndexerProcessorImpl<T> {}

impl<T: StorageProcessor> IndexerProcessorImpl<T> {
    pub fn new(
        wg: AsyncWaitGroup,
        tx: async_channel::Sender<ClientEvent>,
        storage: T,
        client: Arc<bitcoincore_rpc::Client>,
        flag: Arc<AtomicBool>,
    ) -> Self {
        Self {
            tx,
            storage,
            btc_client: client,
            flag,
            wg,
        }
    }
}

#[async_trait::async_trait]
impl<T: StorageProcessor> HookComponent<DispatchEvent> for IndexerProcessorImpl<T> {
    async fn before_start(
        &mut self,
        sender: async_channel::Sender<DispatchEvent>,
        _: async_channel::Receiver<DispatchEvent>,
    ) -> IndexerResult<()> {
        self.wg.wait().await;
        self.restore_from_mempool(sender).await?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl<T: StorageProcessor> Component<DispatchEvent> for IndexerProcessorImpl<T> {
    async fn handle_event(&mut self, event: &DispatchEvent) -> IndexerResult<()> {
        let event = event.get_indexer_event().unwrap();
        if let Err(e) = self.do_handle_event(event).await {
            error!("handle_event error:{:?}", e)
        }
        Ok(())
    }

    async fn interest(&self, event: &DispatchEvent) -> bool {
        event.get_indexer_event().is_some()
    }
}

impl<T: StorageProcessor> IndexerProcessorImpl<T> {
    async fn restore_from_mempool(
        &mut self,
        sender: async_channel::Sender<DispatchEvent>,
    ) -> IndexerResult<()> {
        self.do_handle_sync_mempool(sender).await?;
        Ok(())
    }

    async fn do_handle_sync_mempool(
        &mut self,
        tx: async_channel::Sender<DispatchEvent>,
    ) -> IndexerResult<()> {
        let all_unconsumed = self.storage.get_all_un_consumed_txs().await?;
        info!("all unconsumed txs:{:?}", all_unconsumed);
        let txs = {
            // sort by timestamp to execute tx in order
            let txs = self.btc_client.get_raw_mempool_verbose()?;
            let mut append = vec![];
            for (k, ts) in &all_unconsumed {
                let tx_id: Txid = k.clone().into();
                if !txs.contains_key(&tx_id) {
                    append.push((k.clone(), *ts));
                }
            }
            let mut sorted_pairs: Vec<_> = txs
                .into_iter()
                .map(|(tx_id, info)| {
                    let tx_id: TxIdType = tx_id.into();
                    (tx_id, info.time as i64)
                })
                .collect();
            sorted_pairs.extend_from_slice(append.as_slice());
            sorted_pairs.sort_by(|a, b| a.1.cmp(&b.1));
            sorted_pairs
        };

        for (tx_id, _) in txs {
            info!("get tx from mempool or db:{:?}", &tx_id);
            tx.send(DispatchEvent::IndexerEvent(
                IndexerEvent::TxFromRestoreByTxId(tx_id),
            ))
            .await
            .unwrap();
        }
        self.flag.store(true, Ordering::Relaxed);

        Ok(())
    }
    async fn do_handle_event(&mut self, event: &IndexerEvent) -> IndexerResult<()> {
        info!("do_handle_event,event:{:?}", event);
        match event {
            IndexerEvent::NewTxComing(data, _) => {
                self.do_handle_new_tx_coming(data, false).await?;
            }
            IndexerEvent::GetBalance(address, tx) => {
                self.do_handle_get_balance(address, tx).await?;
            }
            IndexerEvent::UpdateDelta(data) => {
                self.do_handle_update_delta(data).await?;
            }
            IndexerEvent::TxConfirmed(tx_id) => {
                self.do_handle_tx_confirmed(tx_id, DeltaStatus::Confirmed)
                    .await?;
            }
            IndexerEvent::TxFromRestoreByTxId(tx_id) => {
                self.do_handle_restore_tx_by_tx_id(tx_id).await?;
            }
            IndexerEvent::TxRemoved(tx_id) => {
                self.do_handle_tx_removed(tx_id).await?;
            }
            IndexerEvent::ReportHeight(_) => {}
            IndexerEvent::ReportReorg(ts) => {
                self.do_handle_report_reorg(ts).await?;
            }
        }
        Ok(())
    }

    // force_dispatch:true: data from restore
    pub(crate) async fn do_handle_new_tx_coming(
        &mut self,
        data: &Vec<u8>,
        from_restore: bool,
    ) -> IndexerResult<()> {
        let data = self.parse_zmq_data(&data);
        if let Some((tx_id, tx)) = data {
            let seen = self.storage.seen_and_store_txs(&tx).await?;
            if seen.is_seen() {
                if from_restore {
                    if seen.is_executed() {
                        info!("tx_id:{:?} is seen and  has been executed,skip", tx_id);
                        return Ok(());
                    } else {
                        info!(
                            "tx_id:{:?} from restore  is seen but  has not been executed,start to dispatch",
                            tx_id
                        );
                    }
                } else {
                    info!("tx_id:{:?} has been seen,skip", tx_id);
                    return Ok(());
                }
            } else {
                info!("tx_id:{:?} has not been executed,start to dispatch", tx_id);
            }
            self.tx.send(ClientEvent::Transaction(tx)).await.unwrap();
        }
        Ok(())
    }
    fn parse_zmq_data(&self, data: &Vec<u8>) -> Option<(TxIdType, Transaction)> {
        let tx: Transaction = deserialize(&data).expect("Failed to deserialize transaction");
        Some((tx.txid().into(), tx))
    }

    pub(crate) async fn do_handle_get_balance(
        &self,
        _: &AddressType,
        _: &crossbeam::channel::Sender<BalanceType>,
    ) -> IndexerResult<()> {
        todo!()
    }

    async fn do_handle_update_delta(&mut self, data: &TransactionDelta) -> IndexerResult<()> {
        self.storage.add_transaction_delta(data).await?;
        Ok(())
    }
    async fn do_handle_tx_confirmed(
        &mut self,
        tx_id: &TxIdType,
        status: DeltaStatus,
    ) -> IndexerResult<()> {
        self.storage.remove_transaction_delta(tx_id, status).await?;
        Ok(())
    }
    async fn do_handle_restore_tx_by_tx_id(&mut self, tx_id: &TxIdType) -> IndexerResult<()> {
        let txid: Txid = tx_id.clone().into();
        info!("do_handle_force_tx_by_tx_id,txid:{:?}", txid);
        let transaction = self.btc_client.get_raw_transaction(&txid, None)?;
        let data = serialize(&transaction);
        self.do_handle_new_tx_coming(&data, true).await?;

        Ok(())
    }
    async fn do_handle_tx_removed(&mut self, tx_id: &TxIdType) -> IndexerResult<()> {
        self.do_handle_tx_confirmed(tx_id, DeltaStatus::InActive)
            .await?;
        self.tx
            .send(ClientEvent::TxDroped(tx_id.clone()))
            .await
            .unwrap();
        Ok(())
    }
    async fn do_handle_report_reorg(&mut self, txs: &Vec<TxIdType>) -> IndexerResult<()> {
        for tx_id in txs {
            if let Err(e) = self.do_handle_tx_removed(tx_id).await {
                error!("do_handle_report_reorg error:{:?},txid:{:?}", e, tx_id);
            }
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl<T: StorageProcessor> IndexProcessor<DispatchEvent> for IndexerProcessorImpl<T> {}
