use crate::error::IndexerResult;
use crate::storage::db::DB;
use rusty_leveldb::WriteBatch;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

#[derive(Default, Clone)]
pub struct MemoryDB {
    datas: Rc<RefCell<HashMap<Vec<u8>, Vec<u8>>>>,
}

unsafe impl Send for MemoryDB {}
unsafe impl Sync for MemoryDB {}
impl DB for MemoryDB {
    fn set(&mut self, key: &[u8], value: &[u8]) -> IndexerResult<()> {
        let mut data = self.datas.borrow_mut();
        data.insert(key.to_vec(), value.to_vec());
        Ok(())
    }

    fn get(&mut self, key: &[u8]) -> IndexerResult<Option<Vec<u8>>> {
        let mut data = self.datas.borrow_mut();
        Ok(data.get(key).cloned())
    }

    fn write_batch(&mut self, batch: WriteBatch, sync: bool) -> IndexerResult<()> {
        let mut data = self.datas.borrow_mut();
        batch.iter().for_each(|(k, v)| {
            if v.is_none() {
                data.remove(k);
            } else {
                let v = v.unwrap().to_vec();
                data.insert(k.to_vec(), v);
            }
        });
        Ok(())
    }

    fn iter_all<KF, VF, K, V>(
        &mut self,
        prefix: &[u8],
        kf: KF,
        vf: VF,
    ) -> IndexerResult<Vec<(K, V)>>
    where
        KF: Fn(Vec<u8>) -> K,
        VF: Fn(Vec<u8>) -> Option<V>,
    {
        let mut ret = vec![];
        let mut data = self.datas.borrow_mut();
        for (k, v) in data.iter() {
            if k.starts_with(prefix) {
                let v = vf(v.clone());
                if v.is_some() {
                    ret.push((kf(k.clone()), v.unwrap()));
                }
            }
        }
        Ok(ret)
    }
}