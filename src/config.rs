use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};

/// 模型发现搜索路径优先级
fn model_search_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();

    // Windows 开发环境：支持 ASUNA_DEV_ROOT 环境变量
    #[cfg(windows)]
    {
        if let Ok(dev_root) = std::env::var("ASUNA_DEV_ROOT") {
            paths.push(PathBuf::from(dev_root).join("models/multilingual-e5-small"));
        }
    }

    // 跨平台便携路径
    paths.push(PathBuf::from("~/.rustrag/models/multilingual-e5-small"));
    paths.push(PathBuf::from("~/.asuna/models/multilingual-e5-small"));

    paths
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub data_dir: PathBuf,
    pub profile_id: String,

    pub conversation: ConversationConfig,
    pub memory: MemoryConfig,
    pub search: SearchConfig,
    pub embedding: EmbeddingConfig,

    pub db_path: PathBuf,

    /// 手动指定的模型目录（最高优先级）
    pub model_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationConfig {
    pub enabled: bool,
    pub auto_embed: bool,
    pub preview_length: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryConfig {
    pub memory_enabled: bool,
    pub user_profile_enabled: bool,
    pub memory_char_limit: usize,
    pub user_char_limit: usize,
    pub security_scan: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchConfig {
    pub default_top_k: usize,
    pub search_mode: String,
    pub fts_enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingConfig {
    pub model_name: String,
    pub dimensions: usize,
    pub batch_size: usize,
}

impl Default for Config {
    fn default() -> Self {
        let home = dirs_home();
        let data_dir = home.join(".asuna");

        Self {
            data_dir: data_dir.clone(),
            profile_id: "default".to_string(),

            conversation: ConversationConfig {
                enabled: true,
                auto_embed: true,
                preview_length: 200,
            },
            memory: MemoryConfig {
                memory_enabled: true,
                user_profile_enabled: true,
                memory_char_limit: 2200,
                user_char_limit: 1375,
                security_scan: true,
            },
            search: SearchConfig {
                default_top_k: 5,
                search_mode: "hybrid".to_string(),
                fts_enabled: true,
            },
            embedding: EmbeddingConfig {
                model_name: "multilingual-e5-small".to_string(),
                dimensions: 384,
                batch_size: 32,
            },
            db_path: data_dir.join("memory.db"),
            model_path: None,
        }
    }
}

impl Config {
    /// 从 JSON 文件加载配置，若不存在则使用默认值
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        if path.exists() {
            let content = std::fs::read_to_string(path)?;
            let mut config: Config = serde_json::from_str(&content)?;
            // 展开 ~ 路径
            config.data_dir = expand_tilde(&config.data_dir);
            if config.db_path.is_relative() {
                config.db_path = config.data_dir.join(&config.db_path);
            }
            Ok(config)
        } else {
            Ok(Self::default())
        }
    }

    /// 智能发现模型目录
    pub fn discover_model_dir(&self) -> Option<PathBuf> {
        // 1. 手动指定
        if let Some(ref p) = self.model_path {
            if p.join("model_O4.onnx").exists() {
                return Some(p.clone());
            }
        }

        // 2. 搜索预设路径
        for p in model_search_paths() {
            let p = expand_tilde(&p);
            if p.join("model_O4.onnx").exists() {
                return Some(p);
            }
        }

        None
    }

    /// 获取 profile 对应的数据目录
    pub fn profile_dir(&self) -> PathBuf {
        self.data_dir.join("profiles").join(&self.profile_id)
    }

    /// 获取对话归档目录（按 profile 隔离）
    pub fn conversations_dir(&self) -> PathBuf {
        self.profile_dir().join("conversations")
    }

    /// 获取成长记忆目录（按 profile 隔离）
    pub fn memory_dir(&self) -> PathBuf {
        self.profile_dir().join("memory")
    }

    /// 获取 profile 对应的数据库路径
    pub fn profile_db_path(&self) -> PathBuf {
        self.profile_dir().join("memory.db")
    }

    /// 确保所有需要的目录存在
    pub fn ensure_dirs(&self) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.data_dir)?;
        std::fs::create_dir_all(self.profile_dir())?;
        std::fs::create_dir_all(self.conversations_dir())?;
        std::fs::create_dir_all(self.memory_dir())?;
        std::fs::create_dir_all(self.data_dir.join("models"))?;
        Ok(())
    }

    /// 列出所有可用 profile
    pub fn list_profiles(&self) -> Vec<String> {
        let profiles_dir = self.data_dir.join("profiles");
        let mut profiles = Vec::new();
        if let Ok(entries) = std::fs::read_dir(profiles_dir) {
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    if let Some(name) = entry.file_name().to_str() {
                        profiles.push(name.to_string());
                    }
                }
            }
        }
        profiles.sort();
        profiles
    }
}

fn dirs_home() -> PathBuf {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

pub fn expand_tilde(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if s.starts_with("~") {
        let home = dirs_home();
        home.join(s.strip_prefix("~/").unwrap_or(s.strip_prefix("~").unwrap_or(&s)))
    } else {
        path.to_path_buf()
    }
}
