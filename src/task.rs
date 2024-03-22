use crate::cache::{
    Cache, CacheData, CacheHitMiss, LruCache, RedisMetadataDb, SledMetadataDb, TtlCache,
};
use crate::error::Error;
use crate::error::Result;
use crate::metric;
use crate::settings::Settings;
use crate::settings::{MetadataDb, Policy, PolicyType, Rewrite};
use crate::storage::Storage;
use crate::util;

use bytes::Bytes;
use futures::Stream;
use futures::StreamExt;
use metrics::{histogram, increment_counter};
use std::collections::HashMap;
use std::collections::HashSet;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::RwLock;
use warp::http::Response;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Task {
    pub rule_id: RuleId,
    pub url: String,
}

pub enum TaskResponse {
    StringResponse(String),
    BytesResponse(Bytes),
    StreamResponse(Pin<Box<dyn Stream<Item = Result<Bytes>> + Send>>),
    Redirect(warp::reply::WithHeader<warp::http::StatusCode>),
}

impl From<String> for TaskResponse {
    fn from(s: String) -> TaskResponse {
        TaskResponse::StringResponse(s)
    }
}

impl From<CacheData> for TaskResponse {
    fn from(cache_data: CacheData) -> TaskResponse {
        match cache_data {
            CacheData::TextData(text) => text.into(),
            CacheData::BytesData(bytes) => TaskResponse::BytesResponse(bytes),
            CacheData::ByteStream(stream, ..) => TaskResponse::StreamResponse(Box::pin(stream)),
        }
    }
}

impl warp::Reply for TaskResponse {
    fn into_response(self) -> warp::reply::Response {
        match self {
            TaskResponse::StringResponse(content) => Response::builder()
                .header("Content-Type", "text/html")
                .body(content.into())
                .unwrap(),
            TaskResponse::BytesResponse(bytes) => warp::reply::Response::new(bytes.into()),
            TaskResponse::StreamResponse(stream) => {
                warp::reply::Response::new(warp::hyper::Body::wrap_stream(stream))
            }
            TaskResponse::Redirect(r) => r.into_response(),
        }
    }
}

impl Task {
    /// create a unique key for the current task
    pub fn to_key(&self) -> String {
        self.url
            .replace("http://", "http/")
            .replace("https://", "https/")
            .trim_end_matches('/')
            .to_string()
    }
}

pub type RuleId = usize;

#[derive(Clone)]
pub struct TaskManager {
    pub config: Settings,
    /// RuleId -> (cache, size_limit of payload)
    pub rule_map: HashMap<RuleId, (Arc<RwLock<dyn Cache>>, usize)>,
    /// Specifies how to do the upstream rewrite for RuleId.
    /// RuleId -> Vec<Rewrite>
    pub rewrite_map: HashMap<RuleId, Vec<Rewrite>>,
    task_set: Arc<RwLock<HashSet<Task>>>,
}

impl TaskManager {
    pub fn new(config: Settings) -> Self {
        TaskManager {
            config,
            rule_map: HashMap::new(),
            task_set: Arc::new(RwLock::new(HashSet::new())),
            rewrite_map: HashMap::new(),
        }
    }

    pub fn empty() -> Self {
        Self {
            config: Settings::default(),
            rule_map: HashMap::new(),
            task_set: Arc::new(RwLock::new(HashSet::new())),
            rewrite_map: HashMap::new(),
        }
    }

    pub async fn resolve_task(&self, task: &Task) -> (Result<TaskResponse>, CacheHitMiss) {
        // try get from cache
        let mut cache_result = None;
        let key = task.to_key();

        if let Some(bytes) = self.get(task, &key).await {
            cache_result = Some(bytes);
        }
        if let Some(data) = cache_result {
            info!("[Request] [HIT] {:?}", &task);
            return (Ok(data.into()), CacheHitMiss::Hit);
        }
        increment_counter!(metric::COUNTER_CACHE_MISS);
        // cache miss
        // fetch from upstream
        let remote_url = self.resolve_task_upstream(task);
        info!(
            "[Request] [MISS] {:?}, fetching from upstream: {}",
            &task, &remote_url
        );
        let resp = util::make_request(&remote_url, false).await;
        match resp {
            Ok(res) => {
                if !res.status().is_success() {
                    return (Err(Error::UpstreamRequestError(res)), CacheHitMiss::Miss);
                }
                // if the response is too large, respond users with a redirect to upstream
                if let Some(content_length) = res.content_length() {
                    let size_limit = self.get_task_size_limit(task);
                    if size_limit != 0 && size_limit < content_length as usize {
                        return (
                            Ok(TaskResponse::Redirect(warp::reply::with_header(
                                warp::http::StatusCode::FOUND,
                                "Location",
                                remote_url,
                            ))),
                            CacheHitMiss::Miss,
                        );
                    }
                }
                // dispatch async cache task
                let _ = self.spawn_task(task.clone()).await;
                let rule_id = task.rule_id;
                if let Some(rewrite_rules) = self.rewrite_map.get(&rule_id) {
                    let text = res.text().await.unwrap();
                    let content = Self::rewrite_upstream(text, rewrite_rules);
                    (Ok(content.into()), CacheHitMiss::Miss)
                } else {
                    (
                        Ok(TaskResponse::StreamResponse(Box::pin(
                            res.bytes_stream()
                                .map(move |x| x.map_err(Error::RequestError)),
                        ))),
                        CacheHitMiss::Miss,
                    )
                }
            }
            Err(e) => {
                error!("[Request] {:?} failed to fetch upstream: {}", &task, e);
                (Err(e), CacheHitMiss::Miss)
            }
        }
    }

    /// for each rule, create associated cache if the policy has not been created
    pub fn refresh_config(&mut self, settings: &Settings) {
        let app_settings = settings;
        let redis_url = app_settings.get_redis_url();
        let policies = app_settings.policies.clone();

        let tm = self;
        tm.config = app_settings.clone();

        let mut policy_map: HashSet<String> = HashSet::new(); // used to avoid create duplicated cache if some rules share the same policy
                                                              // get active policy set
        for rule in &app_settings.rules {
            policy_map.insert(rule.policy.clone());
        }

        // Create storages
        let mut storage_map = HashMap::new();
        for storage_config in &app_settings.storages {
            let storage = Self::create_storage(storage_config);
            storage_map.insert(storage_config.name.clone(), Arc::new(storage));
        }

        // Clear cache here, so that previous cache objects can be dropped
        tm.rule_map.clear();
        tm.rewrite_map.clear();
        let mut cache_map: HashMap<String, _> = HashMap::new();
        let redis_client = redis::Client::open(redis_url).expect("failed to connect to redis");
        // create cache for each policy
        for policy in &policy_map {
            let cache = Self::create_cache_from_rule(
                policy,
                &policies,
                Some(redis_client.clone()),
                &app_settings.sled.metadata_path,
                &storage_map,
            );
            cache_map.insert(policy.to_string(), cache.unwrap());
        }

        for (idx, rule) in app_settings.rules.iter().enumerate() {
            debug!("creating rule #{}: {:?}", idx, rule);
            let cache = cache_map.get(&rule.policy).unwrap().clone();
            tm.rule_map.insert(
                idx,
                (
                    cache,
                    rule.size_limit
                        .clone()
                        .map_or(0, |x| bytefmt::parse(x).unwrap() as usize),
                ),
            );
            if let Some(rewrite) = rule.rewrite.clone() {
                tm.rewrite_map.insert(idx, rewrite);
            }
        }
    }

    fn create_storage(storage: &crate::settings::Storage) -> crate::storage::Storage {
        match &storage.config {
            crate::settings::StorageConfig::Fs { path } => Storage::FileSystem {
                root_dir: path.clone(),
            },
            crate::settings::StorageConfig::Mem => Storage::new_mem(),
        }
    }

    fn create_cache_from_rule(
        policy_name: &str,
        policies: &[Policy],
        redis_client: Option<redis::Client>,
        sled_metadata_path: &str,
        storage_map: &HashMap<String, Arc<Storage>>,
    ) -> Result<Arc<RwLock<dyn Cache>>> {
        let policy_ident = policy_name;
        for p in policies {
            if p.name == policy_ident {
                let policy_type = p.typ;
                let metadata_db = p.metadata_db;
                match (policy_type, metadata_db) {
                    (PolicyType::Lru, MetadataDb::Redis) => {
                        return Ok(Arc::new(RwLock::new(LruCache::new(
                            p.size.as_ref().map_or(0, |x| bytefmt::parse(x).unwrap()),
                            Arc::new(RedisMetadataDb::new(redis_client.unwrap(), policy_ident)),
                            storage_map.get(&p.storage).unwrap().clone(),
                            policy_ident,
                        ))));
                    }
                    (PolicyType::Lru, MetadataDb::Sled) => {
                        return Ok(Arc::new(RwLock::new(LruCache::new(
                            p.size.as_ref().map_or(0, |x| bytefmt::parse(x).unwrap()),
                            Arc::new(SledMetadataDb::new_lru(
                                &format!("{}/{}", sled_metadata_path, policy_ident),
                                policy_ident,
                            )),
                            storage_map.get(&p.storage).unwrap().clone(),
                            policy_ident,
                        ))));
                    }
                    (PolicyType::Ttl, MetadataDb::Redis) => {
                        return Ok(Arc::new(RwLock::new(TtlCache::new(
                            p.timeout.unwrap_or(0),
                            Arc::new(RedisMetadataDb::new(redis_client.unwrap(), policy_ident)),
                            storage_map.get(&p.storage).unwrap().clone(),
                        ))));
                    }
                    (PolicyType::Ttl, MetadataDb::Sled) => {
                        return Ok(Arc::new(RwLock::new(TtlCache::new(
                            p.timeout.unwrap_or(0),
                            Arc::new(SledMetadataDb::new_ttl(
                                &format!("{}/{}", sled_metadata_path, &policy_ident),
                                policy_ident,
                                p.clean_interval.unwrap_or(3),
                            )),
                            storage_map.get(&p.storage).unwrap().clone(),
                        ))));
                    }
                };
            }
        }
        Err(Error::ConfigInvalid(format!(
            "No such policy: {}",
            policy_ident
        )))
    }

    async fn taskset_contains(&self, t: &Task) -> bool {
        self.task_set.read().await.contains(t)
    }

    async fn taskset_add(&self, t: Task) {
        self.task_set.write().await.insert(t);
    }

    async fn taskset_remove(task_set: Arc<RwLock<HashSet<Task>>>, t: &Task) {
        task_set.write().await.remove(t);
    }

    async fn taskset_len(task_set: Arc<RwLock<HashSet<Task>>>) -> usize {
        let len = task_set.read().await.len();
        histogram!(metric::HG_TASKS_LEN, len as f64);
        len
    }

    /// Spawn an async task
    async fn spawn_task(&self, task: Task) {
        increment_counter!(metric::COUNTER_TASKS_BG);
        if self.taskset_contains(&task).await {
            info!("[TASK] ignored existing task: {:?}", task);
            return;
        }
        self.taskset_add(task.clone()).await;
        let task_set_len = Self::taskset_len(self.task_set.clone()).await;
        info!("[TASK] [len={}] + {:?}", task_set_len, task);
        let c = self.get_cache_for_cache_rule(task.rule_id).unwrap();
        let rewrites = self.rewrite_map.get(&task.rule_id).cloned();
        let task_clone = task.clone();
        let upstream_url = self.resolve_task_upstream(&task_clone);
        let task_list_ptr = self.task_set.clone();
        // spawn an async download task
        tokio::spawn(async move {
            let resp = util::make_request(&upstream_url, false).await;
            match resp {
                Ok(res) => {
                    if res.status().is_success() {
                        if let Some(rewrites) = rewrites {
                            let content = res.text().await.ok();
                            if content.is_none() {
                                increment_counter!(metric::CNT_TASKS_BG_FAILURE);
                                return;
                            }
                            let mut content = content.unwrap();
                            content = Self::rewrite_upstream(content, &rewrites);
                            c.write()
                                .await
                                .put(&task_clone.to_key(), content.into())
                                .await;
                        } else {
                            let len = res.content_length();
                            let bytestream = res.bytes_stream();
                            c.write()
                                .await
                                .put(
                                    &task_clone.to_key(),
                                    CacheData::ByteStream(
                                        Box::new(
                                            bytestream.map(move |x| x.map_err(Error::RequestError)),
                                        ),
                                        len,
                                    ),
                                )
                                .await;
                        }
                        increment_counter!(metric::CNT_TASKS_BG_SUCCESS);
                    } else {
                        warn!(
                            "[TASK] ❌ failed to fetch upstream: {}, Task {:?}",
                            res.status().canonical_reason().unwrap_or("unknown"),
                            &task_clone
                        );
                        increment_counter!(metric::CNT_TASKS_BG_FAILURE);
                    }
                }
                Err(e) => {
                    increment_counter!(metric::CNT_TASKS_BG_FAILURE);
                    error!(
                        "[TASK] ❌ failed to fetch upstream: {}, Task {:?}",
                        e, &task_clone
                    );
                }
            };
            Self::taskset_remove(task_list_ptr.clone(), &task_clone).await;
            Self::taskset_len(task_list_ptr).await;
        });
    }

    /// get task result from cache
    pub async fn get(&self, task: &Task, key: &str) -> Option<CacheData> {
        let rule_id = task.rule_id;
        match self.get_cache_for_cache_rule(rule_id) {
            Some(cache) => cache.read().await.get(key).await,
            None => {
                error!("Failed to get cache for rule #{} from cache map", rule_id);
                None
            }
        }
    }

    pub fn rewrite_upstream(content: String, rewrites: &[Rewrite]) -> String {
        let mut content = content;
        for rewrite in rewrites {
            content = content.replace(&rewrite.from, &rewrite.to);
        }
        content
    }

    pub fn resolve_task_upstream(&self, task_type: &Task) -> String {
        task_type.url.clone()
    }

    pub fn get_cache_for_cache_rule(&self, rule_id: RuleId) -> Option<Arc<RwLock<dyn Cache>>> {
        self.rule_map.get(&rule_id).map(|tuple| tuple.0.clone())
    }

    pub fn get_task_size_limit(&self, task: &Task) -> usize {
        self.rule_map.get(&task.rule_id).unwrap().1
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn rewrite_upstream() {
        let rewrites = vec![
            Rewrite {
                from: "flower".to_string(),
                to: "vegetable".to_string(),
            },
            Rewrite {
                from: "cat".to_string(),
                to: "dog".to_string(),
            },
        ];
        assert_eq!(
            TaskManager::rewrite_upstream("flower cat".to_string(), &rewrites),
            "vegetable dog"
        );
    }
}
