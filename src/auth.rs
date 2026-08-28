use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// 恒定时间字符串比较：先各自 SHA-256 定长摘要，再按恒定时间比较，
/// 避免长度/前缀侧信道。二者相等返回 true。
#[allow(dead_code)]
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    let ha = Sha256::digest(a.as_bytes());
    let hb = Sha256::digest(b.as_bytes());
    ha.ct_eq(&hb).into()
}

#[derive(Debug, Clone)]
pub struct Authenticator {
    /// 摘要缓存：只存 key 的 SHA-256 哈希，不在内存中保留明文 key
    hashes: Vec<[u8; 32]>,
}

impl Authenticator {
    pub fn new(keys: &[String]) -> Self {
        let hashes = keys
            .iter()
            .map(|k| Sha256::digest(k.as_bytes()).into())
            .collect();
        Authenticator { hashes }
    }

    /// 校验候选密钥是否命中任意一个已配置密钥；恒定时间内完成。
    pub fn verify(&self, candidate: Option<&str>) -> bool {
        let Some(candidate) = candidate else {
            return false;
        };
        let candidate_hash: [u8; 32] = Sha256::digest(candidate.as_bytes()).into();
        // 恒定时间累加：对所有条目连续比较，不提前返回
        let mut acc = 0u8;
        for h in &self.hashes {
            acc |= bool::from(candidate_hash.ct_eq(h)) as u8;
        }
        acc == 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth() -> Authenticator {
        Authenticator::new(&["key-aaa".to_string(), "key-bbb".to_string()])
    }

    #[test]
    fn verify_accepts_any_configured_key() {
        let a = auth();
        assert!(a.verify(Some("key-aaa")));
        assert!(a.verify(Some("key-bbb")));
    }

    #[test]
    fn verify_rejects_unknown_and_missing() {
        let a = auth();
        assert!(!a.verify(Some("key-ccc")));
        assert!(!a.verify(Some("KEY-AAA"))); // 大小写敏感
        assert!(!a.verify(None));
    }

    #[test]
    fn reject_similar_prefix() {
        let a = auth();
        assert!(!a.verify(Some("key-a")));
        assert!(!a.verify(Some("key-aaaa")));
        assert!(!a.verify(Some("key-aaaa-key-aaa")));
    }

    #[test]
    fn constant_time_eq_works() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("a", "aa"));
        assert!(!constant_time_eq("", "x"));
    }

    #[test]
    fn verify_empty_key_config() {
        let a = Authenticator::new(&[]);
        assert!(!a.verify(Some("anything")));
    }
}
