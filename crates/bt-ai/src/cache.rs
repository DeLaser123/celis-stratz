//! Prompt-hash response cache: identical (model, messages, params) within a
//! project reuse the stored response — no silent API spend, fully auditable
//! via the ledger (`cache_hit: true`).

use bt_core::error::{CoreError, CoreResult};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedResponse {
    pub text: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub created_at: String,
}

pub struct ResponseCache {
    dir: PathBuf,
}

impl ResponseCache {
    pub fn new(dir: impl Into<PathBuf>) -> CoreResult<ResponseCache> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir).map_err(CoreError::Io)?;
        Ok(ResponseCache { dir })
    }

    pub fn key(model: &str, base_url: &str, payload: &str) -> String {
        bt_core::hash::hash_json(&serde_json::json!({
            "model": model,
            "base_url": base_url,
            "payload": payload,
        }))
    }

    /// Cache identity for real requests: request params that change the
    /// response are part of the key, so a response generated under a
    /// different `max_tokens`/`temperature` is never reused.
    pub fn key_params(
        model: &str,
        base_url: &str,
        payload: &str,
        max_tokens: Option<u32>,
        temperature: f64,
    ) -> String {
        bt_core::hash::hash_json(&serde_json::json!({
            "model": model,
            "base_url": base_url,
            "payload": payload,
            "max_tokens": max_tokens,
            "temperature": temperature,
        }))
    }

    fn path_for(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.json"))
    }

    pub fn get(&self, key: &str) -> Option<CachedResponse> {
        let path = self.path_for(key);
        let text = std::fs::read_to_string(&path).ok()?;
        serde_json::from_str(&text).ok()
    }

    pub fn put(&self, key: &str, resp: &CachedResponse) -> CoreResult<()> {
        let path = self.path_for(key);
        let text = serde_json::to_string_pretty(resp)
            .map_err(|e| CoreError::InvalidData(format!("cache serialize: {e}")))?;
        std::fs::write(&path, text).map_err(CoreError::Io)
    }

    pub fn len(&self) -> usize {
        std::fs::read_dir(&self.dir)
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
                    .count()
            })
            .unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_params_distinguishes_request_params() {
        let base = ResponseCache::key_params("m", "u", "p", None, 0.0);
        assert_ne!(
            base,
            ResponseCache::key_params("m", "u", "p", Some(8192), 0.0)
        );
        assert_ne!(base, ResponseCache::key_params("m", "u", "p", None, 0.7));
        assert_eq!(base, ResponseCache::key_params("m", "u", "p", None, 0.0));
    }

    #[test]
    fn put_get_roundtrip_and_key_sensitivity() {
        let dir = std::env::temp_dir().join(format!("bt_ai_cache_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let c = ResponseCache::new(&dir).unwrap();
        let k1 = ResponseCache::key("glm-5.3-flash", "https://x", "payload-1");
        assert!(c.get(&k1).is_none());
        c.put(
            &k1,
            &CachedResponse {
                text: "hello".into(),
                prompt_tokens: 1,
                completion_tokens: 2,
                created_at: "t".into(),
            },
        )
        .unwrap();
        let got = c.get(&k1).unwrap();
        assert_eq!(got.text, "hello");
        assert_eq!(c.len(), 1);
        // different payload -> different key
        let k2 = ResponseCache::key("glm-5.3-flash", "https://x", "payload-2");
        assert_ne!(k1, k2);
        assert!(c.get(&k2).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
