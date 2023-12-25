use core::arch;
use std::{panic, thread};
use std::process::exit;
use std::sync::Arc;
use log::error;
use tokio::runtime::Runtime;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use crate::component::zmq::component::ZeroMQComponent;
use crate::{Component, ComponentTemplate};
use crate::configuration::base::IndexerConfiguration;
use crate::notifier::common::CommonNotifier;
use crate::processor::common::IndexerProcessorImpl;
use crate::storage::memory::MemoryStorageProcessor;


pub async fn async_create_and_start_processor(origin_exit: watch::Receiver<()>, origin_cfg: IndexerConfiguration) -> (CommonNotifier, Vec<JoinHandle<()>>) {
    panic::set_hook(Box::new(|panic_info| {
        println!("panic occurred: {:?}", panic_info);
        error!("panic occurred: {:?}", panic_info);
        exit(-1);
    }));
    let (notify_tx, notify_rx) = crossbeam::channel::unbounded();
    let default_memory_storage = Arc::new(Box::new(MemoryStorageProcessor::default()));
    let mut processor_wrapper = ComponentTemplate::new(IndexerProcessorImpl::new(notify_tx.clone(), default_memory_storage));
    let indexer_tx = processor_wrapper.event_tx().unwrap();

    let mut ret = vec![];
    processor_wrapper.init(origin_cfg.clone()).await.unwrap();
    ret.extend(processor_wrapper.start(origin_exit.clone()).await.unwrap());

    let mut zmq_wrapper = ComponentTemplate::new(ZeroMQComponent::new(origin_cfg.clone(), indexer_tx.clone()));
    zmq_wrapper.init(origin_cfg.clone()).await.unwrap();
    ret.extend(zmq_wrapper.start(origin_exit.clone()).await.unwrap());

    (CommonNotifier::new(notify_rx.clone(), indexer_tx.clone()), ret)
}

pub fn sync_create_and_start_processor(origin_cfg: IndexerConfiguration) -> CommonNotifier {
    let (tx, rx) = watch::channel(());
    let rt = Runtime::new().unwrap();
    let ret = rt.block_on(async_create_and_start_processor(rx, origin_cfg));
    thread::spawn(move || {
        rt.block_on(async {
            let handlers = ret.1;
            for h in handlers {
                h.await.unwrap();
            }
        });
    });

    ret.0
}

