use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::agent::Runtime;

pub const HEADER_REQUEST_ID: &str = "x-mv-request-id";
pub const HEADER_DURATION: &str = "x-mv-duration-ms";
pub const HEADER_START: &str = "x-mv-start";
pub const HEADER_RESTORE: &str = "x-mv-restore-ms";
pub const HEADER_VM: &str = "x-mv-vm";
pub const HEADER_FUNCTION_ERROR: &str = "x-mv-function-error";
pub const HEADER_FUNCTION_DURATION: &str = "x-mv-function-ms";

pub const MIN_MEMORY_MIB: u32 = 64;
pub const MAX_MEMORY_MIB: u32 = 8192;
pub const MAX_VCPUS: u8 = 16;
pub const MIN_TIMEOUT_MS: u64 = 100;
pub const MAX_TIMEOUT_MS: u64 = 15 * 60 * 1000;
pub const MAX_CONCURRENCY: u32 = 1000;
pub const MAX_ENV_BYTES: usize = 4096;
pub const MAX_NAME_LEN: usize = 63;
pub const MAX_LOG_ENTRIES: usize = 2000;
const RESERVED_ENV_PREFIX: &str = "MV_";

fn default_memory_mib() -> u32 {
    128
}

fn default_vcpus() -> u8 {
    1
}

fn default_timeout_ms() -> u64 {
    3000
}

fn default_max_concurrency() -> u32 {
    10
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeployConfig {
    pub runtime: Runtime,
    #[serde(default)]
    pub handler: Option<String>,
    #[serde(default = "default_memory_mib")]
    pub memory_mib: u32,
    #[serde(default = "default_vcpus")]
    pub vcpus: u8,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default = "default_max_concurrency")]
    pub max_concurrency: u32,
}

pub fn validate_name(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let starts_well = chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    let rest_ok =
        chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if starts_well && rest_ok && name.len() <= MAX_NAME_LEN {
        Ok(())
    } else {
        Err(format!(
            "function names are 1-{MAX_NAME_LEN} characters of a-z, 0-9, '-' and '_', starting with a letter or digit"
        ))
    }
}

fn valid_env_key(key: &str) -> bool {
    let mut chars = key.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

impl DeployConfig {
    pub fn handler(&self) -> &str {
        self.handler
            .as_deref()
            .unwrap_or(self.runtime.default_handler())
    }

    pub fn validate(&self) -> Result<(), String> {
        if !(MIN_MEMORY_MIB..=MAX_MEMORY_MIB).contains(&self.memory_mib)
            || !self.memory_mib.is_multiple_of(4)
        {
            return Err(format!(
                "memory_mib must be a multiple of 4 between {MIN_MEMORY_MIB} and {MAX_MEMORY_MIB}"
            ));
        }
        if !(1..=MAX_VCPUS).contains(&self.vcpus) {
            return Err(format!("vcpus must be between 1 and {MAX_VCPUS}"));
        }
        if !(MIN_TIMEOUT_MS..=MAX_TIMEOUT_MS).contains(&self.timeout_ms) {
            return Err(format!(
                "timeout_ms must be between {MIN_TIMEOUT_MS} and {MAX_TIMEOUT_MS}"
            ));
        }
        if !(1..=MAX_CONCURRENCY).contains(&self.max_concurrency) {
            return Err(format!(
                "max_concurrency must be between 1 and {MAX_CONCURRENCY}"
            ));
        }
        let handler = self.handler();
        let handler_ok = match self.runtime {
            Runtime::Native => {
                !handler.is_empty()
                    && !handler.starts_with('/')
                    && handler
                        .split('/')
                        .all(|part| !part.is_empty() && part != "..")
            }
            Runtime::Python => handler.rsplit_once('.').is_some_and(|(module, function)| {
                !function.is_empty()
                    && module.split('.').all(|part| {
                        !part.is_empty()
                            && part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                    })
                    && function
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_')
            }),
        };
        if !handler_ok {
            return Err(match self.runtime {
                Runtime::Native => "native handlers are relative paths inside the archive".into(),
                Runtime::Python => "python handlers look like module.function".into(),
            });
        }
        let mut env_bytes = 0;
        for (key, value) in &self.env {
            if !valid_env_key(key) {
                return Err(format!("invalid environment variable name {key:?}"));
            }
            if key.starts_with(RESERVED_ENV_PREFIX) {
                return Err(format!(
                    "environment variables starting with {RESERVED_ENV_PREFIX} are reserved"
                ));
            }
            if value.contains('\0') {
                return Err(format!("environment variable {key} contains a NUL byte"));
            }
            env_bytes += key.len() + value.len();
        }
        if env_bytes > MAX_ENV_BYTES {
            return Err(format!(
                "environment variables may hold at most {MAX_ENV_BYTES} bytes"
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolInfo {
    pub idle: u32,
    pub busy: u32,
    pub starting: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FunctionInfo {
    pub name: String,
    pub revision: u64,
    pub config: DeployConfig,
    pub code_size: u64,
    pub code_sha256: String,
    pub created_at: String,
    pub updated_at: String,
    pub init_duration_ms: f64,
    pub pool: PoolInfo,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub logs: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    pub timestamp: String,
    pub request_id: Option<String>,
    pub stream: String,
    pub line: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CapacityInfo {
    pub memory_total_mib: u64,
    pub memory_available_mib: u64,
    pub memory_reserved_mib: u64,
    pub memory_limit_mib: u64,
    pub cpus: u32,
    pub vcpus_busy: u32,
    pub vcpu_limit: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StatusInfo {
    pub version: String,
    pub uptime_s: u64,
    pub functions: u32,
    pub vms: PoolInfo,
    pub capacity: CapacityInfo,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmInfo {
    pub id: String,
    pub function: String,
    pub revision: u64,
    pub state: String,
    pub pid: Option<u32>,
    pub uid: u32,
    pub cgroup: String,
    pub jail: String,
    pub invocations: u64,
    #[serde(default)]
    pub memory: Option<VmMemory>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmMemory {
    pub rss_kib: u64,
    pub pss_kib: u64,
    pub cgroup_bytes: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(json: &str) -> DeployConfig {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn deploy_config_defaults_and_handlers() {
        let python = config(r#"{"runtime":"python"}"#);
        assert_eq!(python.memory_mib, 128);
        assert_eq!(python.vcpus, 1);
        assert_eq!(python.timeout_ms, 3000);
        assert_eq!(python.max_concurrency, 10);
        assert_eq!(python.handler(), "main.handler");
        assert!(python.validate().is_ok());
        let native = config(r#"{"runtime":"native","handler":"bin/fn"}"#);
        assert_eq!(native.handler(), "bin/fn");
        assert!(native.validate().is_ok());
        assert!(
            serde_json::from_str::<DeployConfig>(r#"{"runtime":"python","memory":1}"#).is_err()
        );
    }

    #[test]
    fn deploy_config_validation() {
        let rejected = [
            r#"{"runtime":"python","memory_mib":32}"#,
            r#"{"runtime":"python","memory_mib":130}"#,
            r#"{"runtime":"python","vcpus":0}"#,
            r#"{"runtime":"python","timeout_ms":10}"#,
            r#"{"runtime":"python","max_concurrency":0}"#,
            r#"{"runtime":"python","handler":"main"}"#,
            r#"{"runtime":"python","handler":"a..b"}"#,
            r#"{"runtime":"python","handler":"os.system('x')"}"#,
            r#"{"runtime":"native","handler":"/bin/sh"}"#,
            r#"{"runtime":"native","handler":"../x"}"#,
            r#"{"runtime":"python","env":{"1A":"x"}}"#,
            r#"{"runtime":"python","env":{"MV_X":"x"}}"#,
            r#"{"runtime":"python","env":{"A":"\u0000"}}"#,
        ];
        for json in rejected {
            assert!(config(json).validate().is_err(), "{json}");
        }
        let mut big = config(r#"{"runtime":"python"}"#);
        big.env.insert("A".into(), "x".repeat(MAX_ENV_BYTES));
        assert!(big.validate().is_err());
        assert!(
            config(r#"{"runtime":"python","handler":"pkg.mod.fn","env":{"_K1":"v"}}"#)
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn function_names() {
        for name in ["a", "hello-world", "f_1", "9lives"] {
            assert!(validate_name(name).is_ok(), "{name}");
        }
        for name in ["", "-x", "Hello", "a/b", "a.b", &"x".repeat(64)] {
            assert!(validate_name(name).is_err(), "{name}");
        }
    }
}
