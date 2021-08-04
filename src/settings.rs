use crate::error::Error;
use crate::error::Result;
use config::{Config, Environment, File};

#[derive(Debug, Deserialize, Clone)]
pub struct Settings {
    pub port: u16,
    debug: bool,
    redis: Redis,
    pub url: Option<String>,
    pub rules: Vec<Rule>,
    pub policies: Vec<Policy>,
    pub builtin: BuiltinRules,
}

#[derive(Debug, Deserialize, Clone)]
struct Redis {
    url: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Rule {
    pub path: Option<String>,
    pub target: Option<String>,
    pub policy: String,
    pub upstream: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Policy {
    pub name: String,
    #[serde(rename = "type")]
    pub typ: PolicyType,
    pub timeout: Option<u64>,
    pub size: Option<u64>,
    pub path: Option<String>, // cache path
}

#[derive(Debug, Deserialize, Clone)]
pub struct BuiltinRules {
    pub pypi_index: Rule,
    pub pypi_packages: Rule,
    pub anaconda: Rule,
}

#[derive(Debug, Deserialize, Copy, Clone)]
pub enum PolicyType {
    #[serde(rename = "LRU")]
    Lru,
    #[serde(rename = "TTL")]
    Ttl,
}

impl Settings {
    pub fn new() -> Result<Self> {
        Self::new_from("config", "app")
    }

    pub fn new_from(filename: &str, env_prefix: &str) -> Result<Self> {
        let mut s = Config::default();
        s.merge(File::with_name(filename))?;
        s.merge(Environment::with_prefix(env_prefix))?;
        match s.try_into() {
            Ok(settings) => {
                // TODO: constraints checking
                let settings: Self = settings;
                for r in &settings.rules {
                    if r.target.is_none() && r.path.is_none() {
                        return Err(Error::ConfigInvalid(format!(
                            "one of target and path must be specified. (in rule {:?})",
                            r
                        )));
                    }
                }
                Ok(settings)
            }
            Err(e) => Err(Error::ConfigDeserializeError(e)),
        }
    }

    pub fn get_redis_url(&self) -> String {
        self.redis.url.clone()
    }
}