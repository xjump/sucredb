use std::{mem, str};
use std::path::{Path, PathBuf};
use std::io::Write;
use std::sync::Arc;
use rocksdb::{self, Writable};
use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use utils::*;

struct U16BeSuffixTransform;

impl rocksdb::SliceTransform for U16BeSuffixTransform {
    fn transform<'a>(&mut self, key: &'a [u8]) -> &'a [u8] {
        &key[..2]
    }

    fn in_domain(&mut self, key: &[u8]) -> bool {
        key.len() >= 2
    }
}

pub struct StorageManager {
    path: PathBuf,
    db: Arc<rocksdb::DB>,
}


#[inline]
fn build_key<'a>(buffer: &'a mut [u8], num: u16, key: &[u8]) -> &'a [u8] {
    (&mut buffer[..2]).write_u16::<BigEndian>(num).unwrap();
    (&mut buffer[2..]).write_all(key).unwrap();
    &buffer[..2 + key.len()]
}

#[inline]
fn build_log_key<'a>(buffer: &'a mut [u8], num: u16, log_key: (u64, u64)) -> &'a [u8] {
    (&mut buffer[..2]).write_u16::<BigEndian>(num).unwrap();
    (&mut buffer[2..2 + 8])
        .write_u64::<BigEndian>(log_key.0)
        .unwrap();
    (&mut buffer[2 + 8..2 + 8 + 8])
        .write_u64::<BigEndian>(log_key.1)
        .unwrap();
    &buffer[..2 + 8 + 8]
}

#[inline]
fn build_log_prefix<'a>(buffer: &'a mut [u8], num: u16, prefix: u64) -> &'a [u8] {
    (&mut buffer[..2]).write_u16::<BigEndian>(num).unwrap();
    (&mut buffer[2..2 + 8])
        .write_u64::<BigEndian>(prefix)
        .unwrap();
    &buffer[..2 + 8]
}

// TODO: support TTL
// TODO: specific comparator for log cf
// TODO: merge operator could be a big win
pub struct Storage {
    db: Arc<rocksdb::DB>,
    cf: &'static rocksdb::CFHandle,
    log_cf: &'static rocksdb::CFHandle,
    iterators_handle: Arc<()>,
    num: u16,
}

unsafe impl Sync for Storage {}
unsafe impl Send for Storage {}

pub struct StorageBatch<'a> {
    storage: &'a Storage,
    wb: rocksdb::WriteBatch,
}

pub struct StorageIterator {
    db: Arc<rocksdb::DB>,
    iterator: Box<rocksdb::rocksdb::DBIterator<'static>>,
    iterators_handle: Arc<()>,
    first: bool,
}

pub struct LogStorageIterator {
    db: Arc<rocksdb::DB>,
    iterator: Box<rocksdb::rocksdb::DBIterator<'static>>,
    iterators_handle: Arc<()>,
    first: bool,
}

unsafe impl Send for StorageIterator {}

impl StorageManager {
    pub fn new<P: AsRef<Path>>(path: P) -> Result<StorageManager, GenericError> {
        let mut opts = rocksdb::DBOptions::new();
        opts.create_if_missing(true);
        opts.set_max_background_jobs(4);
        opts.enable_pipelined_write(true);
        let mut def_cf_opts = rocksdb::ColumnFamilyOptions::new();
        def_cf_opts
            .set_prefix_extractor("U16BeSuffixTransform", Box::new(U16BeSuffixTransform))
            .unwrap();
        def_cf_opts.compression_per_level(&[
            rocksdb::DBCompressionType::No,
            rocksdb::DBCompressionType::No,
            rocksdb::DBCompressionType::Lz4,
            rocksdb::DBCompressionType::Lz4,
            rocksdb::DBCompressionType::Lz4,
            rocksdb::DBCompressionType::Lz4,
            rocksdb::DBCompressionType::Lz4,
        ]);
        def_cf_opts.set_write_buffer_size(32 * 1024 * 1024);
        def_cf_opts.set_max_bytes_for_level_base(4 * 32 * 1024 * 1024);
        def_cf_opts.set_max_write_buffer_number(4);

        let mut block_opts = rocksdb::BlockBasedOptions::new();
        block_opts.set_bloom_filter(10, false);
        block_opts.set_lru_cache(4 * 32 * 1024 * 1024);
        def_cf_opts.set_block_based_table_factory(&block_opts);

        let mut log_cf_opts = rocksdb::ColumnFamilyOptions::new();
        log_cf_opts.compression(rocksdb::DBCompressionType::No);
        // TODO: add options to only keep it for X time
        log_cf_opts.set_compaction_style(rocksdb::DBCompactionStyle::Fifo);
        log_cf_opts.set_write_buffer_size(32 * 1024 * 1024);
        log_cf_opts.set_max_write_buffer_number(4);

        let mut block_opts = rocksdb::BlockBasedOptions::new();
        block_opts.set_bloom_filter(10, false);
        block_opts.set_lru_cache(2 * 32 * 1024 * 1024);
        log_cf_opts.set_block_based_table_factory(&block_opts);

        // TODO: Rocksdb is complicated, we might want to tune some more options

        let db = rocksdb::DB::open_cf(
            opts.clone(),
            path.as_ref().to_str().unwrap(),
            vec![("default", def_cf_opts.clone()), ("log", log_cf_opts.clone())],
        ).or_else(|_| -> Result<_, String> {
            let mut db = rocksdb::DB::open_cf(
                opts,
                path.as_ref().to_str().unwrap(),
                vec![("default", def_cf_opts)],
            )?;

            db.create_cf("log", log_cf_opts)?;
            Ok(db)
        })?;

        Ok(StorageManager {
            path: path.as_ref().into(),
            db: Arc::new(db),
        })
    }

    pub fn open(&self, db_num: u16) -> Result<Storage, GenericError> {
        Ok(Storage {
            db: self.db.clone(),
            cf: unsafe { mem::transmute(self.db.cf_handle("default").unwrap()) },
            log_cf: unsafe { mem::transmute(self.db.cf_handle("log").unwrap()) },
            num: db_num,
            iterators_handle: Arc::new(()),
        })
    }
}

impl Drop for StorageManager {
    fn drop(&mut self) {
        let sc = Arc::strong_count(&self.db);
        let wc = Arc::weak_count(&self.db);
        assert_eq!(wc, 0);
        assert_eq!(sc, 1);
    }
}

impl Storage {
    pub fn iterator(&self) -> StorageIterator {
        let mut key_prefix = [0u8; 2];
        build_key(&mut key_prefix, self.num, b"");
        unsafe {
            let mut ro = rocksdb::ReadOptions::new();
            ro.set_total_order_seek(false);
            ro.set_prefix_same_as_start(true);
            let mut iterator = Box::new(self.db.iter_cf_opt(self.cf, ro));
            iterator.seek(rocksdb::SeekKey::Key(&key_prefix[..]));
            let result = StorageIterator {
                db: self.db.clone(),
                iterator: mem::transmute(iterator),
                iterators_handle: self.iterators_handle.clone(),
                first: true,
            };
            result
        }
    }

    pub fn log_iterator(&self, prefix: u64, start: u64) -> LogStorageIterator {
        let mut end_prefix = [0u8; 2 + 8];
        build_log_prefix(&mut end_prefix, self.num, prefix + 1);
        let mut start_key = [0u8; 2 + 8 + 8];
        build_log_key(&mut start_key, self.num, (prefix, start));
        unsafe {
            let mut ro = rocksdb::ReadOptions::new();
            ro.set_iterate_upper_bound(&end_prefix[..]);
            let mut iterator = Box::new(self.db.iter_cf_opt(self.log_cf, ro));
            iterator.seek(rocksdb::SeekKey::Key(&start_key[..]));
            let result = LogStorageIterator {
                db: self.db.clone(),
                iterator: mem::transmute(iterator),
                iterators_handle: self.iterators_handle.clone(),
                first: true,
            };
            result
        }
    }

    pub fn get<R, F: FnOnce(&[u8]) -> R>(&self, key: &[u8], callback: F) -> Option<R> {
        let mut buffer = [0u8; 512];
        let buffer = build_key(&mut buffer, self.num, key);
        let r = self.db.get_cf(self.cf, buffer).unwrap();
        trace!(
            "get {:?} ({:?} bytes)",
            str::from_utf8(key),
            r.as_ref().map(|x| x.len())
        );
        r.map(|r| callback(&*r))
    }

    pub fn log_get<R, F: FnOnce(&[u8]) -> R>(&self, log_key: (u64, u64), callback: F) -> Option<R> {
        let mut buffer = [0u8; 2 + 8 + 8];
        let buffer = build_log_key(&mut buffer, self.num, log_key);
        let r = self.db.get_cf(self.log_cf, buffer).unwrap();
        trace!(
            "log_get {:?} ({:?} bytes)",
            log_key,
            r.as_ref().map(|x| x.len())
        );
        r.map(|r| callback(&*r))
    }

    pub fn get_vec(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.get(key, |v| v.to_owned())
    }

    pub fn log_get_vec(&self, log_key: (u64, u64)) -> Option<Vec<u8>> {
        self.log_get(log_key, |v| v.to_owned())
    }

    pub fn set(&self, key: &[u8], value: &[u8]) {
        let mut b = self.batch_new(0);
        b.set(key, value);
        self.batch_write(b);
    }

    pub fn del(&self, key: &[u8]) {
        let mut b = self.batch_new(0);
        b.del(key);
        self.batch_write(b);
    }

    pub fn batch_new(&self, reserve: usize) -> StorageBatch {
        StorageBatch {
            storage: self,
            wb: rocksdb::WriteBatch::with_capacity(reserve),
        }
    }

    pub fn batch_write(&self, batch: StorageBatch) {
        self.db.write(batch.wb).unwrap();
    }

    pub fn clear(&self) {
        trace!("clear");
        self.check_pending_iters();
        let mut from = [0u8; 2];
        let mut to = [0u8; 2];
        (&mut from[..]).write_u16::<BigEndian>(self.num).unwrap();
        (&mut to[..]).write_u16::<BigEndian>(self.num + 1).unwrap();

        for &cf in &[self.cf, self.log_cf] {
            self.db
                .delete_file_in_range_cf(cf, &from[..], &to[..])
                .unwrap();
            let mut ro = rocksdb::ReadOptions::new();
            ro.set_total_order_seek(false);
            ro.set_prefix_same_as_start(true);
            ro.set_iterate_upper_bound(&to[..]);
            let mut iter = self.db.iter_cf_opt(cf, ro);
            iter.seek(rocksdb::SeekKey::Key(&from[..]));
            while iter.valid() {
                self.db.delete_cf(cf, iter.key()).unwrap();
                iter.next();
            }
        }
    }

    pub fn sync(&self) {
        debug!("sync");
        self.db.sync_wal().unwrap();
    }

    fn check_pending_iters(&self) {
        let sc = Arc::strong_count(&self.iterators_handle);
        let wc = Arc::weak_count(&self.iterators_handle);
        assert!(wc == 0);
        assert!(sc == 1, "{} pending iterators", sc - 1);
    }
}

impl Drop for Storage {
    fn drop(&mut self) {
        self.check_pending_iters();
    }
}

impl<'a> StorageBatch<'a> {
    pub fn set(&mut self, key: &[u8], value: &[u8]) {
        trace!("set {:?} ({} bytes)", str::from_utf8(key), value.len());
        let mut buffer = [0u8; 512];
        let buffer = build_key(&mut buffer, self.storage.num, key);
        self.wb.put_cf(self.storage.cf, buffer, value).unwrap();
    }

    pub fn log_set(&mut self, key: (u64, u64), value: &[u8]) {
        trace!("log_set {:?} ({} bytes)", key, value.len());
        let mut buffer = [0u8; 2 + 8 + 8];
        let buffer = build_log_key(&mut buffer, self.storage.num, key);
        self.wb.put_cf(self.storage.log_cf, buffer, value).unwrap();
    }

    pub fn del(&mut self, key: &[u8]) {
        trace!("del {:?}", str::from_utf8(key));
        let mut buffer = [0u8; 512];
        let buffer = build_key(&mut buffer, self.storage.num, key);
        self.wb.delete_cf(self.storage.cf, buffer).unwrap()
    }
}

impl StorageIterator {
    pub fn iter<'a>(&'a mut self) -> StorageIter<'a> {
        StorageIter { it: self }
    }
}

pub struct StorageIter<'a> {
    it: &'a mut StorageIterator,
}

impl<'a> Iterator for StorageIter<'a> {
    type Item = (&'a [u8], &'a [u8]);
    fn next(&mut self) -> Option<Self::Item> {
        if self.it.first {
            self.it.first = false;
        } else {
            self.it.iterator.next();
        }
        if self.it.iterator.valid() {
            unsafe {
                // safe as slices are valid until the next call to next
                Some((
                    mem::transmute(&self.it.iterator.key()[2..]),
                    mem::transmute(self.it.iterator.value()),
                ))
            }
        } else {
            None
        }
    }
}


impl LogStorageIterator {
    pub fn iter<'a>(&'a mut self) -> LogStorageIter<'a> {
        LogStorageIter { it: self }
    }
}

pub struct LogStorageIter<'a> {
    it: &'a mut LogStorageIterator,
}

impl<'a> Iterator for LogStorageIter<'a> {
    type Item = ((u64, u64), &'a [u8]);
    fn next(&mut self) -> Option<Self::Item> {
        if self.it.first {
            self.it.first = false;
        } else {
            self.it.iterator.next();
        }
        if self.it.iterator.valid() {
            let key = self.it.iterator.key();
            assert!(key.len() == 2 + 8 + 8);
            let first = (&self.it.iterator.key()[2..2 + 8])
                .read_u64::<BigEndian>()
                .unwrap();
            let second = (&self.it.iterator.key()[2 + 8..2 + 8 + 8])
                .read_u64::<BigEndian>()
                .unwrap();
            unsafe {
                // safe as slices are valid until the next call to next
                Some(((first, second), mem::transmute(self.it.iterator.value())))
            }
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::fs;

    #[test]
    fn test_simple() {
        let _ = fs::remove_dir_all("t/test_simple");
        let sm = StorageManager::new("t/test_simple").unwrap();
        let storage = sm.open(1).unwrap();
        assert_eq!(storage.get_vec(b"sample"), None);
        storage.set(b"sample", b"sample_value");
        assert_eq!(storage.get_vec(b"sample").unwrap(), b"sample_value");
        storage.del(b"sample");
        assert_eq!(storage.get_vec(b"sample"), None);
    }

    #[test]
    fn test_simple_log() {
        let _ = fs::remove_dir_all("t/test_simple_log");
        let sm = StorageManager::new("t/test_simple_log").unwrap();
        let storage = sm.open(1).unwrap();
        assert_eq!(storage.get_vec(b"sample"), None);
        let mut b = storage.batch_new(0);
        b.set(b"sample", b"sample_value");
        b.log_set((1, 1), b"sample");
        storage.batch_write(b);
        assert_eq!(storage.get_vec(b"sample").unwrap(), b"sample_value");
        assert_eq!(storage.log_get_vec((1, 1)).unwrap(), b"sample");
    }

    #[test]
    fn test_iter() {
        let _ = fs::remove_dir_all("t/test_iter");
        let sm = StorageManager::new("t/test_iter").unwrap();
        for &i in &[0, 1, 2] {
            let storage = sm.open(i).unwrap();
            storage.set(b"1", i.to_string().as_bytes());
            storage.set(b"2", i.to_string().as_bytes());
            storage.set(b"3", i.to_string().as_bytes());
        }
        for &i in &[0, 1, 2] {
            let storage = sm.open(i).unwrap();
            let results: Vec<Vec<u8>> = storage.iterator().iter().map(|(k, v)| v.into()).collect();
            assert_eq!(results, vec![i.to_string().as_bytes(); 3]);
        }
    }

    #[test]
    fn test_iter_log() {
        let _ = fs::remove_dir_all("t/test_iter_log");
        let sm = StorageManager::new("t/test_iter_log").unwrap();
        for &i in &[0u64, 1, 2] {
            let storage = sm.open(i as u16).unwrap();
            let mut b = storage.batch_new(0);
            b.log_set((i, i + 0), (i + 0).to_string().as_bytes());
            b.log_set((i, i + 1), (i + 1).to_string().as_bytes());
            b.log_set((i, i + 2), (i + 2).to_string().as_bytes());
            b.log_set((i + 1, i), b"");
            storage.batch_write(b);
        }
        for &i in &[0u64, 1, 2] {
            let storage = sm.open(i as u16).unwrap();
            let results: Vec<(_, Vec<u8>)> = storage
                .log_iterator(i, i + 1)
                .iter()
                .map(|(k, v)| (k, v.into()))
                .collect();
            assert_eq!(
                results,
                vec![
                    ((i, i + 1), (i + 1).to_string().into_bytes()),
                    ((i, i + 2), (i + 2).to_string().into_bytes()),
                ]
            );
            assert_eq!(storage.log_iterator(i, i).iter().count(), 3);
        }
    }

    #[test]
    fn test_clear() {
        let _ = fs::remove_dir_all("t/test_clear");
        let sm = StorageManager::new("t/test_clear").unwrap();
        for &i in &[0u64, 1, 2] {
            let storage = sm.open(i as u16).unwrap();
            let mut b = storage.batch_new(0);
            b.set(i.to_string().as_bytes(), i.to_string().as_bytes());
            b.log_set((i, i), i.to_string().as_bytes());
            storage.batch_write(b);
        }
        for &i in &[0u64, 1, 2] {
            let storage = sm.open(i as u16).unwrap();
            assert_eq!(storage.iterator().iter().count(), 1);
            assert_eq!(storage.log_iterator(i, i).iter().count(), 1);
            storage.clear();
            assert_eq!(storage.iterator().iter().count(), 0);
            assert_eq!(storage.log_iterator(i, i).iter().count(), 0);
        }
    }

    #[test]
    fn test_open_all() {
        let _ = fs::remove_dir_all("t/test_open_all");
        let sm = StorageManager::new("t/test_open_all").unwrap();
        sm.open(1).unwrap();
        sm.open(2).unwrap();
        sm.open(3).unwrap();
        sm.open(1).unwrap();
        sm.open(2).unwrap();
        sm.open(3).unwrap();
    }

}
