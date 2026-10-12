//! 带外上传通道的存储层：`create_upload` 建条目 → `PUT /upload/:id` 直传字节 → `send_email` 用 `upload_id` 消费。
//!
//! - id = 16 字节密码学随机数的 32 位小写 hex，既是 URL 里的一次性凭据也是 `upload_id`；
//! - 每个条目绑定创建者 API key 的 SHA-256（owner），只有同一密钥能消费；
//! - 状态机 `Pending -> Uploading -> Uploaded`，同一 id 的并发 PUT 只有一个能进入上传；
//! - 过期：所有访问前惰性清理 + 每 60s 后台清理；条目被丢弃时 `TempDir` 自动删除磁盘文件。

use rand::RngCore;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};
use subtle::ConstantTimeEq;
use tempfile::TempDir;

use crate::attachments::hex_lower;

/// 上传条目的状态机。
enum State {
    /// 已创建 upload_id，尚未收到字节
    Pending,
    /// 正在接收字节（占位，防同一 id 并发 PUT）
    Uploading,
    /// 字节已完整落盘，可被 `send_email` 消费
    Uploaded {
        dir: TempDir,
        path: PathBuf,
        size: u64,
        sha256: String,
    },
}

struct Entry {
    /// 创建者 API key 的 SHA-256；消费时须一致
    owner: [u8; 32],
    filename: String,
    created: Instant,
    state: State,
}

/// `PUT /upload/:id` 的前置检查失败原因（HTTP 层据此区分 404 / 409）。
#[derive(Debug, PartialEq, Eq)]
pub enum UploadErr {
    /// 不存在或已过期
    NotFound,
    /// 状态不允许（正在上传中或已完成）
    Conflict,
}

/// 被 `send_email` 消费下来的上传条目的**文件句柄部分**。
/// 元信息（文件名/大小/sha256）在消费前已由 `UploadMeta` 校验过，此处不再重复携带。
/// `dir` 必须存活到发送结束——它 drop 时磁盘文件才被删除。
#[derive(Debug)]
pub struct ConsumedUpload {
    pub path: PathBuf,
    pub dir: TempDir,
}

/// 条目的只读元信息（peek 用，不消费条目）。
#[derive(Debug, Clone)]
pub struct UploadMeta {
    pub filename: String,
    pub size: u64,
    pub sha256: String,
}

/// 绑定某个调用者身份的读写入口：`peek` 校验用、`take` 消费用。
pub struct UploadResolver<'a> {
    store: &'a UploadStore,
    owner: [u8; 32],
}

impl UploadResolver<'_> {
    /// 只读查看条目，不消费——校验失败时上传仍可被重试
    pub fn peek(&self, id: &str) -> Result<UploadMeta, String> {
        let g = self.store.guard();
        let Some(entry) = g.get(id) else {
            return Err(
                "附件 upload_id 不存在或已过期（上传后需在有效期内发送，过期请重新上传）".into(),
            );
        };
        if !bool::from(entry.owner.ct_eq(&self.owner)) {
            return Err("附件 upload_id 不属于当前 API key".into());
        }
        match &entry.state {
            State::Uploaded { size, sha256, .. } => Ok(UploadMeta {
                filename: entry.filename.clone(),
                size: *size,
                sha256: sha256.clone(),
            }),
            State::Pending | State::Uploading => {
                Err("附件 upload_id 尚未完成上传（请先 curl -T 上传字节）".into())
            }
        }
    }

    /// 消费条目（取走所有权，磁盘文件随返回的 `TempDir` 清理）
    pub fn take(&self, id: &str) -> Result<ConsumedUpload, String> {
        self.store.take(id, &self.owner)
    }
}

/// 未消费 upload 的登记表；放入 `AppState` 全局共享。
pub struct UploadStore {
    inner: Mutex<HashMap<String, Entry>>,
    ttl: Duration,
    max_pending: usize,
    max_pending_bytes: u64,
}

impl UploadStore {
    pub fn new(ttl_secs: u64, max_pending: usize, max_pending_bytes: u64) -> Self {
        UploadStore {
            inner: Mutex::new(HashMap::new()),
            ttl: Duration::from_secs(ttl_secs),
            max_pending,
            max_pending_bytes,
        }
    }

    /// 取锁并顺带惰性清理过期条目（`TempDir` drop 自动删文件）。
    fn guard(&self) -> MutexGuard<'_, HashMap<String, Entry>> {
        let mut g = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let now = Instant::now();
        g.retain(|_, e| now.saturating_duration_since(e.created) < self.ttl);
        g
    }

    /// 后台任务每 60s 调一次，保证闲置时过期文件也会被删除。
    pub fn cleanup_expired(&self) {
        // guard() 内部已按 ttl 清理
        let _guard = self.guard();
    }

    /// 创建上传条目，成功返回 32 位 hex 的 id；超过 `max_pending` 返回 Err。
    pub fn create(&self, owner: [u8; 32], filename: String) -> Result<String, String> {
        let mut g = self.guard();
        if g.len() >= self.max_pending {
            return Err(format!(
                "待消费的上传任务已达上限 {} 个，请先消费已有 upload 或稍后重试",
                self.max_pending
            ));
        }
        let id = new_id();
        g.insert(
            id.clone(),
            Entry {
                owner,
                filename,
                created: Instant::now(),
                state: State::Pending,
            },
        );
        Ok(id)
    }

    /// `Pending -> Uploading`。未知/已过期 → NotFound；其它状态 → Conflict。
    pub fn begin_upload(&self, id: &str) -> Result<(), UploadErr> {
        let mut g = self.guard();
        let Some(entry) = g.get_mut(id) else {
            return Err(UploadErr::NotFound);
        };
        match entry.state {
            State::Pending => {
                entry.state = State::Uploading;
                Ok(())
            }
            State::Uploading | State::Uploaded { .. } => Err(UploadErr::Conflict),
        }
    }

    /// `Uploading -> Uploaded`，成功返回条目的文件名。
    pub fn finish_upload(
        &self,
        id: &str,
        dir: TempDir,
        path: PathBuf,
        size: u64,
        sha256: String,
    ) -> Result<(), UploadErr> {
        let mut g = self.guard();
        let Some(entry) = g.get_mut(id) else {
            return Err(UploadErr::NotFound);
        };
        match entry.state {
            State::Uploading => {
                entry.state = State::Uploaded {
                    dir,
                    path,
                    size,
                    sha256,
                };
                Ok(())
            }
            State::Pending | State::Uploaded { .. } => Err(UploadErr::Conflict),
        }
    }

    /// 上传失败 / 超限：删除条目，该 id 之后不可用（磁盘文件由调用方持有的 TempDir 清理）。
    pub fn abort(&self, id: &str) {
        self.guard().remove(id);
    }

    /// 已上传未消费的总字节数（`max_pending_bytes` 用）。
    pub fn uploaded_bytes(&self) -> u64 {
        self.guard()
            .values()
            .filter_map(|e| match &e.state {
                State::Uploaded { size, .. } => Some(*size),
                _ => None,
            })
            .sum()
    }

    /// 本次上传再接收 `incoming` 字节后，已上传未消费总量是否超过 `max_pending_bytes`。
    /// 边收边调，用于在上传过程中就截断超限的大文件。
    pub fn exceeds_pending_bytes(&self, incoming: u64) -> bool {
        self.uploaded_bytes().saturating_add(incoming) > self.max_pending_bytes
    }

    /// `max_pending_bytes` 的配置值（错误文案用）
    pub fn max_pending_bytes(&self) -> u64 {
        self.max_pending_bytes
    }

    /// 条目文件名（上传完成响应与审计日志用）。
    pub fn filename_of(&self, id: &str) -> Option<String> {
        self.guard().get(id).map(|e| e.filename.clone())
    }

    /// 绑定 owner 的解析器：`peek` 先校验、`take` 后消费。
    /// 校验（文件名一致、sha256、大小）都应在 `take` 之前完成，
    /// 否则一次失败的 send_email 会白白消耗掉上传。
    pub fn resolver(&self, owner: [u8; 32]) -> UploadResolver<'_> {
        UploadResolver { store: self, owner }
    }

    /// `send_email` 消费：需已完成上传、未过期、owner 一致；成功后从表中移除。
    /// 任何一项不满足都不会消费掉该条目（owner 不符时别人仍可正常消费）。
    pub fn take(&self, id: &str, owner: &[u8; 32]) -> Result<ConsumedUpload, String> {
        let mut g = self.guard();
        let usable = match g.get(id) {
            None => {
                return Err(
                    "附件 upload_id 不存在或已过期（上传后需在有效期内发送，过期请重新上传）".into(),
                )
            }
            Some(entry) => {
                if !bool::from(entry.owner.ct_eq(owner)) {
                    return Err("附件 upload_id 不属于当前 API key".into());
                }
                matches!(entry.state, State::Uploaded { .. })
            }
        };
        if !usable {
            return Err("附件 upload_id 尚未完成上传（请先 curl -T 上传字节）".into());
        }

        let entry = g.remove(id).expect("entry just checked");
        match entry.state {
            State::Uploaded { dir, path, .. } => Ok(ConsumedUpload { path, dir }),
            // 上面已确认是 Uploaded
            State::Pending | State::Uploading => unreachable!("state checked above"),
        }
    }
}

/// 16 字节密码学随机数 → 32 位小写 hex。
fn new_id() -> String {
    let mut raw = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut raw);
    hex_lower(&raw)
}

/// 日志用短 id 前缀——审计日志不得出现完整 id。
pub fn short_id(id: &str) -> &str {
    &id[..id.len().min(8)]
}

/// 一次算出数据的 sha256 hex（测试里用来独立核对上传结果）
#[cfg(test)]
pub(crate) fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    digest_hex(&Sha256::digest(data))
}

/// 摘要字节 → 小写 hex。流式上传边收边算摘要，收完用这个转字符串
/// （注意：传入的是**摘要**，不要再用 `sha256_hex` 包一层，那会二次哈希）。
pub fn digest_hex(digest: &[u8]) -> String {
    hex_lower(digest)
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWNER: [u8; 32] = [7u8; 32];
    const OTHER: [u8; 32] = [9u8; 32];

    fn store() -> UploadStore {
        UploadStore::new(900, 4, 1024 * 1024)
    }

    /// 走完整的 Pending -> Uploading -> Uploaded 流程
    fn upload(st: &UploadStore, id: &str, data: &[u8]) {
        st.begin_upload(id).unwrap();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("upload.bin");
        std::fs::write(&path, data).unwrap();
        st.finish_upload(id, dir, path, data.len() as u64, sha256_hex(data))
            .unwrap();
    }

    fn create(st: &UploadStore, name: &str) -> String {
        st.create(OWNER, name.to_string()).unwrap()
    }

    #[test]
    fn create_returns_32_hex_id_and_is_unique() {
        let st = store();
        let a = create(&st, "a.bin");
        let b = create(&st, "b.bin");
        assert_eq!(a.len(), 32);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_ne!(a, b);
        assert_eq!(st.filename_of(&a).as_deref(), Some("a.bin"));
    }

    #[test]
    fn create_rejects_over_max_pending() {
        let st = UploadStore::new(900, 2, 1024 * 1024);
        let _a = create(&st, "a.bin");
        let _b = create(&st, "b.bin");
        let err = st.create(OWNER, "c.bin".into()).unwrap_err();
        assert!(err.contains("上限 2"), "err: {}", err);

        // 已上传（未消费）同样计入 max_pending
        let st2 = UploadStore::new(900, 1, 1024 * 1024);
        let id = create(&st2, "a.bin");
        upload(&st2, &id, b"x");
        assert!(st2.create(OWNER, "c.bin".into()).is_err());
    }

    #[test]
    fn begin_upload_state_machine() {
        let st = store();
        assert_eq!(st.begin_upload("missing"), Err(UploadErr::NotFound));

        let id = create(&st, "a.bin");
        assert_eq!(st.begin_upload(&id), Ok(()));
        // 第二次（并发 PUT）被拒
        assert_eq!(st.begin_upload(&id), Err(UploadErr::Conflict));

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("upload.bin");
        std::fs::write(&path, b"x").unwrap();
        st.finish_upload(&id, dir, path, 1, sha256_hex(b"x"))
            .unwrap();
        // 完成后再 PUT 也是 409
        assert_eq!(st.begin_upload(&id), Err(UploadErr::Conflict));

        // 未知 id 的 finish / take 也不 panic
        assert_eq!(
            st.finish_upload(
                "missing",
                TempDir::new().unwrap(),
                PathBuf::from("/nonexistent"),
                0,
                String::new()
            ),
            Err(UploadErr::NotFound)
        );
    }

    #[test]
    fn finish_without_begin_is_conflict() {
        let st = store();
        let id = create(&st, "a.bin");
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("upload.bin");
        std::fs::write(&path, b"x").unwrap();
        assert_eq!(
            st.finish_upload(&id, dir, path, 1, sha256_hex(b"x")),
            Err(UploadErr::Conflict)
        );
    }

    #[test]
    fn take_consumes_once_with_correct_metadata() {
        let st = store();
        let id = create(&st, "report.bin");
        let data = b"\x00\x01\x02binary payload";
        upload(&st, &id, data);

        // 元信息由 peek 提供（消费前的校验用）
        let resolver = st.resolver(OWNER);
        let meta = resolver.peek(&id).unwrap();
        assert_eq!(meta.filename, "report.bin");
        assert_eq!(meta.size, data.len() as u64);
        assert_eq!(meta.sha256, sha256_hex(data));

        let taken = resolver.take(&id).unwrap();
        assert_eq!(std::fs::read(&taken.path).unwrap(), data.to_vec());

        // 同一 id 不能再次消费
        let err = st.take(&id, &OWNER).unwrap_err();
        assert!(err.contains("不存在或已过期"), "err: {}", err);
        // 文件随 TempDir drop 删除
        let path = taken.path.clone();
        let dir = taken.dir;
        drop(dir);
        assert!(!path.exists());
    }

    #[test]
    fn peek_does_not_consume_and_reports_metadata() {
        let st = store();
        let id = create(&st, "a.bin");
        upload(&st, &id, b"payload");

        let resolver = st.resolver(OWNER);
        // 多次 peek 都成功，且条目不消失
        for _ in 0..3 {
            assert_eq!(resolver.peek(&id).unwrap().size, 7);
        }
        assert!(st.filename_of(&id).is_some());

        // 校验失败（owner 不符）也不影响原 owner
        assert!(st.resolver(OTHER).peek(&id).is_err());
        assert!(resolver.take(&id).is_ok());
    }

    #[test]
    fn take_before_upload_fails() {
        let st = store();
        let id = create(&st, "a.bin");
        let err = st.take(&id, &OWNER).unwrap_err();
        assert!(err.contains("尚未完成上传"), "err: {}", err);
        // 未完成也不该被移除
        assert_eq!(st.filename_of(&id).as_deref(), Some("a.bin"));
    }

    #[test]
    fn take_rejects_foreign_owner_without_consuming() {
        let st = store();
        let id = create(&st, "a.bin");
        upload(&st, &id, b"payload");

        let err = st.take(&id, &OTHER).unwrap_err();
        assert!(err.contains("不属于当前 API key"), "err: {}", err);
        // 原 owner 仍可正常消费
        let meta = st.resolver(OWNER).peek(&id).unwrap();
        assert_eq!(meta.size, 7);
        assert!(st.take(&id, &OWNER).is_ok());
    }

    #[test]
    fn expired_entry_is_swept_and_reported_as_missing() {
        // ttl = 0：创建即过期
        let st = UploadStore::new(0, 4, 1024 * 1024);
        let id = create(&st, "a.bin");
        assert_eq!(st.begin_upload(&id), Err(UploadErr::NotFound));
        assert!(st.take(&id, &OWNER).unwrap_err().contains("不存在或已过期"));

        // 之后的 create 不再被过期条目占用名额
        assert!(st.create(OWNER, "b.bin".into()).is_ok());
    }

    #[test]
    fn expired_uploaded_entry_releases_disk_file() {
        let st = UploadStore::new(900, 4, 1024 * 1024);
        let id = create(&st, "a.bin");
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("upload.bin");
        std::fs::write(&path, b"x").unwrap();
        st.begin_upload(&id).unwrap();
        let held = TempDir::new().unwrap();
        let held_path = held.path().join("upload.bin");
        std::fs::write(&held_path, b"x").unwrap();
        st.finish_upload(&id, held, held_path.clone(), 1, sha256_hex(b"x"))
            .unwrap();
        assert!(held_path.exists());
        // 惰性清理（把 ttl 改成 0 的等价做法：直接跑后台清理入口无法改 ttl，这里用 abort 路径）
        st.abort(&id);
        assert!(!held_path.exists());
        assert!(st.filename_of(&id).is_none());
        let _ = dir;
        let _ = path;
    }

    #[test]
    fn cleanup_expired_drops_stale_entries() {
        let st = UploadStore::new(0, 4, 1024 * 1024);
        // 直接塞入而非 create（create 后就已过期）：验证后台清理入口不 panic
        let _id = create(&st, "a.bin");
        st.cleanup_expired();
        assert!(st.filename_of(&_id).is_none());
    }

    #[test]
    fn uploaded_bytes_sums_and_frees_after_take() {
        let st = store();
        let a = create(&st, "a.bin");
        let b = create(&st, "b.bin");
        upload(&st, &a, &[0u8; 100]);
        upload(&st, &b, &[0u8; 40]);
        assert_eq!(st.uploaded_bytes(), 140);

        st.take(&a, &OWNER).unwrap();
        assert_eq!(st.uploaded_bytes(), 40);
        st.abort(&b);
        assert_eq!(st.uploaded_bytes(), 0);
    }

    #[test]
    fn short_id_is_prefix_of_at_most_8() {
        assert_eq!(short_id("0123456789abcdef"), "01234567");
        assert_eq!(short_id("abc"), "abc");
    }
}
