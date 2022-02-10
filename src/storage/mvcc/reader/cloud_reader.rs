// Copyright 2022 TiKV Project Authors. Licensed under Apache-2.0.

use crate::storage::mvcc::{Result, TxnCommitRecord};
use rfstore::{UserMeta, EXTRA_CF, LOCK_CF, WRITE_CF};
use std::sync::Arc;
use txn_types::{Key, Lock, OldValue, TimeStamp, Value, Write, WriteType};

pub struct CloudReader {
    snapshot: Arc<kvengine::SnapAccess>,
    pub statistics: tikv_kv::Statistics,
}

impl CloudReader {
    pub fn new(snapshot: Arc<kvengine::SnapAccess>) -> Self {
        Self {
            snapshot,
            statistics: tikv_kv::Statistics::default(),
        }
    }

    fn get_commit_by_item(item: &kvengine::Item, start_ts: TimeStamp) -> Option<TxnCommitRecord> {
        let user_meta = UserMeta::from_slice(item.user_meta());
        if user_meta.start_ts == start_ts.into_inner() {
            let (write_type, short_value) = if item.value_len() > 0 {
                (WriteType::Put, Some(item.get_value().to_vec()))
            } else {
                (WriteType::Delete, None)
            };
            let write = Write::new(write_type, TimeStamp::new(user_meta.start_ts), short_value);
            return Some(TxnCommitRecord::SingleRecord {
                commit_ts: TimeStamp::new(user_meta.commit_ts),
                write,
            });
        }
        None
    }

    pub fn get_txn_commit_record(
        &mut self,
        key: &Key,
        start_ts: TimeStamp,
    ) -> Result<TxnCommitRecord> {
        let raw_key = key.to_raw()?;
        let item = self.snapshot.get(WRITE_CF, &raw_key, 0);
        if item.user_meta_len() > 0 {
            if let Some(record) = Self::get_commit_by_item(&item, start_ts) {
                return Ok(record);
            }
        }
        let mut data_iter = self.snapshot.new_iterator(WRITE_CF, false, true);
        data_iter.seek(&raw_key);
        while data_iter.valid() {
            let key = data_iter.key();
            if key != raw_key {
                break;
            }
            if let Some(record) = Self::get_commit_by_item(&data_iter.item(), start_ts) {
                return Ok(record);
            }
            data_iter.next();
        }
        let rollback_key =
            rfstore::mvcc::encode_extra_txn_status_key(&raw_key, start_ts.into_inner());
        let item = self.snapshot.get(EXTRA_CF, &rollback_key, 0);
        if item.value_len() == 0 {
            return Ok(TxnCommitRecord::None {
                overlapped_write: None,
            });
        }
        let user_meta = UserMeta::from_slice(item.user_meta());
        let write: Write;
        if user_meta.commit_ts == 0 {
            write = Write::new(WriteType::Rollback, start_ts, None);
        } else {
            write = Write::new(WriteType::Lock, start_ts, None);
        }
        Ok(TxnCommitRecord::SingleRecord {
            commit_ts: TimeStamp::new(user_meta.commit_ts),
            write,
        })
    }

    pub fn load_lock(&self, key: &Key) -> Result<Option<Lock>> {
        let raw_key = key.to_raw().unwrap();
        let item = self.snapshot.get(LOCK_CF, &raw_key, 0);
        if item.value_len() == 0 {
            return Ok(None);
        }
        let lock = Lock::parse(item.get_value())?;
        return Ok(Some(lock));
    }

    pub fn get(
        &mut self,
        key: &Key,
        ts: TimeStamp,
        _gc_fence_limit: Option<TimeStamp>,
    ) -> Result<Option<Value>> {
        let raw_key = key.to_raw()?;
        let item = self.snapshot.get(WRITE_CF, &raw_key, ts.into_inner());
        if item.value_len() > 0 {
            return Ok(Some(item.get_value().to_vec()));
        }
        return Ok(None);
    }

    pub fn get_write(
        &mut self,
        key: &Key,
        ts: TimeStamp,
        _gc_fence_limit: Option<TimeStamp>,
    ) -> Result<Option<Write>> {
        self.seek_write(key, ts)
            .map(|opt| opt.map(|(_, write)| write))
    }

    pub fn seek_write(&mut self, key: &Key, ts: TimeStamp) -> Result<Option<(TimeStamp, Write)>> {
        let raw_key = key.to_raw()?;
        let item = self.snapshot.get(WRITE_CF, &raw_key, ts.into_inner());
        if item.user_meta_len() > 0 {
            let user_meta = UserMeta::from_slice(item.user_meta());
            let write_type: WriteType;
            let short_value: Option<Value>;
            if item.get_value().len() == 0 {
                write_type = WriteType::Delete;
                short_value = None;
            } else {
                write_type = WriteType::Put;
                short_value = Some(item.get_value().to_vec())
            }
            let write = Write::new(write_type, TimeStamp::new(user_meta.start_ts), short_value);
            return Ok(Some((
                TimeStamp::new(user_meta.commit_ts),
                write.to_owned(),
            )));
        }
        return Ok(None);
    }

    #[inline(always)]
    pub fn get_old_value(&mut self, prev_write: Option<Write>) -> Result<OldValue> {
        if let Some(write) = prev_write {
            if write.write_type == WriteType::Delete {
                return Ok(OldValue::None);
            }
            // Locks and Rolbacks are stored in extra CF, will not be seeked by seek_write.
            assert_eq!(write.write_type, WriteType::Put);
            return Ok(OldValue::value(write.short_value.unwrap()));
        }
        return Ok(OldValue::None);
    }

    /// Scan locks that satisfies `filter(lock)` returns true, from the given start key `start`.
    /// At most `limit` locks will be returned. If `limit` is set to `0`, it means unlimited.
    ///
    /// The return type is `(locks, is_remain)`. `is_remain` indicates whether there MAY be
    /// remaining locks that can be scanned.
    pub fn scan_locks<F>(
        &mut self,
        start: Option<&Key>,
        end: Option<&Key>,
        filter: F,
        limit: usize,
    ) -> Result<(Vec<(Key, Lock)>, bool)>
    where
        F: Fn(&Lock) -> bool,
    {
        let mut locks = vec![];
        let mut lock_iter = self.snapshot.new_iterator(LOCK_CF, false, false);
        if let Some(start) = start {
            let raw_start = start.to_raw()?;
            lock_iter.seek(&raw_start);
        } else {
            lock_iter.rewind();
        }
        while lock_iter.valid() {
            let key = Key::from_raw(lock_iter.key());
            if let Some(end) = end {
                if key >= *end {
                    return Ok((locks, false));
                }
            }
            let item = lock_iter.item();
            let lock = Lock::parse(item.get_value())?;
            if filter(&lock) {
                locks.push((key, lock));
                if limit > 0 && locks.len() == limit {
                    return Ok((locks, true));
                }
            }
            lock_iter.next();
        }
        Ok((locks, false))
    }
}
