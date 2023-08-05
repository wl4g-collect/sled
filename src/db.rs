use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::mem::{self, ManuallyDrop};
use std::num::NonZeroU64;
use std::ops;
use std::ops::Bound;
use std::ops::RangeBounds;
use std::path::Path;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use cache_advisor::CacheAdvisor;
use concurrent_map::{ConcurrentMap, Minimum};
use inline_array::InlineArray;
use parking_lot::{
    lock_api::{ArcRwLockReadGuard, ArcRwLockWriteGuard},
    RawRwLock, RwLock,
};
use stack_map::StackMap;

use crate::*;

/// sled 1.0
#[derive(Debug, Clone)]
pub struct Db<
    const INDEX_FANOUT: usize = 64,
    const LEAF_FANOUT: usize = 1024,
    const EBR_LOCAL_GC_BUFFER_SIZE: usize = 128,
> {
    global_error: Arc<AtomicPtr<(io::ErrorKind, String)>>,
    config: Config,
    high_level_rc: Arc<()>,
    index: ConcurrentMap<
        InlineArray,
        Node<LEAF_FANOUT>,
        INDEX_FANOUT,
        EBR_LOCAL_GC_BUFFER_SIZE,
    >,
    node_id_to_low_key_index:
        ConcurrentMap<u64, InlineArray, INDEX_FANOUT, EBR_LOCAL_GC_BUFFER_SIZE>,
    store: heap::Heap,
    cache_advisor: RefCell<CacheAdvisor>,
    flush_epoch: FlushEpoch,
    // the value here is for serialized bytes
    dirty: ConcurrentMap<(NonZeroU64, InlineArray), Option<Arc<Vec<u8>>>>,
    shutdown_sender: Option<mpsc::Sender<mpsc::Sender<()>>>,
    was_recovered: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct Node<const LEAF_FANOUT: usize> {
    // used for access in heap::Heap
    id: NodeId,
    #[serde(skip)]
    inner: Arc<RwLock<Option<Box<Leaf<LEAF_FANOUT>>>>>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct Leaf<const LEAF_FANOUT: usize> {
    lo: InlineArray,
    hi: Option<InlineArray>,
    prefix_length: usize,
    data: StackMap<InlineArray, InlineArray, LEAF_FANOUT>,
    #[serde(skip)]
    dirty_flush_epoch: Option<NonZeroU64>,
    in_memory_size: usize,
}

fn flusher<
    const INDEX_FANOUT: usize,
    const LEAF_FANOUT: usize,
    const EBR_LOCAL_GC_BUFFER_SIZE: usize,
>(
    db: Db<INDEX_FANOUT, LEAF_FANOUT, EBR_LOCAL_GC_BUFFER_SIZE>,
    shutdown_signal: mpsc::Receiver<mpsc::Sender<()>>,
    flush_every_ms: usize,
) {
    let interval = Duration::from_millis(flush_every_ms as _);
    let mut last_flush_duration = Duration::default();
    loop {
        let recv_timeout = interval
            .saturating_sub(last_flush_duration)
            .max(Duration::from_millis(1));
        if let Ok(shutdown_sender) = shutdown_signal.recv_timeout(recv_timeout)
        {
            drop(db);
            if let Err(e) = shutdown_sender.send(()) {
                log::error!(
                    "Db flusher could not ack shutdown to requestor: {e:?}"
                );
            }
            log::debug!(
                "flush thread terminating after signalling to requestor"
            );
            return;
        }

        let before_flush = Instant::now();
        if let Err(e) = db.flush() {
            log::error!("Db flusher encountered error while flushing: {:?}", e);
            db.set_error(&e);
            return;
        }
        last_flush_duration = before_flush.elapsed();
    }
}

impl<const LEAF_FANOUT: usize> Leaf<LEAF_FANOUT> {
    fn serialize(&self, zstd_compression_level: i32) -> Vec<u8> {
        let mut ret = vec![];

        let mut zstd_enc =
            zstd::stream::Encoder::new(&mut ret, zstd_compression_level)
                .unwrap();

        bincode::serialize_into(&mut zstd_enc, self).unwrap();

        zstd_enc.finish().unwrap();

        ret
    }

    fn deserialize(buf: &[u8]) -> io::Result<Box<Leaf<LEAF_FANOUT>>> {
        let zstd_decoded = zstd::stream::decode_all(buf).unwrap();
        let mut leaf: Box<Leaf<LEAF_FANOUT>> =
            bincode::deserialize(&zstd_decoded).unwrap();

        // use decompressed buffer length as a cheap proxy for in-memory size for now
        leaf.in_memory_size = zstd_decoded.len();

        Ok(leaf)
    }

    fn set_in_memory_size(&mut self) {
        self.in_memory_size = mem::size_of::<Leaf<LEAF_FANOUT>>()
            + self.hi.as_ref().map(|h| h.len()).unwrap_or(0)
            + self.lo.len()
            + self.data.iter().map(|(k, v)| k.len() + v.len()).sum::<usize>();
    }

    fn split_if_full(
        &mut self,
        new_epoch: NonZeroU64,
        allocator: &heap::Heap,
    ) -> Option<(InlineArray, Node<LEAF_FANOUT>)> {
        if self.data.is_full() {
            // split
            let split_offset = if self.lo.is_empty() {
                // split left-most shard almost at the beginning for
                // optimizing downward-growing workloads
                1
            } else if self.hi.is_none() {
                // split right-most shard almost at the end for
                // optimizing upward-growing workloads
                self.data.len() - 2
            } else {
                self.data.len() / 2
            };

            let data = self.data.split_off(split_offset);

            let left_max = &self.data.last().as_ref().unwrap().0;
            let right_min = &data.first().as_ref().unwrap().0;

            // suffix truncation attempts to shrink the split key
            // so that shorter keys bubble up into the index
            let splitpoint_length = right_min
                .iter()
                .zip(left_max.iter())
                .take_while(|(a, b)| a == b)
                .count()
                + 1;

            let split_key = InlineArray::from(&right_min[..splitpoint_length]);

            let rhs_id = allocator.allocate_object_id();

            log::trace!(
                "split leaf {:?} at split key: {:?} into new node {}",
                self.lo,
                split_key,
                rhs_id
            );

            let mut rhs = Leaf {
                dirty_flush_epoch: Some(new_epoch),
                hi: self.hi.clone(),
                lo: split_key.clone(),
                prefix_length: 0,
                in_memory_size: 0,
                data,
            };
            rhs.set_in_memory_size();

            self.hi = Some(split_key.clone());
            self.set_in_memory_size();

            let rhs_node = Node {
                id: NodeId(rhs_id),
                inner: Arc::new(Some(Box::new(rhs)).into()),
            };

            return Some((split_key, rhs_node));
        }

        None
    }
}

#[must_use]
struct LeafReadGuard<
    'a,
    const INDEX_FANOUT: usize = 64,
    const LEAF_FANOUT: usize = 1024,
    const EBR_LOCAL_GC_BUFFER_SIZE: usize = 128,
> {
    leaf_read: ManuallyDrop<
        ArcRwLockReadGuard<RawRwLock, Option<Box<Leaf<LEAF_FANOUT>>>>,
    >,
    low_key: InlineArray,
    inner: &'a Db<INDEX_FANOUT, LEAF_FANOUT, EBR_LOCAL_GC_BUFFER_SIZE>,
    node_id: NodeId,
}

impl<
        'a,
        const INDEX_FANOUT: usize,
        const LEAF_FANOUT: usize,
        const EBR_LOCAL_GC_BUFFER_SIZE: usize,
    > Drop
    for LeafReadGuard<'a, INDEX_FANOUT, LEAF_FANOUT, EBR_LOCAL_GC_BUFFER_SIZE>
{
    fn drop(&mut self) {
        let size = self.leaf_read.as_ref().unwrap().in_memory_size;
        // we must drop our mutex before calling mark_access_and_evict
        unsafe {
            ManuallyDrop::drop(&mut self.leaf_read);
        }
        if let Err(e) = self.inner.mark_access_and_evict(self.node_id, size) {
            self.inner.set_error(&e);
            log::error!(
                "io error while paging out dirty data: {:?} \
                for guard of leaf with low key {:?}",
                e,
                self.low_key
            );
        }
    }
}

struct LeafWriteGuard<
    'a,
    const INDEX_FANOUT: usize = 64,
    const LEAF_FANOUT: usize = 1024,
    const EBR_LOCAL_GC_BUFFER_SIZE: usize = 128,
> {
    leaf_write: ManuallyDrop<
        ArcRwLockWriteGuard<RawRwLock, Option<Box<Leaf<LEAF_FANOUT>>>>,
    >,
    flush_epoch_guard: FlushEpochGuard<'a>,
    low_key: InlineArray,
    inner: &'a Db<INDEX_FANOUT, LEAF_FANOUT, EBR_LOCAL_GC_BUFFER_SIZE>,
    node_id: NodeId,
}

impl<
        'a,
        const INDEX_FANOUT: usize,
        const LEAF_FANOUT: usize,
        const EBR_LOCAL_GC_BUFFER_SIZE: usize,
    > LeafWriteGuard<'a, INDEX_FANOUT, LEAF_FANOUT, EBR_LOCAL_GC_BUFFER_SIZE>
{
    fn epoch(&self) -> NonZeroU64 {
        self.flush_epoch_guard.epoch()
    }
}

impl<
        'a,
        const INDEX_FANOUT: usize,
        const LEAF_FANOUT: usize,
        const EBR_LOCAL_GC_BUFFER_SIZE: usize,
    > Drop
    for LeafWriteGuard<'a, INDEX_FANOUT, LEAF_FANOUT, EBR_LOCAL_GC_BUFFER_SIZE>
{
    fn drop(&mut self) {
        let size = self.leaf_write.as_ref().unwrap().in_memory_size;
        // we must drop our mutex before calling mark_access_and_evict
        unsafe {
            ManuallyDrop::drop(&mut self.leaf_write);
        }
        if let Err(e) = self.inner.mark_access_and_evict(self.node_id, size) {
            self.inner.set_error(&e);
            log::error!("io error while paging out dirty data: {:?}", e);
        }
    }
}

pub fn open_default<P: AsRef<Path>>(path: P) -> io::Result<Db> {
    Config { path: path.as_ref().into(), ..Default::default() }.open()
}

impl<
        const INDEX_FANOUT: usize,
        const LEAF_FANOUT: usize,
        const EBR_LOCAL_GC_BUFFER_SIZE: usize,
    > Drop for Db<INDEX_FANOUT, LEAF_FANOUT, EBR_LOCAL_GC_BUFFER_SIZE>
{
    fn drop(&mut self) {
        if Arc::strong_count(&self.high_level_rc) == 2 {
            if let Some(shutdown_sender) = self.shutdown_sender.take() {
                let (tx, rx) = mpsc::channel();
                if shutdown_sender.send(tx).is_ok() {
                    if let Err(e) = rx.recv() {
                        log::error!(
                            "failed to shut down flusher thread: {:?}",
                            e
                        );
                    } else {
                        log::trace!("flush thread successfully terminated");
                    }
                }
            }
        }

        if Arc::strong_count(&self.high_level_rc) == 1 {
            if let Err(e) = self.flush() {
                eprintln!("failed to flush Db on Drop: {e:?}");
            }

            // this is probably unnecessary but it will avoid issues
            // if egregious bugs get introduced that trigger it
            self.set_error(&io::Error::new(
                io::ErrorKind::Other,
                "system has been shut down".to_string(),
            ));
        }
    }
}

fn map_bound<T, U, F: FnOnce(T) -> U>(bound: Bound<T>, f: F) -> Bound<U> {
    match bound {
        Bound::Unbounded => Bound::Unbounded,
        Bound::Included(x) => Bound::Included(f(x)),
        Bound::Excluded(x) => Bound::Excluded(f(x)),
    }
}

impl<
        const INDEX_FANOUT: usize,
        const LEAF_FANOUT: usize,
        const EBR_LOCAL_GC_BUFFER_SIZE: usize,
    > Db<INDEX_FANOUT, LEAF_FANOUT, EBR_LOCAL_GC_BUFFER_SIZE>
{
    // This is only pub for an extra assertion during testing.
    #[doc(hidden)]
    pub fn check_error(&self) -> io::Result<()> {
        let err_ptr: *const (io::ErrorKind, String) =
            self.global_error.load(Ordering::Acquire);

        if err_ptr.is_null() {
            Ok(())
        } else {
            let deref: &(io::ErrorKind, String) = unsafe { &*err_ptr };
            Err(io::Error::new(deref.0, deref.1.clone()))
        }
    }

    fn set_error(&self, error: &io::Error) {
        let kind = error.kind();
        let reason = error.to_string();

        let boxed = Box::new((kind, reason));
        let ptr = Box::into_raw(boxed);

        if self
            .global_error
            .compare_exchange(
                std::ptr::null_mut(),
                ptr,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_err()
        {
            // global fatal error already installed, drop this one
            unsafe {
                drop(Box::from_raw(ptr));
            }
        }
    }

    pub fn storage_stats(&self) -> heap::Stats {
        self.store.stats()
    }

    pub fn size_on_disk(&self) -> io::Result<u64> {
        use std::fs::read_dir;

        fn recurse(mut dir: std::fs::ReadDir) -> io::Result<u64> {
            dir.try_fold(0, |acc, file| {
                let file = file?;
                let size = match file.metadata()? {
                    data if data.is_dir() => recurse(read_dir(file.path())?)?,
                    data => data.len(),
                };
                Ok(acc + size)
            })
        }

        recurse(read_dir(&self.config.path)?)
    }

    fn leaf_for_key<'a>(
        &'a self,
        key: &[u8],
    ) -> io::Result<
        LeafReadGuard<'a, INDEX_FANOUT, LEAF_FANOUT, EBR_LOCAL_GC_BUFFER_SIZE>,
    > {
        let (low_key, read, node_id) = loop {
            let (low_key, node) = self.index.get_lte(key).unwrap();
            let mut read = node.inner.read_arc();

            if read.is_none() {
                drop(read);
                let (_low_key, write, _node_id) = self.page_in(key)?;
                read = ArcRwLockWriteGuard::downgrade(write);
            }

            let leaf = read.as_ref().unwrap();

            assert!(&*leaf.lo <= key);
            if let Some(ref hi) = leaf.hi {
                if &**hi < key {
                    log::trace!("key overshoot on leaf_for_key");
                    continue;
                }
            }
            break (low_key, read, node.id);
        };

        Ok(LeafReadGuard {
            leaf_read: ManuallyDrop::new(read),
            inner: self,
            low_key,
            node_id,
        })
    }

    /// Returns `true` if the database was
    /// recovered from a previous process.
    /// Note that database state is only
    /// guaranteed to be present up to the
    /// last call to `flush`! Otherwise state
    /// is synced to disk periodically if the
    /// `Config.sync_every_ms` configuration option
    /// is set to `Some(number_of_ms_between_syncs)`
    /// or if the IO buffer gets filled to
    /// capacity before being rotated.
    pub fn was_recovered(&self) -> bool {
        self.was_recovered
    }

    pub fn open_with_config(
        config: &Config,
    ) -> io::Result<Db<INDEX_FANOUT, LEAF_FANOUT, EBR_LOCAL_GC_BUFFER_SIZE>>
    {
        let (store, index_data) = heap::recover(&config.path)?;

        let first_id_opt = if index_data.is_empty() {
            Some(store.allocate_object_id())
        } else {
            None
        };

        let index: ConcurrentMap<
            InlineArray,
            Node<LEAF_FANOUT>,
            INDEX_FANOUT,
            EBR_LOCAL_GC_BUFFER_SIZE,
        > = initialize(&index_data, first_id_opt);

        let node_id_to_low_key_index: ConcurrentMap<
            u64,
            InlineArray,
            INDEX_FANOUT,
            EBR_LOCAL_GC_BUFFER_SIZE,
        > = index.iter().map(|(low_key, node)| (node.id.0, low_key)).collect();

        let mut ret = Db {
            global_error: Default::default(),
            high_level_rc: Arc::new(()),
            store,
            cache_advisor: RefCell::new(CacheAdvisor::new(
                config.cache_capacity_bytes,
                config.entry_cache_percent,
            )),
            index,
            config: config.clone(),
            node_id_to_low_key_index,
            dirty: Default::default(),
            flush_epoch: Default::default(),
            shutdown_sender: None,
            was_recovered: first_id_opt.is_none(),
        };

        if let Some(flush_every_ms) = ret.config.flush_every_ms {
            let db = ret.clone();
            let (tx, rx) = mpsc::channel();
            ret.shutdown_sender = Some(tx);
            std::thread::spawn(move || flusher(db, rx, flush_every_ms));
        }
        Ok(ret)
    }

    fn page_in(
        &self,
        key: &[u8],
    ) -> io::Result<(
        InlineArray,
        ArcRwLockWriteGuard<RawRwLock, Option<Box<Leaf<LEAF_FANOUT>>>>,
        NodeId,
    )> {
        loop {
            let (low_key, node) = self.index.get_lte(key).unwrap();
            let mut write = node.inner.write_arc();
            if write.is_none() {
                let leaf_bytes = self.store.read(node.id.0)?.unwrap();
                let leaf: Box<Leaf<LEAF_FANOUT>> =
                    Leaf::deserialize(&leaf_bytes).unwrap();
                *write = Some(leaf);
            }
            let leaf = write.as_mut().unwrap();

            assert!(&*leaf.lo <= key);
            if let Some(ref hi) = leaf.hi {
                if &**hi < key {
                    let size = leaf.in_memory_size;
                    drop(write);
                    log::trace!("key overshoot in leaf_for_key_mut_inner");
                    self.mark_access_and_evict(node.id, size)?;

                    continue;
                }
            }
            return Ok((low_key, write, node.id));
        }
    }

    fn leaf_for_key_mut<'a>(
        &'a self,
        key: &[u8],
    ) -> io::Result<
        LeafWriteGuard<'a, INDEX_FANOUT, LEAF_FANOUT, EBR_LOCAL_GC_BUFFER_SIZE>,
    > {
        let (low_key, mut write, node_id) = self.page_in(key)?;

        let flush_epoch_guard = self.flush_epoch.check_in();

        let leaf = write.as_mut().unwrap();
        assert!(&*leaf.lo <= key);
        if let Some(ref hi) = leaf.hi {
            assert!(&**hi > key);
        }

        if let Some(old_flush_epoch) = leaf.dirty_flush_epoch {
            if old_flush_epoch != flush_epoch_guard.epoch() {
                assert_eq!(
                    old_flush_epoch.get() + 1,
                    flush_epoch_guard.epoch().get()
                );

                log::trace!(
                    "cooperatively flushing {:?} with dirty epoch {} after checking into epoch {}",
                    node_id,
                    old_flush_epoch.get(),
                    flush_epoch_guard.epoch().get()
                );
                // cooperatively serialize and put into dirty
                let dirty_epoch = leaf.dirty_flush_epoch.take().unwrap();

                // be extra-explicit about serialized bytes
                let leaf_ref: &Leaf<LEAF_FANOUT> = &*leaf;

                let serialized =
                    leaf_ref.serialize(self.config.zstd_compression_level);

                self.dirty.insert(
                    (dirty_epoch, leaf.lo.clone()),
                    Some(Arc::new(serialized)),
                );
            }
        }

        Ok(LeafWriteGuard {
            flush_epoch_guard,
            leaf_write: ManuallyDrop::new(write),
            inner: self,
            low_key,
            node_id,
        })
    }

    // NB: must not be called while holding a leaf lock - which also means
    // that no two LeafGuards can be held concurrently in the same scope due to
    // this being called in the destructor.
    fn mark_access_and_evict(
        &self,
        node_id: NodeId,
        size: usize,
    ) -> io::Result<()> {
        let mut ca = self.cache_advisor.borrow_mut();
        let to_evict = ca.accessed_reuse_buffer(node_id.0, size);
        for (node_to_evict, _rough_size) in to_evict {
            let low_key =
                self.node_id_to_low_key_index.get(node_to_evict).unwrap();
            let node = self.index.get(&low_key).unwrap();
            let mut write = node.inner.write();
            if write.is_none() {
                // already paged out
                continue;
            }
            let leaf: &mut Leaf<LEAF_FANOUT> = write.as_mut().unwrap();
            if let Some(dirty_flush_epoch) = leaf.dirty_flush_epoch {
                let serialized =
                    leaf.serialize(self.config.zstd_compression_level);

                self.dirty.insert(
                    (dirty_flush_epoch, leaf.lo.clone()),
                    Some(Arc::new(serialized)),
                );
            }
            *write = None;
        }

        Ok(())
    }

    /// Retrieve a value from the `Tree` if it exists.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # let config = sled::Config::tmp().unwrap();
    /// # let db: sled::Db<64, 1024, 128> = config.open()?;
    /// db.insert(&[0], vec![0])?;
    /// assert_eq!(db.get(&[0]).unwrap(), Some(sled::InlineArray::from(vec![0])));
    /// assert!(db.get(&[1]).unwrap().is_none());
    /// # Ok(()) }
    /// ```
    pub fn get<K: AsRef<[u8]>>(
        &self,
        key: K,
    ) -> io::Result<Option<InlineArray>> {
        self.check_error()?;

        let key_ref = key.as_ref();

        let leaf_guard = self.leaf_for_key(key_ref)?;

        let leaf = leaf_guard.leaf_read.as_ref().unwrap();

        if let Some(ref hi) = leaf.hi {
            assert!(&**hi > key_ref);
        }

        Ok(leaf.data.get(key_ref).cloned())
    }

    /// Insert a key to a new value, returning the last value if it
    /// was set.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # let config = sled::Config::tmp().unwrap();
    /// # let db: sled::Db<64, 1024, 128> = config.open()?;
    /// assert_eq!(db.insert(&[1, 2, 3], vec![0]).unwrap(), None);
    /// assert_eq!(db.insert(&[1, 2, 3], vec![1]).unwrap(), Some(sled::InlineArray::from(&[0])));
    /// # Ok(()) }
    /// ```
    #[doc(alias = "set")]
    #[doc(alias = "put")]
    pub fn insert<K, V>(
        &self,
        key: K,
        value: V,
    ) -> io::Result<Option<InlineArray>>
    where
        K: AsRef<[u8]>,
        V: Into<InlineArray>,
    {
        self.check_error()?;

        let key_ref = key.as_ref();

        let value_ivec = value.into();
        let mut leaf_guard = self.leaf_for_key_mut(key_ref)?;
        let new_epoch = leaf_guard.flush_epoch_guard.epoch();

        let leaf = leaf_guard.leaf_write.as_mut().unwrap();

        // TODO handle prefix encoding

        let ret = leaf.data.insert(key_ref.into(), value_ivec.clone());

        let old_size = ret.as_ref().map(|v| key_ref.len() + v.len()).unwrap_or(0);
        let new_size = key_ref.len() + value_ivec.len();

        if new_size > old_size {
            leaf.in_memory_size += new_size - old_size;
        } else {
            leaf.in_memory_size =
                leaf.in_memory_size.saturating_sub(old_size - new_size);
        }

        let split = leaf.split_if_full(new_epoch, &self.store);
        if split.is_some() || Some(value_ivec) != ret {
            leaf.dirty_flush_epoch = Some(new_epoch);
            self.dirty.insert((new_epoch, leaf_guard.low_key.clone()), None);
        }
        if let Some((split_key, rhs_node)) = split {
            self.dirty.insert((new_epoch, split_key.clone()), None);
            self.node_id_to_low_key_index
                .insert(rhs_node.id.0, split_key.clone());
            self.index.insert(split_key, rhs_node);
        }

        Ok(ret)
    }

    /// Delete a value, returning the old value if it existed.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # let config = sled::Config::tmp().unwrap();
    /// # let db: sled::Db<64, 1024, 128> = config.open()?;
    /// db.insert(&[1], vec![1]);
    /// assert_eq!(db.remove(&[1]).unwrap(), Some(sled::InlineArray::from(vec![1])));
    /// assert!(db.remove(&[1]).unwrap().is_none());
    /// # Ok(()) }
    /// ```
    #[doc(alias = "delete")]
    #[doc(alias = "del")]
    pub fn remove<K: AsRef<[u8]>>(
        &self,
        key: K,
    ) -> io::Result<Option<InlineArray>> {
        self.check_error()?;

        let key_ref = key.as_ref();

        let mut leaf_guard = self.leaf_for_key_mut(key_ref)?;
        let new_epoch = leaf_guard.flush_epoch_guard.epoch();

        let leaf = leaf_guard.leaf_write.as_mut().unwrap();

        // TODO handle prefix encoding

        let ret = leaf.data.remove(key_ref);

        if ret.is_some() {
            leaf.dirty_flush_epoch = Some(new_epoch);
            self.dirty.insert((new_epoch, leaf_guard.low_key.clone()), None);
        }

        Ok(ret)
    }

    /// Synchronously flushes all dirty IO buffers and calls
    /// fsync. If this succeeds, it is guaranteed that all
    /// previous writes will be recovered if the system
    /// crashes. Returns the number of bytes flushed during
    /// this call.
    ///
    /// Flushing can take quite a lot of time, and you should
    /// measure the performance impact of using it on
    /// realistic sustained workloads running on realistic
    /// hardware.
    ///
    /// This is called automatically on drop of the last open Db
    /// instance.
    pub fn flush(&self) -> io::Result<()> {
        let mut write_batch = vec![];

        let (
            previous_flush_complete_notifier,
            previous_vacant_notifier,
            forward_flush_notifier,
        ) = self.flush_epoch.roll_epoch_forward();

        previous_flush_complete_notifier.wait_for_complete();
        let flush_through_epoch: NonZeroU64 =
            previous_vacant_notifier.wait_for_complete();

        let flush_boundary = (
            NonZeroU64::new(flush_through_epoch.get() + 1).unwrap(),
            InlineArray::default(),
        );
        for ((epoch, low_key), _) in self.dirty.range(..flush_boundary) {
            if let Some(node) = self.index.get(&*low_key) {
                let mut lock = node.inner.write();

                assert_eq!(
                    epoch,
                    flush_through_epoch,
                    "{:?} is dirty for old epoch {}",
                    node.id,
                    epoch.get()
                );

                let serialized_value_opt = self
                    .dirty
                    .remove(&(epoch, low_key.clone()))
                    .expect("violation of flush responsibility");

                let leaf_bytes: Vec<u8> =
                    if let Some(serialized_value) = serialized_value_opt {
                        if Arc::strong_count(&serialized_value) == 1 {
                            Arc::into_inner(serialized_value).unwrap()
                        } else {
                            serialized_value.to_vec()
                        }
                    } else {
                        let leaf_ref: &mut Leaf<LEAF_FANOUT> =
                            lock.as_mut().unwrap();
                        let dirty_epoch =
                            leaf_ref.dirty_flush_epoch.take().unwrap();
                        assert_eq!(epoch, dirty_epoch);
                        // ugly but basically free
                        leaf_ref.serialize(self.config.zstd_compression_level)
                    };

                drop(lock);
                // println!("node id {} is dirty", node.id.0);
                write_batch.push((
                    node.id.0,
                    Some((InlineArray::from(&*low_key), leaf_bytes)),
                ));
            } else {
                continue;
            };
        }

        let written_count = write_batch.len();
        if written_count > 0 {
            self.store.write_batch(write_batch)?;
            log::trace!(
                "marking epoch {} as flushed - {} objects written",
                flush_through_epoch.get(),
                written_count
            );
        }
        forward_flush_notifier.mark_complete();
        self.store.maintenance()?;
        Ok(())
    }

    /// Compare and swap. Capable of unique creation, conditional modification,
    /// or deletion. If old is `None`, this will only set the value if it
    /// doesn't exist yet. If new is `None`, will delete the value if old is
    /// correct. If both old and new are `Some`, will modify the value if
    /// old is correct.
    ///
    /// It returns `Ok(Ok(CompareAndSwapSuccess { new_value, previous_value }))` if operation finishes successfully.
    ///
    /// If it fails it returns:
    ///     - `Ok(Err(CompareAndSwapError{ current, proposed }))` if no IO
    ///       error was encountered but the operation
    ///       failed to specify the correct current value. `CompareAndSwapError` contains
    ///       current and proposed values.
    ///     - `Err(io::Error)` if there was a high-level IO problem that prevented
    ///       the operation from logically progressing. This is usually fatal and
    ///       will prevent future requests from functioning, and requires the
    ///       administrator to fix the system issue before restarting.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # let config = sled::Config::tmp().unwrap();
    /// # let db: sled::Db<64, 1024, 128> = config.open()?;
    /// // unique creation
    /// assert!(
    ///     db.compare_and_swap(&[1], None as Option<&[u8]>, Some(&[10])).unwrap().is_ok(),
    /// );
    ///
    /// // conditional modification
    /// assert!(
    ///     db.compare_and_swap(&[1], Some(&[10]), Some(&[20])).unwrap().is_ok(),
    /// );
    ///
    /// // failed conditional modification -- the current value is returned in
    /// // the error variant
    /// let operation = db.compare_and_swap(&[1], Some(&[30]), Some(&[40]));
    /// assert!(operation.is_ok()); // the operation succeeded
    /// let modification = operation.unwrap();
    /// assert!(modification.is_err());
    /// let actual_value = modification.unwrap_err();
    /// assert_eq!(actual_value.current.map(|ivec| ivec.to_vec()), Some(vec![20]));
    ///
    /// // conditional deletion
    /// assert!(
    ///     db.compare_and_swap(&[1], Some(&[20]), None as Option<&[u8]>).unwrap().is_ok(),
    /// );
    /// assert!(db.get(&[1]).unwrap().is_none());
    /// # Ok(()) }
    /// ```
    #[doc(alias = "cas")]
    #[doc(alias = "tas")]
    #[doc(alias = "test_and_swap")]
    #[doc(alias = "compare_and_set")]
    pub fn compare_and_swap<K, OV, NV>(
        &self,
        key: K,
        old: Option<OV>,
        new: Option<NV>,
    ) -> CompareAndSwapResult
    where
        K: AsRef<[u8]>,
        OV: AsRef<[u8]>,
        NV: Into<InlineArray>,
    {
        self.check_error()?;

        let key_ref = key.as_ref();

        let mut leaf_guard = self.leaf_for_key_mut(key_ref)?;
        let new_epoch = leaf_guard.epoch();

        let proposed: Option<InlineArray> = new.map(Into::into);

        let leaf = leaf_guard.leaf_write.as_mut().unwrap();

        // TODO handle prefix encoding

        let current = leaf.data.get(key_ref).cloned();

        let previous_matches = match (old, &current) {
            (None, None) => true,
            (Some(conditional), Some(current))
                if conditional.as_ref() == current.as_ref() =>
            {
                true
            }
            _ => false,
        };

        let ret = if previous_matches {
            if let Some(ref new_value) = proposed {
                leaf.data.insert(key_ref.into(), new_value.clone())
            } else {
                leaf.data.remove(key_ref)
            };

            Ok(CompareAndSwapSuccess {
                new_value: proposed,
                previous_value: current,
            })
        } else {
            Err(CompareAndSwapError { current, proposed })
        };

        let split = leaf.split_if_full(new_epoch, &self.store);
        if split.is_some() || ret.is_ok() {
            leaf.dirty_flush_epoch = Some(new_epoch);
            self.dirty.insert((new_epoch, leaf_guard.low_key.clone()), None);
        }
        if let Some((split_key, rhs_node)) = split {
            self.dirty.insert((new_epoch, split_key.clone()), None);
            self.node_id_to_low_key_index
                .insert(rhs_node.id.0, split_key.clone());
            self.index.insert(split_key, rhs_node);
        }

        Ok(ret)
    }

    /// Fetch the value, apply a function to it and return the result.
    ///
    /// # Note
    ///
    /// This may call the function multiple times if the value has been
    /// changed from other threads in the meantime.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sled::{Config, InlineArray};
    ///
    /// let config = Config::tmp().unwrap();
    /// let db: sled::Db<64, 1024, 128> = config.open()?;
    ///
    /// fn u64_to_ivec(number: u64) -> InlineArray {
    ///     InlineArray::from(number.to_be_bytes().to_vec())
    /// }
    ///
    /// let zero = u64_to_ivec(0);
    /// let one = u64_to_ivec(1);
    /// let two = u64_to_ivec(2);
    /// let three = u64_to_ivec(3);
    ///
    /// fn increment(old: Option<&[u8]>) -> Option<Vec<u8>> {
    ///     let number = match old {
    ///         Some(bytes) => {
    ///             let array: [u8; 8] = bytes.try_into().unwrap();
    ///             let number = u64::from_be_bytes(array);
    ///             number + 1
    ///         }
    ///         None => 0,
    ///     };
    ///
    ///     Some(number.to_be_bytes().to_vec())
    /// }
    ///
    /// assert_eq!(db.update_and_fetch("counter", increment).unwrap(), Some(zero));
    /// assert_eq!(db.update_and_fetch("counter", increment).unwrap(), Some(one));
    /// assert_eq!(db.update_and_fetch("counter", increment).unwrap(), Some(two));
    /// assert_eq!(db.update_and_fetch("counter", increment).unwrap(), Some(three));
    /// # Ok(()) }
    /// ```
    pub fn update_and_fetch<K, V, F>(
        &self,
        key: K,
        mut f: F,
    ) -> io::Result<Option<InlineArray>>
    where
        K: AsRef<[u8]>,
        F: FnMut(Option<&[u8]>) -> Option<V>,
        V: Into<InlineArray>,
    {
        let key_ref = key.as_ref();
        let mut current = self.get(key_ref)?;

        loop {
            let tmp = current.as_ref().map(AsRef::as_ref);
            let next = f(tmp).map(Into::into);
            match self.compare_and_swap::<_, _, InlineArray>(
                key_ref,
                tmp,
                next.clone(),
            )? {
                Ok(_) => return Ok(next),
                Err(CompareAndSwapError { current: cur, .. }) => {
                    current = cur;
                }
            }
        }
    }

    /// Fetch the value, apply a function to it and return the previous value.
    ///
    /// # Note
    ///
    /// This may call the function multiple times if the value has been
    /// changed from other threads in the meantime.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sled::{Config, InlineArray};
    ///
    /// let config = Config::tmp().unwrap();
    /// let db: sled::Db<64, 1024, 128> = config.open()?;
    ///
    /// fn u64_to_ivec(number: u64) -> InlineArray {
    ///     InlineArray::from(number.to_be_bytes().to_vec())
    /// }
    ///
    /// let zero = u64_to_ivec(0);
    /// let one = u64_to_ivec(1);
    /// let two = u64_to_ivec(2);
    ///
    /// fn increment(old: Option<&[u8]>) -> Option<Vec<u8>> {
    ///     let number = match old {
    ///         Some(bytes) => {
    ///             let array: [u8; 8] = bytes.try_into().unwrap();
    ///             let number = u64::from_be_bytes(array);
    ///             number + 1
    ///         }
    ///         None => 0,
    ///     };
    ///
    ///     Some(number.to_be_bytes().to_vec())
    /// }
    ///
    /// assert_eq!(db.fetch_and_update("counter", increment).unwrap(), None);
    /// assert_eq!(db.fetch_and_update("counter", increment).unwrap(), Some(zero));
    /// assert_eq!(db.fetch_and_update("counter", increment).unwrap(), Some(one));
    /// assert_eq!(db.fetch_and_update("counter", increment).unwrap(), Some(two));
    /// # Ok(()) }
    /// ```
    pub fn fetch_and_update<K, V, F>(
        &self,
        key: K,
        mut f: F,
    ) -> io::Result<Option<InlineArray>>
    where
        K: AsRef<[u8]>,
        F: FnMut(Option<&[u8]>) -> Option<V>,
        V: Into<InlineArray>,
    {
        let key_ref = key.as_ref();
        let mut current = self.get(key_ref)?;

        loop {
            let tmp = current.as_ref().map(AsRef::as_ref);
            let next = f(tmp);
            match self.compare_and_swap(key_ref, tmp, next)? {
                Ok(_) => return Ok(current),
                Err(CompareAndSwapError { current: cur, .. }) => {
                    current = cur;
                }
            }
        }
    }

    pub fn iter(
        &self,
    ) -> Iter<INDEX_FANOUT, LEAF_FANOUT, EBR_LOCAL_GC_BUFFER_SIZE> {
        Iter {
            prefetched: VecDeque::new(),
            prefetched_back: VecDeque::new(),
            next_fetch: None,
            next_fetch_back: None,
            next_calls: 0,
            next_back_calls: 0,
            inner: self,
            bounds: (Bound::Unbounded, Bound::Unbounded),
        }
    }

    pub fn range<K, R>(
        &self,
        range: R,
    ) -> Iter<INDEX_FANOUT, LEAF_FANOUT, EBR_LOCAL_GC_BUFFER_SIZE>
    where
        K: AsRef<[u8]>,
        R: RangeBounds<K>,
    {
        let start: Bound<InlineArray> =
            map_bound(range.start_bound(), |b| InlineArray::from(b.as_ref()));
        let end: Bound<InlineArray> =
            map_bound(range.end_bound(), |b| InlineArray::from(b.as_ref()));

        Iter {
            prefetched: VecDeque::new(),
            prefetched_back: VecDeque::new(),
            next_fetch: None,
            next_fetch_back: None,
            next_calls: 0,
            next_back_calls: 0,
            inner: self,
            bounds: (start, end),
        }
    }

    /// Create a new batched update that is applied
    /// atomically. Readers will atomically see all updates
    /// at an atomic instant, and if the database crashes,
    /// either 0% or 100% of the full batch will be recovered,
    /// but never a partial batch. If a `flush` operation succeeds
    /// after this, it is guaranteed that 100% of the batch will be
    /// visible, unless later concurrent updates changed the values
    /// before the flush.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # let _ = std::fs::remove_dir_all("batch_doctest");
    /// # let db: sled::Db<64, 1024, 128> = sled::open_default("batch_doctest")?;
    /// db.insert("key_0", "val_0")?;
    ///
    /// let mut batch = sled::Batch::default();
    /// batch.insert("key_a", "val_a");
    /// batch.insert("key_b", "val_b");
    /// batch.insert("key_c", "val_c");
    /// batch.remove("key_0");
    ///
    /// db.apply_batch(batch)?;
    /// // key_0 no longer exists, and key_a, key_b, and key_c
    /// // now do exist.
    /// # let _ = std::fs::remove_dir_all("batch_doctest");
    /// # Ok(()) }
    /// ```
    pub fn apply_batch(&self, batch: Batch) -> io::Result<()> {
        // NB: we rely on lexicographic lock acquisition
        // by iterating over the batch's BTreeMap to avoid
        // deadlocks during 2PL
        let mut acquired_locks: BTreeMap<
            InlineArray,
            (
                ArcRwLockWriteGuard<RawRwLock, Option<Box<Leaf<LEAF_FANOUT>>>>,
                NodeId,
            ),
        > = BTreeMap::new();

        // Phase 1: lock acquisition
        let mut last: Option<(
            InlineArray,
            ArcRwLockWriteGuard<RawRwLock, Option<Box<Leaf<LEAF_FANOUT>>>>,
            NodeId,
        )> = None;

        for key in batch.writes.keys() {
            if let Some((_lo, w, _id)) = &last {
                let leaf = w.as_ref().unwrap();
                assert!(&leaf.lo <= key);
                if let Some(hi) = &leaf.hi {
                    if hi <= key {
                        let (lo, w, id) = last.take().unwrap();
                        acquired_locks.insert(lo, (w, id));
                    }
                }
            }
            if last.is_none() {
                last = Some(self.page_in(key)?);
            }
        }

        if let Some((lo, w, id)) = last.take() {
            acquired_locks.insert(lo, (w, id));
        }

        // NB: add the flush epoch at the end of the lock acquisition
        // process when all locks have been acquired, to avoid situations
        // where a leaf is already dirty with an epoch "from the future".
        let flush_epoch_guard = self.flush_epoch.check_in();
        let new_epoch = flush_epoch_guard.epoch();

        // Flush any leaves that are dirty from a previous flush epoch
        // before performing operations.
        for (write, node_id) in acquired_locks.values_mut() {
            let leaf = write.as_mut().unwrap();
            if let Some(old_flush_epoch) = leaf.dirty_flush_epoch {
                if old_flush_epoch != new_epoch {
                    assert_eq!(old_flush_epoch.get() + 1, new_epoch.get());

                    log::trace!(
                        "cooperatively flushing {:?} with dirty epoch {} after checking into epoch {}",
                        node_id,
                        old_flush_epoch.get(),
                        new_epoch.get()
                    );
                    // cooperatively serialize and put into dirty
                    let dirty_epoch = leaf.dirty_flush_epoch.take().unwrap();

                    // be extra-explicit about serialized bytes
                    let leaf_ref: &Leaf<LEAF_FANOUT> = &*leaf;

                    let serialized =
                        leaf_ref.serialize(self.config.zstd_compression_level);

                    self.dirty.insert(
                        (dirty_epoch, leaf.lo.clone()),
                        Some(Arc::new(serialized)),
                    );
                }
            }
        }

        let mut splits: Vec<(InlineArray, Node<LEAF_FANOUT>)> = vec![];

        // Insert and split when full
        for (key, value_opt) in batch.writes {
            let range = ..=&key;
            let (_lo, (ref mut w, _id)) = acquired_locks
                .range_mut::<InlineArray, _>(range)
                .next_back()
                .unwrap();
            let leaf = w.as_mut().unwrap();

            assert!(leaf.lo <= key);
            if let Some(hi) = &leaf.hi {
                assert!(hi > &key);
            }

            if let Some(value) = value_opt {
                leaf.data.insert(key, value);

                if let Some((split_key, rhs_node)) =
                    leaf.split_if_full(new_epoch, &self.store)
                {
                    let node_id = rhs_node.id;
                    let write = rhs_node.inner.write_arc();
                    assert!(write.is_some());

                    splits.push((split_key.clone(), rhs_node));
                    acquired_locks.insert(split_key, (write, node_id));
                }
            } else {
                leaf.data.remove(&key);
            }
        }

        // Make splits globally visible
        for (split_key, rhs_node) in splits {
            self.node_id_to_low_key_index
                .insert(rhs_node.id.0, split_key.clone());
            self.index.insert(split_key, rhs_node);
        }

        // Drop locks
        drop(acquired_locks);

        Ok(())
    }

    /// Returns `true` if the `Tree` contains a value for
    /// the specified key.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # let config = sled::Config::tmp().unwrap();
    /// # let db: sled::Db<64, 1024, 128> = config.open()?;
    /// db.insert(&[0], vec![0])?;
    /// assert!(db.contains_key(&[0])?);
    /// assert!(!db.contains_key(&[1])?);
    /// # Ok(()) }
    /// ```
    pub fn contains_key<K: AsRef<[u8]>>(&self, key: K) -> io::Result<bool> {
        self.get(key).map(|v| v.is_some())
    }

    /// Retrieve the key and value before the provided key,
    /// if one exists.
    ///
    /// # Note
    /// The order follows the Ord implementation for `Vec<u8>`:
    ///
    /// `[] < [0] < [255] < [255, 0] < [255, 255] ...`
    ///
    /// To retain the ordering of numerical types use big endian reprensentation
    ///
    /// # Examples
    ///
    /// ```
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sled::InlineArray;
    /// # let config = sled::Config::tmp().unwrap();
    /// # let db: sled::Db<64, 1024, 128> = config.open()?;
    /// for i in 0..10 {
    ///     db.insert(&[i], vec![i])
    ///         .expect("should write successfully");
    /// }
    ///
    /// assert!(db.get_lt(&[]).unwrap().is_none());
    /// assert!(db.get_lt(&[0]).unwrap().is_none());
    /// assert_eq!(
    ///     db.get_lt(&[1]).unwrap(),
    ///     Some((InlineArray::from(&[0]), InlineArray::from(&[0])))
    /// );
    /// assert_eq!(
    ///     db.get_lt(&[9]).unwrap(),
    ///     Some((InlineArray::from(&[8]), InlineArray::from(&[8])))
    /// );
    /// assert_eq!(
    ///     db.get_lt(&[10]).unwrap(),
    ///     Some((InlineArray::from(&[9]), InlineArray::from(&[9])))
    /// );
    /// assert_eq!(
    ///     db.get_lt(&[255]).unwrap(),
    ///     Some((InlineArray::from(&[9]), InlineArray::from(&[9])))
    /// );
    /// # Ok(()) }
    /// ```
    pub fn get_lt<K>(
        &self,
        key: K,
    ) -> io::Result<Option<(InlineArray, InlineArray)>>
    where
        K: AsRef<[u8]>,
    {
        #[cfg(feature = "metrics")]
        let _measure = Measure::new(&M.tree_get);
        self.range(..key).next_back().transpose()
    }

    /// Retrieve the next key and value from the `Tree` after the
    /// provided key.
    ///
    /// # Note
    /// The order follows the Ord implementation for `Vec<u8>`:
    ///
    /// `[] < [0] < [255] < [255, 0] < [255, 255] ...`
    ///
    /// To retain the ordering of numerical types use big endian reprensentation
    ///
    /// # Examples
    ///
    /// ```
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sled::InlineArray;
    /// # let config = sled::Config::tmp().unwrap();
    /// # let db: sled::Db<64, 1024, 128> = config.open()?;
    /// for i in 0..10 {
    ///     db.insert(&[i], vec![i])?;
    /// }
    ///
    /// assert_eq!(
    ///     db.get_gt(&[]).unwrap(),
    ///     Some((InlineArray::from(&[0]), InlineArray::from(&[0])))
    /// );
    /// assert_eq!(
    ///     db.get_gt(&[0]).unwrap(),
    ///     Some((InlineArray::from(&[1]), InlineArray::from(&[1])))
    /// );
    /// assert_eq!(
    ///     db.get_gt(&[1]).unwrap(),
    ///     Some((InlineArray::from(&[2]), InlineArray::from(&[2])))
    /// );
    /// assert_eq!(
    ///     db.get_gt(&[8]).unwrap(),
    ///     Some((InlineArray::from(&[9]), InlineArray::from(&[9])))
    /// );
    /// assert!(db.get_gt(&[9]).unwrap().is_none());
    ///
    /// db.insert(500u16.to_be_bytes(), vec![10]);
    /// assert_eq!(
    ///     db.get_gt(&499u16.to_be_bytes()).unwrap(),
    ///     Some((InlineArray::from(&500u16.to_be_bytes()), InlineArray::from(&[10])))
    /// );
    /// # Ok(()) }
    /// ```
    pub fn get_gt<K>(
        &self,
        key: K,
    ) -> io::Result<Option<(InlineArray, InlineArray)>>
    where
        K: AsRef<[u8]>,
    {
        #[cfg(feature = "metrics")]
        let _measure = Measure::new(&M.tree_get);
        self.range((ops::Bound::Excluded(key), ops::Bound::Unbounded))
            .next()
            .transpose()
    }

    /// Create an iterator over tuples of keys and values
    /// where all keys start with the given prefix.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # let config = sled::Config::tmp().unwrap();
    /// # let db: sled::Db<64, 1024, 128> = config.open()?;
    /// use sled::InlineArray;
    /// db.insert(&[0, 0, 0], vec![0, 0, 0])?;
    /// db.insert(&[0, 0, 1], vec![0, 0, 1])?;
    /// db.insert(&[0, 0, 2], vec![0, 0, 2])?;
    /// db.insert(&[0, 0, 3], vec![0, 0, 3])?;
    /// db.insert(&[0, 1, 0], vec![0, 1, 0])?;
    /// db.insert(&[0, 1, 1], vec![0, 1, 1])?;
    ///
    /// let prefix: &[u8] = &[0, 0];
    /// let mut r = db.scan_prefix(prefix);
    /// assert_eq!(
    ///     r.next().unwrap().unwrap(),
    ///     (InlineArray::from(&[0, 0, 0]), InlineArray::from(&[0, 0, 0]))
    /// );
    /// assert_eq!(
    ///     r.next().unwrap().unwrap(),
    ///     (InlineArray::from(&[0, 0, 1]), InlineArray::from(&[0, 0, 1]))
    /// );
    /// assert_eq!(
    ///     r.next().unwrap().unwrap(),
    ///     (InlineArray::from(&[0, 0, 2]), InlineArray::from(&[0, 0, 2]))
    /// );
    /// assert_eq!(
    ///     r.next().unwrap().unwrap(),
    ///     (InlineArray::from(&[0, 0, 3]), InlineArray::from(&[0, 0, 3]))
    /// );
    /// assert!(r.next().is_none());
    /// # Ok(()) }
    /// ```
    pub fn scan_prefix<'a, P>(
        &'a self,
        prefix: P,
    ) -> Iter<'a, INDEX_FANOUT, LEAF_FANOUT, EBR_LOCAL_GC_BUFFER_SIZE>
    where
        P: AsRef<[u8]>,
    {
        let prefix_ref = prefix.as_ref();
        let mut upper = prefix_ref.to_vec();

        while let Some(last) = upper.pop() {
            if last < u8::MAX {
                upper.push(last + 1);
                return self.range(prefix_ref..&upper);
            }
        }

        self.range(prefix..)
    }

    /// Returns the first key and value in the `Tree`, or
    /// `None` if the `Tree` is empty.
    pub fn first(&self) -> io::Result<Option<(InlineArray, InlineArray)>> {
        self.iter().next().transpose()
    }

    /// Returns the last key and value in the `Tree`, or
    /// `None` if the `Tree` is empty.
    pub fn last(&self) -> io::Result<Option<(InlineArray, InlineArray)>> {
        self.iter().next_back().transpose()
    }

    /// Atomically removes the maximum item in the `Tree` instance.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # let config = sled::Config::tmp().unwrap();
    /// # let db: sled::Db<64, 1024, 128> = config.open()?;
    /// db.insert(&[0], vec![0])?;
    /// db.insert(&[1], vec![10])?;
    /// db.insert(&[2], vec![20])?;
    /// db.insert(&[3], vec![30])?;
    /// db.insert(&[4], vec![40])?;
    /// db.insert(&[5], vec![50])?;
    ///
    /// assert_eq!(&db.pop_last()?.unwrap().0, &[5]);
    /// assert_eq!(&db.pop_last()?.unwrap().0, &[4]);
    /// assert_eq!(&db.pop_last()?.unwrap().0, &[3]);
    /// assert_eq!(&db.pop_last()?.unwrap().0, &[2]);
    /// assert_eq!(&db.pop_last()?.unwrap().0, &[1]);
    /// assert_eq!(&db.pop_last()?.unwrap().0, &[0]);
    /// assert_eq!(db.pop_last()?, None);
    /// # Ok(()) }
    /// ```
    pub fn pop_last(&self) -> io::Result<Option<(InlineArray, InlineArray)>> {
        loop {
            if let Some(first_res) = self.iter().next_back() {
                let first = first_res?;
                if self
                    .compare_and_swap::<_, _, &[u8]>(
                        &first.0,
                        Some(&first.1),
                        None,
                    )?
                    .is_ok()
                {
                    log::trace!("pop_last removed item {:?}", first);
                    return Ok(Some(first));
                }
            // try again
            } else {
                log::trace!("pop_last removed nothing from empty tree");
                return Ok(None);
            }
        }
    }

    /// Pops the last kv pair in the provided range, or returns `Ok(None)` if nothing
    /// exists within that range.
    ///
    /// # Panics
    ///
    /// This will panic if the provided range's end_bound() == Bound::Excluded(K::MIN).
    ///
    /// # Examples
    ///
    /// ```
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # let config = sled::Config::tmp().unwrap();
    /// # let db: sled::Db<64, 1024, 128> = config.open()?;
    ///
    /// let data = vec![
    ///     (b"key 1", b"value 1"),
    ///     (b"key 2", b"value 2"),
    ///     (b"key 3", b"value 3")
    /// ];
    ///
    /// for (k, v) in data {
    ///     db.insert(k, v).unwrap();
    /// }
    ///
    /// let r1 = db.pop_last_in_range(b"key 1".as_ref()..=b"key 3").unwrap();
    /// assert_eq!(Some((b"key 3".into(), b"value 3".into())), r1);
    ///
    /// let r2 = db.pop_last_in_range(b"key 1".as_ref()..b"key 3").unwrap();
    /// assert_eq!(Some((b"key 2".into(), b"value 2".into())), r2);
    ///
    /// let r3 = db.pop_last_in_range(b"key 4".as_ref()..).unwrap();
    /// assert!(r3.is_none());
    ///
    /// let r4 = db.pop_last_in_range(b"key 2".as_ref()..=b"key 3").unwrap();
    /// assert!(r4.is_none());
    ///
    /// let r5 = db.pop_last_in_range(b"key 0".as_ref()..=b"key 3").unwrap();
    /// assert_eq!(Some((b"key 1".into(), b"value 1".into())), r5);
    ///
    /// let r6 = db.pop_last_in_range(b"key 0".as_ref()..=b"key 3").unwrap();
    /// assert!(r6.is_none());
    /// # Ok (()) }
    /// ```
    pub fn pop_last_in_range<K, R>(
        &self,
        range: R,
    ) -> io::Result<Option<(InlineArray, InlineArray)>>
    where
        K: AsRef<[u8]>,
        R: Clone + RangeBounds<K>,
    {
        loop {
            let mut r = self.range(range.clone());
            let (k, v) = if let Some(kv_res) = r.next() {
                kv_res?
            } else {
                return Ok(None);
            };
            if self
                .compare_and_swap(&k, Some(&v), None as Option<InlineArray>)?
                .is_ok()
            {
                return Ok(Some((k, v)));
            }
        }
    }

    /// Atomically removes the minimum item in the `Tree` instance.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # let config = sled::Config::tmp().unwrap();
    /// # let db: sled::Db<64, 1024, 128> = config.open()?;
    /// db.insert(&[0], vec![0])?;
    /// db.insert(&[1], vec![10])?;
    /// db.insert(&[2], vec![20])?;
    /// db.insert(&[3], vec![30])?;
    /// db.insert(&[4], vec![40])?;
    /// db.insert(&[5], vec![50])?;
    ///
    /// assert_eq!(&db.pop_first()?.unwrap().0, &[0]);
    /// assert_eq!(&db.pop_first()?.unwrap().0, &[1]);
    /// assert_eq!(&db.pop_first()?.unwrap().0, &[2]);
    /// assert_eq!(&db.pop_first()?.unwrap().0, &[3]);
    /// assert_eq!(&db.pop_first()?.unwrap().0, &[4]);
    /// assert_eq!(&db.pop_first()?.unwrap().0, &[5]);
    /// assert_eq!(db.pop_first()?, None);
    /// # Ok(()) }
    /// ```
    pub fn pop_first(&self) -> io::Result<Option<(InlineArray, InlineArray)>> {
        loop {
            if let Some(first_res) = self.iter().next() {
                let first = first_res?;
                if self
                    .compare_and_swap::<_, _, &[u8]>(
                        &first.0,
                        Some(&first.1),
                        None,
                    )?
                    .is_ok()
                {
                    log::trace!("pop_first removed item {:?}", first);
                    return Ok(Some(first));
                }
            // try again
            } else {
                log::trace!("pop_first removed nothing from empty tree");
                return Ok(None);
            }
        }
    }

    /// Pops the first kv pair in the provided range, or returns `Ok(None)` if nothing
    /// exists within that range.
    ///
    /// # Panics
    ///
    /// This will panic if the provided range's end_bound() == Bound::Excluded(K::MIN).
    ///
    /// # Examples
    ///
    /// ```
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # let config = sled::Config::tmp().unwrap();
    /// # let db: sled::Db<64, 1024, 128> = config.open()?;
    ///
    /// let data = vec![
    ///     (b"key 1", b"value 1"),
    ///     (b"key 2", b"value 2"),
    ///     (b"key 3", b"value 3")
    /// ];
    ///
    /// for (k, v) in data {
    ///     db.insert(k, v).unwrap();
    /// }
    ///
    /// let r1 = db.pop_first_in_range("key 1".as_ref()..="key 3").unwrap();
    /// assert_eq!(Some((b"key 1".into(), b"value 1".into())), r1);
    ///
    /// let r2 = db.pop_first_in_range("key 1".as_ref().."key 3").unwrap();
    /// assert_eq!(Some((b"key 2".into(), b"value 2".into())), r2);
    ///
    /// let r3_res: std::io::Result<Vec<_>> = db.range(b"key 4".as_ref()..).collect();
    /// let r3: Vec<_> = r3_res.unwrap();
    /// assert!(r3.is_empty());
    ///
    /// let r4 = db.pop_first_in_range("key 2".as_ref()..="key 3").unwrap();
    /// assert_eq!(Some((b"key 3".into(), b"value 3".into())), r4);
    /// # Ok (()) }
    /// ```
    pub fn pop_first_in_range<K, R>(
        &self,
        range: R,
    ) -> io::Result<Option<(InlineArray, InlineArray)>>
    where
        K: AsRef<[u8]>,
        R: Clone + RangeBounds<K>,
    {
        loop {
            let mut r = self.range(range.clone());
            let (k, v) = if let Some(kv_res) = r.next() {
                kv_res?
            } else {
                return Ok(None);
            };
            if self
                .compare_and_swap(&k, Some(&v), None as Option<InlineArray>)?
                .is_ok()
            {
                return Ok(Some((k, v)));
            }
        }
    }

    /// Returns the number of elements in this tree.
    ///
    /// Beware: performs a full O(n) scan under the hood.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # let config = sled::Config::tmp().unwrap();
    /// # let db: sled::Db<64, 1024, 128> = config.open()?;
    /// db.insert(b"a", vec![0]);
    /// db.insert(b"b", vec![1]);
    /// assert_eq!(db.len(), 2);
    /// # Ok(()) }
    /// ```
    pub fn len(&self) -> usize {
        self.iter().count()
    }

    /// Returns `true` if the `Tree` contains no elements.
    ///
    /// This is O(1), as we only need to see if an iterator
    /// returns anything for the first call to `next()`.
    pub fn is_empty(&self) -> io::Result<bool> {
        if let Some(res) = self.iter().next() {
            res?;
            Ok(false)
        } else {
            Ok(true)
        }
    }

    /// Clears the `Tree`, removing all values.
    ///
    /// Note that this is not atomic.
    ///
    /// Beware: performs a full O(n) scan under the hood.
    pub fn clear(&self) -> io::Result<()> {
        for k in self.iter().keys() {
            let key = k?;
            let _old = self.remove(key)?;
        }
        Ok(())
    }

    /// Returns the CRC32 of all keys and values
    /// in this Tree.
    ///
    /// This is O(N) and locks the underlying tree
    /// for the duration of the entire scan.
    pub fn checksum(&self) -> io::Result<u32> {
        let mut hasher = crc32fast::Hasher::new();
        for kv_res in self.iter() {
            let (k, v) = kv_res?;
            hasher.update(&k);
            hasher.update(&v);
        }
        Ok(hasher.finalize())
    }
}

#[allow(unused)]
pub struct Iter<
    'a,
    const INDEX_FANOUT: usize,
    const LEAF_FANOUT: usize,
    const EBR_LOCAL_GC_BUFFER_SIZE: usize,
> {
    inner: &'a Db<INDEX_FANOUT, LEAF_FANOUT, EBR_LOCAL_GC_BUFFER_SIZE>,
    bounds: (Bound<InlineArray>, Bound<InlineArray>),
    next_calls: usize,
    next_back_calls: usize,
    next_fetch: Option<InlineArray>,
    next_fetch_back: Option<InlineArray>,
    prefetched: VecDeque<(InlineArray, InlineArray)>,
    prefetched_back: VecDeque<(InlineArray, InlineArray)>,
}

impl<
        'a,
        const INDEX_FANOUT: usize,
        const LEAF_FANOUT: usize,
        const EBR_LOCAL_GC_BUFFER_SIZE: usize,
    > Iterator
    for Iter<'a, INDEX_FANOUT, LEAF_FANOUT, EBR_LOCAL_GC_BUFFER_SIZE>
{
    type Item = io::Result<(InlineArray, InlineArray)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.prefetched.is_empty() {
            let search_key = if let Some(last) = &self.next_fetch {
                last.clone()
            } else {
                match &self.bounds.0 {
                    Bound::Included(b) | Bound::Excluded(b) => b.clone(),
                    Bound::Unbounded => InlineArray::MIN,
                }
            };

            let node = match self.inner.leaf_for_key(&search_key) {
                Ok(n) => n,
                Err(e) => return Some(Err(e)),
            };

            let leaf = node.leaf_read.as_ref().unwrap();
            for (k, v) in leaf.data.iter() {
                if self.bounds.contains(k) {
                    self.prefetched.push_back((k.clone(), v.clone()));
                }
            }
            if self.prefetched.is_empty() {
                return None;
            }
            self.next_fetch = leaf.hi.clone();
        }

        self.prefetched.pop_front().map(Ok)
    }
}

impl<
        'a,
        const INDEX_FANOUT: usize,
        const LEAF_FANOUT: usize,
        const EBR_LOCAL_GC_BUFFER_SIZE: usize,
    > DoubleEndedIterator
    for Iter<'a, INDEX_FANOUT, LEAF_FANOUT, EBR_LOCAL_GC_BUFFER_SIZE>
{
    fn next_back(&mut self) -> Option<Self::Item> {
        todo!()
    }
}

impl<
        'a,
        const INDEX_FANOUT: usize,
        const LEAF_FANOUT: usize,
        const EBR_LOCAL_GC_BUFFER_SIZE: usize,
    > Iter<'a, INDEX_FANOUT, LEAF_FANOUT, EBR_LOCAL_GC_BUFFER_SIZE>
{
    pub fn keys(
        self,
    ) -> impl 'a + DoubleEndedIterator<Item = io::Result<InlineArray>> {
        self.into_iter().map(|kv_res| kv_res.map(|(k, _v)| k))
    }

    pub fn values(
        self,
    ) -> impl 'a + DoubleEndedIterator<Item = io::Result<InlineArray>> {
        self.into_iter().map(|kv_res| kv_res.map(|(_k, v)| v))
    }
}

/// A batch of updates that will
/// be applied atomically to the
/// Tree.
///
/// # Examples
///
/// ```
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// use sled::{Batch, open};
///
/// # let _ = std::fs::remove_dir_all("batch_db_2");
/// let db: sled::Db<64, 1024, 128> = open("batch_db_2")?;
/// db.insert("key_0", "val_0")?;
///
/// let mut batch = Batch::default();
/// batch.insert("key_a", "val_a");
/// batch.insert("key_b", "val_b");
/// batch.insert("key_c", "val_c");
/// batch.remove("key_0");
///
/// db.apply_batch(batch)?;
/// // key_0 no longer exists, and key_a, key_b, and key_c
/// // now do exist.
/// # let _ = std::fs::remove_dir_all("batch_db_2");
/// # Ok(()) }
/// ```
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Batch {
    pub(crate) writes:
        std::collections::BTreeMap<InlineArray, Option<InlineArray>>,
}

impl Batch {
    /// Set a key to a new value
    pub fn insert<K, V>(&mut self, key: K, value: V)
    where
        K: Into<InlineArray>,
        V: Into<InlineArray>,
    {
        self.writes.insert(key.into(), Some(value.into()));
    }

    /// Remove a key
    pub fn remove<K>(&mut self, key: K)
    where
        K: Into<InlineArray>,
    {
        self.writes.insert(key.into(), None);
    }

    /// Get a value if it is present in the `Batch`.
    /// `Some(None)` means it's present as a deletion.
    pub fn get<K: AsRef<[u8]>>(&self, k: K) -> Option<Option<&InlineArray>> {
        let inner = self.writes.get(k.as_ref())?;
        Some(inner.as_ref())
    }
}

fn initialize<
    const INDEX_FANOUT: usize,
    const LEAF_FANOUT: usize,
    const EBR_LOCAL_GC_BUFFER_SIZE: usize,
>(
    index_data: &[(u64, InlineArray)],
    first_id_opt: Option<u64>,
) -> ConcurrentMap<
    InlineArray,
    Node<LEAF_FANOUT>,
    INDEX_FANOUT,
    EBR_LOCAL_GC_BUFFER_SIZE,
> {
    if index_data.is_empty() {
        let first_id = first_id_opt.unwrap();
        let first_leaf = Leaf {
            hi: None,
            lo: InlineArray::default(),
            // this does not need to be marked as dirty until it actually
            // receives inserted data
            dirty_flush_epoch: None,
            prefix_length: 0,
            data: StackMap::new(),
            in_memory_size: mem::size_of::<Leaf<LEAF_FANOUT>>(),
        };
        let first_node = Node {
            id: NodeId(first_id),
            inner: Arc::new(Some(Box::new(first_leaf)).into()),
        };
        return [(InlineArray::default(), first_node)].into_iter().collect();
    }

    let ret = ConcurrentMap::default();

    for (id, low_key) in index_data {
        let node = Node { id: NodeId(*id), inner: Arc::new(None.into()) };
        ret.insert(low_key.clone(), node);
    }

    ret
}
