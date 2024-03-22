use crate::error::Error;
use crate::error::Result;
use crate::metric;
use crate::models;
use crate::models::SledMetadata;
use crate::storage::Storage;
use crate::util;

use async_trait::async_trait;
use bytes::Bytes;
use futures::{future, stream, Stream, StreamExt};
use metrics::{describe_histogram, histogram, increment_counter};
use redis::Commands;
use sled::transaction::{TransactionError, TransactionResult};
use sled::Transactional;
use std::convert::AsRef;
use std::convert::TryInto;
use std::fmt;
use std::marker::Send;
use std::path::Path;
use std::str;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::vec::Vec;

/// Datatype of cache size.
/// Note: It is persistent in some database, so changes may not be backward compatible.
pub type CacheSizeType = u64;

pub enum CacheHitMiss {
    Hit,
    Miss,
}

pub enum CacheData {
    TextData(String),
    BytesData(Bytes),
    ByteStream(
        Box<dyn Stream<Item = Result<Bytes>> + Send + Unpin>,
        Option<CacheSizeType>,
    ), // stream and size
}

impl CacheData {
    pub fn len(&self) -> CacheSizeType {
        match &self {
            CacheData::TextData(text) => text.len() as CacheSizeType,
            CacheData::BytesData(bytes) => bytes.len() as CacheSizeType,
            CacheData::ByteStream(_, size) => size.unwrap(),
        }
    }

    pub async fn into_vec_u8(self) -> Vec<u8> {
        match self {
            CacheData::TextData(text) => text.into_bytes(),
            CacheData::BytesData(bytes) => bytes.to_vec(),
            CacheData::ByteStream(stream, _) => {
                let mut vec: Vec<u8> = Vec::new();
                stream
                    .for_each(|item| {
                        vec.append(&mut item.unwrap().to_vec());
                        future::ready(())
                    })
                    .await;
                vec
            }
        }
    }

    pub fn into_byte_stream(self) -> Box<dyn Stream<Item = Result<Bytes>> + Send + Unpin> {
        match self {
            CacheData::TextData(data) => {
                let stream = stream::iter(vec![Ok(Bytes::from(data))]);
                let stream: Box<dyn Stream<Item = Result<Bytes>> + Send + Unpin> = Box::new(stream);
                stream
            }
            CacheData::BytesData(data) => {
                let stream = stream::iter(vec![Ok(data)]);
                Box::new(stream)
            }
            CacheData::ByteStream(stream, _) => stream,
        }
    }
}

impl From<String> for CacheData {
    fn from(s: String) -> CacheData {
        CacheData::TextData(s)
    }
}

impl From<Bytes> for CacheData {
    fn from(bytes: Bytes) -> CacheData {
        CacheData::BytesData(bytes)
    }
}

impl From<Vec<u8>> for CacheData {
    fn from(vec: Vec<u8>) -> CacheData {
        CacheData::BytesData(Bytes::from(vec))
    }
}

impl AsRef<[u8]> for CacheData {
    fn as_ref(&self) -> &[u8] {
        // TODO:
        match &self {
            CacheData::TextData(text) => text.as_ref(),
            CacheData::BytesData(bytes) => bytes.as_ref(),
            _ => unimplemented!(),
        }
    }
}

impl fmt::Debug for CacheData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut f = f.debug_struct("CacheData");
        match &self {
            CacheData::TextData(s) => f.field("TextData", s),
            CacheData::BytesData(b) => f.field("Bytes", b),
            CacheData::ByteStream(_, size) => f.field(
                "ByteStream",
                &format!(
                    "(stream of size {})",
                    size.map(|x| format!("{}", x))
                        .unwrap_or_else(|| "unknown".to_string())
                ),
            ),
        };
        f.finish()
    }
}

/// Cache is a trait that defines the shared beshaviors of all cache policies.
/// - `put`: put a key-value pair into the cache
/// - `get`: get a value from the cache
#[async_trait]
pub trait Cache: Sync + Send {
    async fn put(&mut self, key: &str, entry: CacheData);
    async fn get(&self, key: &str) -> Option<CacheData>;
}

/// `LruMetadataStore` defines required behavior for an LRU cache
pub trait LruMetadataStore: Sync + Send {
    fn get_lru_entry(&self, key: &str) -> CacheHitMiss;
    fn set_lru_entry(&self, key: &str, value: &CacheData);
    /// Run eviction policy if needed, reserve at least `size` for new cache entry.
    /// Return a list of evicted keys.
    fn evict(
        &self,
        new_size: CacheSizeType,
        new_key: &str,
        size_limit: CacheSizeType,
    ) -> Vec<String>;
    fn get_total_size(&self) -> CacheSizeType;
}

/// `TtlMetadataStore` defines required behavior for a TTL cache
pub trait TtlMetadataStore: Sync + Send {
    fn get_ttl_entry(&self, key: &str) -> CacheHitMiss;
    fn set_ttl_entry(&self, key: &str, value: &CacheData, ttl: u64);
    fn spawn_expiration_cleanup_thread(
        &self,
        storage: &Storage,
        pending_close: Arc<AtomicBool>,
    ) -> Result<JoinHandle<()>>;
}

/// Wrapper of an LRU cache object
pub struct LruCache {
    pub size_limit: CacheSizeType,
    metadata_db: Arc<dyn LruMetadataStore>,
    storage: Arc<Storage>,
}

impl LruCache {
    pub fn new(
        size_limit: CacheSizeType,
        metadata_db: Arc<dyn LruMetadataStore>,
        storage: Arc<Storage>,
        metric_id: &str,
    ) -> Self {
        describe_histogram!(
            metric::get_cache_size_metrics_key(metric_id),
            metrics::Unit::Bytes,
            "The size of cache in bytes."
        );
        Self {
            size_limit,
            metadata_db,
            storage,
        }
    }
}

#[async_trait]
impl Cache for LruCache {
    async fn put(&mut self, key: &str, entry: CacheData) {
        let file_size = entry.len() as CacheSizeType;

        if file_size > self.size_limit {
            info!(
                "skip cache for {}, because its size exceeds cache size limit({})",
                key, self.size_limit
            );
            return;
        }
        // Run eviction, set new entry
        let evicted_keys = self.metadata_db.evict(file_size, key, self.size_limit);
        for file in evicted_keys {
            match self.storage.remove(&file).await {
                Ok(_) => {
                    increment_counter!(metric::CNT_RM_FILES);
                    info!("LRU cache removed {}", &file);
                }
                Err(e) => {
                    warn!("failed to remove file: {:?}", e);
                }
            };
        }
        self.metadata_db.set_lru_entry(key, &entry);
        // self.metadata_db.set(key, &mut entry);
        self.storage.persist(key, entry).await;
    }

    async fn get(&self, key: &str) -> Option<CacheData> {
        match self.metadata_db.get_lru_entry(key) {
            CacheHitMiss::Hit => {
                return match self.storage.read(key).await {
                    Ok(data) => {
                        // trace!("CACHE GET [HIT] {} -> {:?} ", redis_key, &cache_result);
                        Some(data)
                    }
                    Err(_) => None,
                };
            }
            CacheHitMiss::Miss => {
                // trace!("CACHE GET [MISS] {} -> {:?} ", redis_key, &cache_result);
                None
            }
        }
    }
}

pub struct TtlCache {
    pub ttl: u64,
    metadata_db: Arc<dyn TtlMetadataStore>,
    storage: Arc<Storage>,
    pub pending_close: Arc<AtomicBool>,
    pub expiration_thread_handler: Option<JoinHandle<()>>,
}

impl TtlCache {
    pub fn new(ttl: u64, metadata_db: Arc<dyn TtlMetadataStore>, storage: Arc<Storage>) -> Self {
        let mut cache = Self {
            ttl,
            metadata_db,
            storage,
            pending_close: Arc::new(AtomicBool::new(false)),
            expiration_thread_handler: None,
        };
        let thread_handler = cache
            .metadata_db
            .spawn_expiration_cleanup_thread(&cache.storage, cache.pending_close.clone())
            .unwrap();
        cache.expiration_thread_handler = Some(thread_handler);
        cache
    }
}

#[async_trait]
impl Cache for TtlCache {
    async fn get(&self, key: &str) -> Option<CacheData> {
        match self.metadata_db.get_ttl_entry(key) {
            CacheHitMiss::Hit => {
                return match self.storage.read(key).await {
                    Ok(data) => {
                        trace!("CACHE GET [HIT] {} -> {:?} ", key, data);
                        Some(data)
                    }
                    Err(_) => None,
                };
            }
            CacheHitMiss::Miss => {
                trace!("CACHE GET [MISS] {}", key);
                None
            }
        }
    }
    async fn put(&mut self, key: &str, entry: CacheData) {
        self.metadata_db.set_ttl_entry(key, &entry, self.ttl);
        self.storage.persist(key, entry).await;
    }
}

pub struct RedisMetadataDb {
    redis_client: redis::Client,
    id: String,
}

impl RedisMetadataDb {
    pub fn new(redis_client: redis::Client, id: &str) -> Self {
        Self {
            redis_client,
            id: id.into(),
        }
    }

    #[allow(clippy::wrong_self_convention)]
    pub fn from_prefixed_key(&self, cache_key: &str) -> String {
        let cache_key = &cache_key[self.id.len() + 1..];
        cache_key.to_string()
    }

    fn to_prefixed_key(&self, cache_key: &str) -> String {
        format!("{}_{}", self.id, cache_key)
    }

    fn total_size_key(&self) -> String {
        self.to_prefixed_key("total_size")
    }

    /// returns the key to the zlist that stores the cache entries
    fn entries_zlist_key(&self) -> String {
        self.to_prefixed_key("cache_keys")
    }

    pub fn get_redis_key(id: &str, cache_key: &str) -> String {
        format!("{}/{}", id, cache_key)
    }

    pub fn from_redis_key(id: &str, key: &str) -> String {
        String::from(&key[id.len() + 1..])
    }
}

impl LruMetadataStore for RedisMetadataDb {
    fn get_lru_entry(&self, key: &str) -> CacheHitMiss {
        let redis_key = &self.to_prefixed_key(key);
        let mut sync_con = models::get_sync_con(&self.redis_client).unwrap();
        let cache_result = models::get_cache_entry(&mut sync_con, redis_key).unwrap();
        match cache_result {
            Some(_) => {
                // cache hit
                // update cache entry in db
                let new_atime = util::now();
                match models::update_cache_entry_atime(
                    &mut sync_con,
                    redis_key,
                    new_atime,
                    &self.entries_zlist_key(),
                ) {
                    Ok(_) => {}
                    Err(e) => {
                        info!("Failed to update cache entry atime: {}", e);
                    }
                }
                trace!("CACHE GET [HIT] {} -> {:?} ", redis_key, &cache_result);
                CacheHitMiss::Hit
            }
            None => {
                trace!("CACHE GET [MISS] {} -> {:?} ", redis_key, &cache_result);
                CacheHitMiss::Miss
            }
        }
    }

    fn set_lru_entry(&self, key: &str, value: &CacheData) {
        let redis_key = &self.to_prefixed_key(key);
        let mut con = models::get_sync_con(&self.redis_client).unwrap();
        let entry = &CacheEntry::new(redis_key, value.len() as CacheSizeType);
        let _redis_resp_str = models::set_lru_cache_entry(
            &mut con,
            redis_key,
            entry,
            &self.total_size_key(),
            &self.entries_zlist_key(),
        );
        trace!("CACHE SET {} -> {:?}", &redis_key, value);
    }

    fn evict(
        &self,
        new_size: CacheSizeType,
        new_key: &str,
        size_limit: CacheSizeType,
    ) -> Vec<String> {
        let mut files_to_remove = Vec::new();
        let redis_key = &self.to_prefixed_key(new_key);
        let file_size = new_size;
        let mut sync_con = models::get_sync_con(&self.redis_client).unwrap();
        // evict cache entry if necessary
        let _tx_result = redis::transaction(
            &mut sync_con,
            &[redis_key, &self.total_size_key(), &self.entries_zlist_key()],
            |con, _pipe| {
                let mut cur_cache_size = self.get_total_size();
                while cur_cache_size + file_size > size_limit {
                    // LRU eviction
                    trace!(
                        "current {} + new {} > limit {}",
                        con.get::<&str, Option<CacheSizeType>>(&self.total_size_key())
                            .unwrap()
                            .unwrap_or(0),
                        file_size,
                        size_limit
                    );
                    let pkg_to_remove: Vec<(String, CacheSizeType)> =
                        con.zpopmin(&self.entries_zlist_key(), 1).unwrap();
                    trace!("pkg_to_remove: {:?}", pkg_to_remove);
                    if pkg_to_remove.is_empty() {
                        info!("some files need to be evicted but they are missing from redis filelist. The cache metadata is inconsistent.");
                        return Err(redis::RedisError::from(std::io::Error::new(
                            std::io::ErrorKind::Other,
                            "cache metadata inconsistent",
                        )));
                    }
                    files_to_remove.append(
                        &mut pkg_to_remove
                            .iter()
                            .map(|(k, _)| self.from_prefixed_key(k))
                            .collect(),
                    );
                    // remove metadata in redis
                    for (f, _) in pkg_to_remove {
                        let pkg_size: Option<CacheSizeType> = con.hget(&f, "size").unwrap();
                        let _del_cnt = con.del::<&str, isize>(&f);
                        cur_cache_size = con
                            .decr::<&str, CacheSizeType, CacheSizeType>(
                                &self.total_size_key(),
                                pkg_size.unwrap_or(0),
                            )
                            .unwrap();
                        trace!("total_size -= {:?} -> {}", pkg_size, cur_cache_size);
                    }
                }
                Ok(Some(()))
            },
        );
        files_to_remove
    }

    fn get_total_size(&self) -> CacheSizeType {
        let key = self.total_size_key();
        let mut con = self.redis_client.get_connection().unwrap();
        let size = con
            .get::<&str, Option<CacheSizeType>>(&key)
            .unwrap()
            .unwrap_or(0);
        histogram!(metric::get_cache_size_metrics_key(&self.id), size as f64);
        size
    }
}

impl TtlMetadataStore for RedisMetadataDb {
    fn get_ttl_entry(&self, key: &str) -> CacheHitMiss {
        let redis_key = Self::get_redis_key(&self.id, key);
        let mut sync_con = models::get_sync_con(&self.redis_client).unwrap();
        match models::get(&mut sync_con, &redis_key) {
            Ok(res) => match res {
                Some(_) => CacheHitMiss::Hit,
                None => CacheHitMiss::Miss,
            },
            Err(e) => {
                info!("get cache entry key={} failed: {}", key, e);
                CacheHitMiss::Miss
            }
        }
    }
    fn set_ttl_entry(&self, key: &str, _value: &CacheData, ttl: u64) {
        let redis_key = Self::get_redis_key(&self.id, key);
        let mut sync_con = models::get_sync_con(&self.redis_client).unwrap();
        match models::set(&mut sync_con, &redis_key, "") {
            Ok(_) => {}
            Err(e) => {
                error!("set cache entry for {} failed: {}", key, e);
            }
        }
        match models::expire(&mut sync_con, &redis_key, ttl as usize) {
            Ok(_) => {}
            Err(e) => {
                error!("set cache entry ttl for {} failed: {}", key, e);
            }
        }
        trace!("CACHE SET {} TTL={}", &key, ttl);
    }

    fn spawn_expiration_cleanup_thread(
        &self,
        storage: &Storage,
        pending_close: Arc<AtomicBool>,
    ) -> Result<JoinHandle<()>> {
        let cloned_client = self.redis_client.clone();
        let id_clone = self.id.to_string();
        let storage_clone = storage.clone();
        let pending_close_clone = pending_close;

        let expiration_thread_handler = std::thread::spawn(move || {
            debug!("TTL expiration listener is created!");
            futures::executor::block_on(async move {
                loop {
                    if pending_close_clone.load(std::sync::atomic::Ordering::SeqCst) {
                        return;
                    }
                    match cloned_client.get_connection() {
                        Ok(mut con) => {
                            let mut pubsub = con.as_pubsub();
                            trace!("subscribe to cache key pattern: {}", &id_clone);
                            match pubsub.psubscribe(format!("__keyspace*__:{}*", &id_clone)) {
                                Ok(_) => {}
                                Err(e) => {
                                    error!("Failed to psubscribe: {}", e);
                                    continue;
                                }
                            }
                            pubsub
                                .set_read_timeout(Some(std::time::Duration::from_secs(1)))
                                .unwrap();
                            loop {
                                // break if the associated cache object is about to be closed
                                if pending_close_clone.load(std::sync::atomic::Ordering::SeqCst) {
                                    return;
                                }
                                match pubsub.get_message() {
                                    Ok(msg) => {
                                        let channel: String = msg.get_channel().unwrap();
                                        let payload: String = msg.get_payload().unwrap();
                                        let redis_key = &channel[channel.find(':').unwrap() + 1..];
                                        let file = Self::from_redis_key(&id_clone, redis_key);
                                        trace!(
                                            "channel '{}': payload {}, file: {}",
                                            msg.get_channel_name(),
                                            payload,
                                            file,
                                        );
                                        if payload != "expired" {
                                            continue;
                                        }
                                        match storage_clone.remove(&file).await {
                                            Ok(_) => {
                                                increment_counter!(metric::CNT_RM_FILES);
                                                info!("TTL cache removed {}", &file);
                                            }
                                            Err(e) => {
                                                warn!("Failed to remove {}: {}", &file, e);
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        if e.kind() == redis::ErrorKind::IoError && e.is_timeout() {
                                            // ignore timeout error, as expected
                                        } else {
                                            error!(
                                                "Failed to get_message, retrying every 3s: {} {:?}",
                                                e,
                                                e.kind()
                                            );
                                            util::sleep_ms(3000);
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            error!("Failed to get redis connection: {}", e);
                            util::sleep_ms(3000);
                        }
                    }
                }
            });
        });
        Ok(expiration_thread_handler)
    }
}

impl Drop for TtlCache {
    /// The spawned key expiration handler thread needs to be dropped.
    fn drop(&mut self) {
        self.pending_close
            .store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(thread_handler) = self.expiration_thread_handler.take() {
            thread_handler.thread().unpark();
            thread_handler.join().unwrap();
            trace!("spawned thread dropped.");
        } else {
            warn!("expiration_thread_handler is None! If the thread is not spawned in the first place, the cache may have not been working properly. Otherwise, a thread is leaked.");
        }
    }
}

/// A wrapper for Sled
pub struct SledMetadataDb {
    db: sled::Db,
    metadata_tree: sled::Tree,
    atime_tree: sled::Tree,
    /// Column family name
    cf: String,
    // TTL
    /// interval of periodic cleanup of expired entries in seconds
    clean_interval: u64,
}

impl SledMetadataDb {
    pub fn new_lru(path: &str, cf_name: &str) -> Self {
        let db = Self::open_db(path).unwrap();
        let metadata_tree = db.open_tree(cf_name).unwrap();
        let atime_tree = db.open_tree(format!("{}_atime_tree", path)).unwrap();
        db.transaction::<_, _, ()>(|tx_db| {
            models::sled_try_init_current_size(tx_db, cf_name).unwrap();
            Ok(())
        })
        .unwrap();
        Self {
            db,
            metadata_tree,
            atime_tree,
            cf: cf_name.to_string(),
            clean_interval: 0,
        }
    }

    pub fn new_ttl(path: &str, cf_name: &str, clean_interval: u64) -> Self {
        let db = Self::open_db(path).unwrap();
        let metadata_tree = db.open_tree(cf_name).unwrap();
        let atime_tree = db.open_tree(format!("{}_atime_tree", path)).unwrap();
        Self {
            db,
            metadata_tree,
            atime_tree,
            cf: cf_name.to_string(),
            clean_interval,
        }
    }

    /// Open db, and retry if fails
    /// Reference: https://github.com/spacejam/sled/issues/1234
    fn open_db(path: impl AsRef<Path>) -> Result<sled::Db> {
        let mut sled_error = Error::OtherError("Unknown error: sled not initialized".into());
        for retry_attempt in 0..10 {
            match sled::open(&path) {
                Ok(db) => return Ok(db),
                Err(e) => {
                    warn!(
                        "{}/10 Failed to open sled db at {}: {}",
                        retry_attempt + 1,
                        path.as_ref().display(),
                        e
                    );
                    sled_error = Error::SledError(e);
                }
            }
            util::sleep_ms(1000);
        }
        Err(sled_error)
    }
}

/// An LRU Cache implementation with Sled.
///
/// Two mappings are maintained:
/// 1. filename -> (size, atime)
/// 2. atime -> filename
/// The `filename` is the external cache key. Its `atime` is stored to remove old
/// atime mapping.
impl LruMetadataStore for SledMetadataDb {
    fn get_lru_entry(&self, key: &str) -> CacheHitMiss {
        let tx_result: TransactionResult<_, TransactionError> =
            (&self.metadata_tree, &self.atime_tree).transaction(|(metadata_tree, atime_tree)| {
                match metadata_tree.get(key) {
                    Ok(Some(_)) => {
                        // update cache entry in db
                        let new_atime = util::now_nanos();
                        models::sled_update_cache_entry_atime(
                            metadata_tree,
                            atime_tree,
                            key,
                            new_atime,
                        );
                        Ok(CacheHitMiss::Hit)
                    }
                    _ => Ok(CacheHitMiss::Miss),
                }
            });
        match tx_result {
            Ok(hit_miss) => hit_miss,
            Err(e) => {
                error!("Failed to get_lru_entry: {}", e);
                CacheHitMiss::Miss
            }
        }
    }

    fn set_lru_entry(&self, key: &str, value: &CacheData) {
        let atime = util::now_nanos();
        let db_tree: &sled::Tree = &self.db;
        let tx_result: TransactionResult<_, TransactionError> =
            (db_tree, &self.metadata_tree, &self.atime_tree).transaction(
                |(db, metadata_tree, atime_tree)| {
                    models::sled_insert_cache_entry(
                        db,
                        &self.cf,
                        metadata_tree,
                        atime_tree,
                        key,
                        value.len() as CacheSizeType,
                        atime,
                    );
                    let current_size = models::sled_lru_get_current_size(db, &self.cf)
                        .unwrap()
                        .unwrap()
                        + value.len() as CacheSizeType;
                    models::sled_lru_set_current_size(db, &self.cf, current_size);
                    histogram!(
                        metric::get_cache_size_metrics_key(&self.cf),
                        current_size as f64
                    );
                    Ok(())
                },
            );
        match tx_result {
            Ok(_) => (),
            Err(e) => {
                error!("Failed to set_lru_entry: {}", e);
            }
        };
    }

    /// Run eviction policy if needed, reserve at least `size` for new cache entry.
    fn evict(
        &self,
        evict_size: CacheSizeType,
        _new_key: &str,
        size_limit: CacheSizeType,
    ) -> Vec<String> {
        let mut files_to_remove = Vec::new();
        let db = &self.db;
        let prefix = &self.cf;
        let default_tree: &sled::Tree = db;
        let atime_tree = &self.atime_tree;
        let metadata_tree = &self.metadata_tree;
        while models::sled_lru_get_current_size_notx(db, prefix)
            .unwrap()
            .unwrap()
            + evict_size
            > size_limit
        {
            // read a possible eviction candidate, multiple threads may read the same one
            if let Ok(Some(atime_tree_val)) = atime_tree.first() {
                // An eviction is atomic
                let tx_result: sled::transaction::TransactionResult<_, ()> =
                    (default_tree, atime_tree, metadata_tree).transaction::<_, _>(
                        |(db, atime_tree, metadata_tree)| {
                            match atime_tree.get(&atime_tree_val.0) {
                                Ok(Some(_)) => {
                                    // transactions in sled are serializable, continue
                                    let filename: &str =
                                        std::str::from_utf8(atime_tree_val.1.as_ref()).unwrap();
                                    let entry: SledMetadata =
                                        metadata_tree.get(filename).unwrap().unwrap().into();
                                    let file_size = entry.size;
                                    let cache_size = models::sled_lru_get_current_size(db, prefix)
                                        .unwrap()
                                        .unwrap()
                                        - file_size;
                                    models::sled_lru_set_current_size(db, prefix, cache_size);
                                    histogram!(
                                        metric::get_cache_size_metrics_key(&self.cf),
                                        cache_size as f64
                                    );
                                    metadata_tree.remove(filename).unwrap();
                                    atime_tree.remove(&atime_tree_val.0).unwrap();
                                    Ok(Some(filename.to_string()))
                                }
                                _ => {
                                    // some other thread would remove the entry
                                    Ok(None)
                                }
                            }
                        },
                    );
                if let Some(filename) = tx_result.unwrap() {
                    files_to_remove.push(filename);
                }
            }
        }
        files_to_remove
    }

    fn get_total_size(&self) -> CacheSizeType {
        self.db
            .transaction::<_, _, ()>(|tx_db| {
                Ok(models::sled_lru_get_current_size(tx_db, &self.cf)
                    .unwrap()
                    .unwrap())
            })
            .unwrap()
    }
}

impl TtlMetadataStore for SledMetadataDb {
    fn get_ttl_entry(&self, key: &str) -> CacheHitMiss {
        match self.metadata_tree.get(key) {
            Ok(Some(val)) => {
                let exp_time: i64 = i64::from_be_bytes(val.as_ref().try_into().unwrap());
                if exp_time > util::now_nanos() {
                    CacheHitMiss::Hit
                } else {
                    CacheHitMiss::Miss
                }
            }
            Ok(None) => CacheHitMiss::Miss,
            Err(e) => {
                error!("failed to get ttl entry {}: {:?}", key, e);
                CacheHitMiss::Miss
            }
        }
    }

    fn set_ttl_entry(&self, key: &str, _value: &CacheData, ttl: u64) {
        let _tx_result: TransactionResult<_, ()> = (&self.atime_tree, &self.metadata_tree)
            .transaction(|(atime_tree, metadata_tree)| {
                let expire_time = (util::now_nanos() + ttl as i64 * 1_000_000_000).to_be_bytes();
                atime_tree.insert(&expire_time, key).unwrap();
                metadata_tree.insert(key, &expire_time).unwrap();
                Ok(())
            });
        trace!("CACHE SET {} TTL={}", &key, ttl);
    }

    fn spawn_expiration_cleanup_thread(
        &self,
        storage: &Storage,
        pending_close: Arc<AtomicBool>,
    ) -> Result<JoinHandle<()>> {
        let storage_clone = storage.clone();
        let pending_close_clone = pending_close;
        let atime_tree = self.atime_tree.clone();
        let metadata_tree = self.metadata_tree.clone();
        let clean_interval = self.clean_interval;
        let expiration_thread_handler = std::thread::spawn(move || {
            futures::executor::block_on(async move {
                debug!("TTL expiration listener is created! (sled)");
                loop {
                    if pending_close_clone.load(std::sync::atomic::Ordering::SeqCst) {
                        return;
                    }
                    let time = util::now_nanos();
                    let files_to_remove: Vec<String> = atime_tree
                        .range(..time.to_be_bytes())
                        .map(|e| {
                            let e = e.unwrap();
                            let key = std::str::from_utf8(e.1.as_ref()).unwrap();
                            let _tx_result: TransactionResult<_, ()> =
                                (&atime_tree, &metadata_tree).transaction(
                                    |(atime_tree, metadata_tree)| {
                                        atime_tree.remove(&e.0).unwrap();
                                        metadata_tree.remove(&e.1).unwrap();
                                        Ok(())
                                    },
                                );
                            key.to_string()
                        })
                        .collect();
                    for key in files_to_remove {
                        match storage_clone.remove(&key).await {
                            Ok(_) => {
                                increment_counter!(metric::CNT_RM_FILES);
                                info!("TTL cache removed {}", &key);
                            }
                            Err(e) => {
                                warn!("Failed to remove {}: {}.", &key, e);
                            }
                        }
                    }
                    // park the thread, and unpark it when `drop` is called so that
                    // configuration update will not be blocked.
                    std::thread::park_timeout(std::time::Duration::from_secs(clean_interval));
                }
            });
        });
        Ok(expiration_thread_handler)
    }
}

#[derive(Hash, Eq, PartialEq, Debug)]
pub struct CacheEntry<Metadata, Key, Value> {
    pub metadata: Metadata,
    pub key: Key,
    pub value: Value,
}

#[derive(Debug)]
pub struct LruCacheMetadata {
    pub size: CacheSizeType,
    pub atime: i64, // last access timestamp
}

impl CacheEntry<LruCacheMetadata, String, ()> {
    pub fn new(path: &str, size: u64) -> CacheEntry<LruCacheMetadata, String, ()> {
        CacheEntry {
            metadata: LruCacheMetadata {
                size,
                atime: util::now(),
            },
            key: String::from(path),
            value: (),
        }
    }

    /**
     * Convert a cache entry to an array keys and values to be stored as redis hash
     */
    pub fn to_redis_multiple_fields(&self) -> Vec<(&str, String)> {
        vec![
            ("path", self.key.clone()),
            ("size", self.metadata.size.to_string()),
            ("atime", self.metadata.atime.to_string()),
        ]
    }
}

pub struct NoCache {}

#[async_trait]
impl Cache for NoCache {
    async fn put(&mut self, _key: &str, _entry: CacheData) {}
    async fn get(&self, _key: &str) -> Option<CacheData> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream::{self};
    use futures::StreamExt;
    use lazy_static::lazy_static;
    use std::fs;
    use std::io;
    use std::io::prelude::*;
    use std::thread;
    use std::time;
    use tokio::sync::RwLock;

    impl CacheData {
        pub async fn to_vec(self) -> Vec<u8> {
            match self {
                CacheData::TextData(text) => text.into_bytes(),
                CacheData::BytesData(bytes) => bytes.to_vec(),
                CacheData::ByteStream(mut stream, ..) => {
                    let mut v = Vec::new();
                    while let Some(bytes_result) = stream.next().await {
                        if bytes_result.is_err() {
                            return Vec::new();
                        }
                        v.append(&mut bytes_result.unwrap().to_vec());
                    }
                    v
                }
            }
        }
    }

    impl LruCache {
        fn get_total_size(&self) -> CacheSizeType {
            self.metadata_db.get_total_size()
        }
    }

    static TEST_CACHE_DIR: &str = "cache";

    fn setup() {
        lazy_static! {
            /// Initialize logger only once.
            static ref LOGGER: () = {
                let mut log_builder = pretty_env_logger::formatted_builder();
                log_builder
                    .filter_module("sled", log::LevelFilter::Info)
                    .filter_level(log::LevelFilter::Trace)
                    .target(pretty_env_logger::env_logger::Target::Stdout)
                    .init();
            };
        };
        let _ = &LOGGER;
    }

    fn new_redis_client() -> redis::Client {
        redis::Client::open("redis://localhost:3001/")
            .expect("Failed to connect to redis server (test)")
    }

    fn get_file_all(path: &str) -> Vec<u8> {
        let mut buf = Vec::new();
        fs::File::open(path).unwrap().read_to_end(&mut buf).unwrap();
        buf
    }

    fn file_not_exist(path: &str) -> bool {
        match fs::File::open(path) {
            Ok(_) => false,
            Err(e) => {
                if e.kind() == io::ErrorKind::NotFound {
                    return true;
                }
                false
            }
        }
    }

    macro_rules! new_lru_redis_cache {
        ($dir: expr, $size: expr, $redis_client: expr, $id: expr) => {
            LruCache::new(
                $size,
                Arc::new(RedisMetadataDb::new($redis_client, $id)),
                Arc::new(Storage::FileSystem {
                    root_dir: $dir.to_string(),
                }),
                $id,
            )
        };
    }

    macro_rules! new_lru_sled_cache {
        ($dir: expr, $size: expr, $id: expr) => {
            LruCache::new(
                $size,
                Arc::new(SledMetadataDb::new_lru(&format!("{}/sled", $dir), $id)),
                Arc::new(Storage::FileSystem {
                    root_dir: $dir.to_string(),
                }),
                $id,
            )
        };
    }

    macro_rules! new_ttl_redis_cache {
        ($dir: expr, $ttl: expr, $redis_client:expr, $id: expr) => {
            TtlCache::new(
                $ttl,
                Arc::new(RedisMetadataDb::new($redis_client, $id)),
                Arc::new(Storage::FileSystem {
                    root_dir: $dir.to_string(),
                }),
            )
        };
    }

    macro_rules! new_ttl_sled_cache {
        ($dir: expr, $ttl: expr, $id: expr, $interval:expr) => {
            TtlCache::new(
                $ttl,
                Arc::new(SledMetadataDb::new_ttl($dir, $id, $interval)),
                Arc::new(Storage::FileSystem {
                    root_dir: $dir.to_string(),
                }),
            )
        };
    }

    macro_rules! cache_put {
        ($cache: ident, $k: expr, $v: expr) => {
            $cache.put($k, $v).await;
        };
    }

    macro_rules! cache_get {
        ($cache: ident, $k: expr) => {
            $cache.get($k).await
        };
    }

    #[tokio::test]
    async fn lru_redis_cache_entry_set_success() {
        let redis_client = new_redis_client();
        let mut lru_cache = new_lru_redis_cache!(
            TEST_CACHE_DIR,
            16 * 1024 * 1024,
            redis_client,
            "test_cache_entry_success"
        );
        let key = "answer";
        let cached_data = vec![42];
        let len = cached_data.len();
        cache_put!(lru_cache, "answer", cached_data.clone().into());
        let total_size_expected = len as CacheSizeType;
        let total_size_actual: CacheSizeType = lru_cache.get_total_size();
        let cached_data_actual = get_file_all(&format!("{}/{}", TEST_CACHE_DIR, key));
        // metadata: size is 1, file content is the same
        assert_eq!(total_size_actual, total_size_expected);
        assert_eq!(&cached_data_actual, &cached_data);
    }

    #[tokio::test]
    async fn lru_sled_cache_entry_set_success() {
        setup();
        let mut lru_cache =
            new_lru_sled_cache!(TEST_CACHE_DIR, 16 * 1024 * 1024, "test_cache_entry_success");
        let key = "answer";
        let cached_data = vec![42];
        let len = cached_data.len();
        cache_put!(lru_cache, "answer", cached_data.clone().into());
        let total_size_expected = len as CacheSizeType;
        let total_size_actual: CacheSizeType = lru_cache.get_total_size();
        let cached_data_actual = get_file_all(&format!("{}/{}", TEST_CACHE_DIR, key));
        // metadata: size is 1, file content is the same
        assert_eq!(total_size_actual, total_size_expected);
        assert_eq!(&cached_data_actual, &cached_data);
    }

    async fn lru_cache_size_constaint_tester(mut lru_cache: LruCache, cached_path: &str) {
        cache_put!(lru_cache, "tsu_ki", vec![0; 5].into());
        let total_size_actual: CacheSizeType = lru_cache.get_total_size();
        assert_eq!(total_size_actual, 5);
        thread::sleep(time::Duration::from_secs(1));
        cache_put!(lru_cache, "kirei", vec![0; 11].into());
        let total_size_actual: CacheSizeType = lru_cache.get_total_size();
        assert_eq!(total_size_actual, 16);
        assert_eq!(
            get_file_all(&format!("{}/{}", cached_path, "tsu_ki")),
            vec![0; 5]
        );
        assert_eq!(
            get_file_all(&format!("{}/{}", cached_path, "kirei")),
            vec![0; 11]
        );
        // cache is full, evict tsu_ki
        cache_put!(lru_cache, "suki", vec![1; 4].into());
        assert_eq!(lru_cache.get_total_size(), 15);
        assert!(file_not_exist(&format!("{}/{}", cached_path, "tsu_ki")));
        // evict both suki and kirei
        cache_put!(lru_cache, "deadbeef", vec![2; 16].into());
        assert_eq!(lru_cache.get_total_size(), 16);
        assert!(file_not_exist(&format!("{}/{}", cached_path, "kirei")));
        assert!(file_not_exist(&format!("{}/{}", cached_path, "suki")));
        assert_eq!(
            get_file_all(&format!("{}/{}", cached_path, "deadbeef")),
            vec![2; 16]
        );
    }

    #[tokio::test]
    async fn lru_redis_cache_size_constraint() {
        setup();
        let redis_client = new_redis_client();
        let lru_cache = new_lru_redis_cache!(
            TEST_CACHE_DIR,
            16,
            redis_client,
            "lru_cache_size_constraint"
        );
        lru_cache_size_constaint_tester(lru_cache, TEST_CACHE_DIR).await;
    }

    #[tokio::test]
    async fn lru_sled_cache_size_constraint() {
        setup();
        let cached_dir = &format!("{}/size_constraint", TEST_CACHE_DIR);
        let sled_lru_cache = new_lru_sled_cache!(cached_dir, 16, "lru_cache_size_constraint");
        lru_cache_size_constaint_tester(sled_lru_cache, cached_dir).await;
    }

    async fn test_lru_cache_no_evict_recent_tester(mut lru_cache: LruCache) {
        let key1 = "1二号去听经";
        let key2 = "2晚上住旅店";
        let key3 = "3三号去餐厅";
        let key4 = "4然后看电影";
        cache_put!(lru_cache, key1, vec![1].into());
        thread::sleep(time::Duration::from_secs(1));
        cache_put!(lru_cache, key2, vec![2].into());
        thread::sleep(time::Duration::from_secs(1));
        cache_put!(lru_cache, key3, vec![3].into());
        assert_eq!(lru_cache.get_total_size(), 3);
        // set key4, evict key1
        thread::sleep(time::Duration::from_secs(1));
        cache_put!(lru_cache, key4, vec![4].into());
        assert!(cache_get!(lru_cache, key1).is_none());
        // assert
        assert_eq!(lru_cache.get_total_size(), 3);
        // get key2, update atime
        thread::sleep(time::Duration::from_secs(1));
        assert_eq!(cache_get!(lru_cache, key2).unwrap().to_vec().await, vec![2]);
        assert_eq!(lru_cache.get_total_size(), 3);
        // set key1, evict key3
        thread::sleep(time::Duration::from_secs(1));
        cache_put!(lru_cache, key1, vec![11].into());
        assert_eq!(lru_cache.get_total_size(), 3);
        assert!(cache_get!(lru_cache, key3).is_none());
        assert_eq!(lru_cache.get_total_size(), 3);
    }

    #[tokio::test]
    async fn test_lru_cache_no_evict_recent() {
        let redis_client = new_redis_client();
        let lru_cache =
            new_lru_redis_cache!(TEST_CACHE_DIR, 3, redis_client, "lru_no_evict_recent");
        let lru_sled_cache = new_lru_sled_cache!(
            &format!("{}/no_evict_recent", TEST_CACHE_DIR),
            3,
            "lru_no_evict_recent"
        );
        test_lru_cache_no_evict_recent_tester(lru_cache).await;
        test_lru_cache_no_evict_recent_tester(lru_sled_cache).await;
    }

    #[tokio::test]
    async fn key_update_total_size() {
        let redis_client = new_redis_client();
        let mut lru_cache = new_lru_redis_cache!(
            TEST_CACHE_DIR,
            3,
            redis_client,
            "key_update_no_change_total_size"
        );
        let key = "Phantom";
        cache_put!(lru_cache, key, vec![0].into());
        assert_eq!(lru_cache.get_total_size(), 1);
        cache_put!(lru_cache, key, vec![0, 1].into());
        assert_eq!(lru_cache.get_total_size(), 2);
        let mut lru_cache = new_lru_sled_cache!(
            &format!("{}/key_update_no_change_total_size", TEST_CACHE_DIR),
            3,
            "key_update_no_change_total_size"
        );
        let key = "Phantom";
        cache_put!(lru_cache, key, vec![0].into());
        assert_eq!(lru_cache.get_total_size(), 1);
        cache_put!(lru_cache, key, vec![0, 1].into());
        assert_eq!(lru_cache.get_total_size(), 2);
    }

    async fn lru_cache_isolation_tester(mut lru_cache_1: LruCache, mut lru_cache_2: LruCache) {
        cache_put!(lru_cache_1, "1", vec![1].into());
        cache_put!(lru_cache_2, "2", vec![2].into());
        assert_eq!(lru_cache_1.get_total_size(), 1);
        assert_eq!(lru_cache_2.get_total_size(), 1);
        assert_eq!(
            cache_get!(lru_cache_1, "1").unwrap().to_vec().await,
            vec![1_u8]
        );
        assert!(cache_get!(lru_cache_1, "2").is_none());
        assert!(cache_get!(lru_cache_2, "1").is_none());
        assert_eq!(
            cache_get!(lru_cache_2, "2").unwrap().to_vec().await,
            vec![2]
        );
    }

    #[tokio::test]
    async fn lru_redis_cache_isolation() {
        let lru_cache_1 = new_lru_redis_cache!(
            &format!("{}/{}", TEST_CACHE_DIR, "1"),
            3,
            new_redis_client(),
            "cache_isolation_1"
        );
        let lru_cache_2 = new_lru_redis_cache!(
            &format!("{}/{}", TEST_CACHE_DIR, "2"),
            3,
            new_redis_client(),
            "cache_isolation_2"
        );
        lru_cache_isolation_tester(lru_cache_1, lru_cache_2).await;
    }

    #[tokio::test]
    async fn lru_sled_cache_isolation() {
        let cache1 = new_lru_sled_cache!(
            &format!("{}/sled/{}", TEST_CACHE_DIR, "1"),
            3,
            "cache_isolation_1"
        );
        let cache2 = new_lru_sled_cache!(
            &format!("{}/sled/{}", TEST_CACHE_DIR, "2"),
            3,
            "cache_isolation_2"
        );
        lru_cache_isolation_tester(cache1, cache2).await;
    }

    #[tokio::test]
    async fn lru_sled_cache_concurrency() {
        let cache = new_lru_sled_cache!(
            &format!("{}/sled/{}", TEST_CACHE_DIR, "concurrency"),
            2,
            "cache_concurrency"
        );
        let arc_cache = Arc::new(RwLock::new(cache));
        let mut threads = Vec::new();
        for _ in 0..256 {
            let cache = arc_cache.clone();
            threads.push(tokio::spawn(async move {
                cache.write().await.put("k1", vec![1].into()).await;
                cache.write().await.put("k2", vec![2].into()).await;
                cache.write().await.put("k3", vec![3].into()).await;
                cache.write().await.put("k4", vec![4].into()).await;
            }));
        }
        for t in threads {
            t.await.unwrap();
        }
        assert_eq!(arc_cache.read().await.get_total_size(), 2);
    }

    #[tokio::test]
    async fn cache_stream_size_valid() {
        let mut lru_cache =
            new_lru_redis_cache!(TEST_CACHE_DIR, 3, new_redis_client(), "stream_cache");
        let bytes: Bytes = Bytes::from(vec![1, 1, 4]);
        let stream = stream::iter(vec![Ok(bytes.clone())]);
        let stream: Box<dyn Stream<Item = Result<Bytes>> + Send + Unpin> = Box::new(stream);
        cache_put!(lru_cache, "na tsu", CacheData::ByteStream(stream, Some(3)));
        let size = lru_cache.get_total_size();
        assert_eq!(size, 3);
    }

    #[tokio::test]
    async fn test_ttl_redis_cache_expire_key() {
        setup();
        let mut cache = new_ttl_redis_cache!(TEST_CACHE_DIR, 1, new_redis_client(), "ttl_simple");
        cache_put!(cache, "key", vec![1].into());
        assert_eq!(cache_get!(cache, "key").unwrap().to_vec().await, vec![1]);
        util::sleep_ms(4000);
        assert!(cache_get!(cache, "key").is_none());
    }

    #[tokio::test]
    async fn ttl_sled_cache_expire_key() {
        setup();
        let mut cache = new_ttl_sled_cache!(
            &format!("{}/sled_no_dup", TEST_CACHE_DIR),
            1,
            "ttl_sled_no_dup",
            0
        );
        cache_put!(cache, "key", vec![1].into());
        assert_eq!(cache_get!(cache, "key").unwrap().to_vec().await, vec![1]);
        util::sleep_ms(1000);
        assert!(cache_get!(cache, "key").is_none());
    }
}
