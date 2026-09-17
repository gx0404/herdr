//! 只持久化账号引用、公开身份与 pane 绑定，禁止写入凭据或用量报文。
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Default, Serialize, Deserialize)]
pub(super) struct Saved {
    pub bindings: HashMap<String, String>,
    pub identities: HashMap<String, String>,
}

fn path() -> std::path::PathBuf {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(crate::api::socket_path().to_string_lossy().as_bytes());
    crate::config::state_dir()
        .join("account-usage")
        .join(format!("{hash:x}.json"))
}

pub(super) fn load() -> Saved {
    std::fs::read(path())
        .ok()
        .filter(|bytes| bytes.len() <= 1024 * 1024)
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

pub(super) fn store(saved: &Saved) {
    let path = path();
    let result = (|| -> std::io::Result<()> {
        let Some(parent) = path.parent() else {
            return Ok(());
        };
        std::fs::create_dir_all(parent)?;
        let bytes = serde_json::to_vec(saved).map_err(std::io::Error::other)?;
        let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
        std::fs::write(&temporary, bytes)?;
        crate::platform::replace_file(&temporary, &path)
    })();
    if let Err(error) = result {
        tracing::warn!(%error, "保存账号绑定失败");
    }
}
