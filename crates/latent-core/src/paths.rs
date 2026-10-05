//! latent 用户数据目录解析(唯一权威):settings.json / models.json /
//! web-search.json / .latentignore / skills / agents / system-prompt.md /
//! sessions 等用户级数据都落在同一个目录下。
//!
//! 解析优先级:
//! 1. `LATENT_HOME` 环境变量 —— 显式指定,原样使用(不做 `~` 展开,推荐绝对路径);
//! 2. 旧版目录 `~/.latent` —— 存在即沿用,老用户数据原地兼容;
//! 3. 默认 `~/.config/latent`(XDG 风格,写入时按需创建)。
//!
//! 依赖方向约定:本模块是唯一解析点。进程入口层(CLI 装配/各模式入口)调
//! [`latent_dir`] 读环境变量,得到的目录作为参数逐层下传 —— 纯函数面一律收
//! 解析结果而非读环境,保证可测性与环境解耦。

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// 数据目录的解析来源(供启动诊断区分)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LatentDirSource {
    /// `LATENT_HOME` 环境变量显式指定。
    EnvOverride,
    /// 旧版 `~/.latent` 存在,原地沿用。
    LegacyDotLatent,
    /// 默认 `~/.config/latent`。
    DefaultConfig,
}

/// 纯函数面(可测):按 `LATENT_HOME` → 旧版 `~/.latent`(存在即沿用)→
/// `~/.config/latent` 解析数据目录;环境变量与 home 都缺失 = `None`。
/// 空环境变量视为未设置。
pub fn resolve_latent_dir(
    home: Option<&Path>,
    env_override: Option<&OsStr>,
) -> (Option<PathBuf>, LatentDirSource) {
    if let Some(dir) = env_override
        .map(PathBuf::from)
        .filter(|dir| !dir.as_os_str().is_empty())
    {
        return (Some(dir), LatentDirSource::EnvOverride);
    }
    let Some(home) = home else {
        return (None, LatentDirSource::DefaultConfig);
    };
    let legacy = home.join(".latent");
    if legacy.is_dir() {
        return (Some(legacy), LatentDirSource::LegacyDotLatent);
    }
    (
        Some(home.join(".config").join("latent")),
        LatentDirSource::DefaultConfig,
    )
}

/// 运行期入口:读 `LATENT_HOME` 环境变量解析数据目录。
/// 只在进程入口层调用;下层函数一律收解析结果作参数。
pub fn latent_dir(home: Option<&Path>) -> Option<PathBuf> {
    let env = std::env::var_os("LATENT_HOME");
    resolve_latent_dir(home, env.as_deref()).0
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "latent_paths_test_{tag}_{}_{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            TempDir(path)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn env_override_wins_over_everything() {
        let home = TempDir::new("env");
        std::fs::create_dir_all(home.0.join(".latent")).unwrap();
        // 旧版目录存在时环境变量仍然优先;原样使用(不拼 .latent、不展开 ~)
        let (dir, source) = resolve_latent_dir(
            Some(&home.0),
            Some(OsStr::new("/custom/latent-home")),
        );
        assert_eq!(dir, Some(PathBuf::from("/custom/latent-home")));
        assert_eq!(source, LatentDirSource::EnvOverride);
    }

    #[test]
    fn env_override_without_home_still_resolves() {
        let (dir, source) = resolve_latent_dir(None, Some(OsStr::new("/custom/latent-home")));
        assert_eq!(dir, Some(PathBuf::from("/custom/latent-home")));
        assert_eq!(source, LatentDirSource::EnvOverride);
    }

    #[test]
    fn empty_env_override_is_ignored() {
        let home = TempDir::new("empty-env");
        let (dir, source) = resolve_latent_dir(Some(&home.0), Some(OsStr::new("")));
        assert_eq!(dir, Some(home.0.join(".config").join("latent")));
        assert_eq!(source, LatentDirSource::DefaultConfig);
    }

    #[test]
    fn legacy_dot_latent_used_when_present() {
        let home = TempDir::new("legacy");
        std::fs::create_dir_all(home.0.join(".latent")).unwrap();
        let (dir, source) = resolve_latent_dir(Some(&home.0), None);
        assert_eq!(dir, Some(home.0.join(".latent")));
        assert_eq!(source, LatentDirSource::LegacyDotLatent);
    }

    #[test]
    fn default_is_config_dir_when_no_legacy() {
        let home = TempDir::new("fresh");
        // .latent 是普通文件(非目录)时不视为旧版数据目录
        std::fs::write(home.0.join(".latent"), b"not a dir").unwrap();
        let (dir, source) = resolve_latent_dir(Some(&home.0), None);
        assert_eq!(dir, Some(home.0.join(".config").join("latent")));
        assert_eq!(source, LatentDirSource::DefaultConfig);
    }

    #[test]
    fn missing_home_and_env_is_none() {
        let (dir, source) = resolve_latent_dir(None, None);
        assert_eq!(dir, None);
        assert_eq!(source, LatentDirSource::DefaultConfig);
    }
}
