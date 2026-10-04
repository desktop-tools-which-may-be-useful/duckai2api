use serde::{Deserialize, Serialize};

/// 模型来源：上游实时拉取或本地快照兜底（P1-8 修正位）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelSource {
    Upstream,
    Snapshot,
}

/// 一条模型记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    /// OpenAI /v1/models 的 `owned_by`（如 `duck.ai` / `tinfoil`）。
    #[serde(default = "default_owned_by")]
    pub owned_by: String,
    /// 别名（旧名 → 新名映射，客户端可直接请求别名）。
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default = "default_source")]
    pub source: ModelSource,
}

fn default_owned_by() -> String {
    "duck.ai".to_string()
}

fn default_source() -> ModelSource {
    ModelSource::Snapshot
}

impl ModelInfo {
    pub fn snapshot(id: impl Into<String>, owned_by: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            owned_by: owned_by.into(),
            aliases: Vec::new(),
            source: ModelSource::Snapshot,
        }
    }

    pub fn with_alias(mut self, alias: impl Into<String>) -> Self {
        self.aliases.push(alias.into());
        self
    }

    pub fn matches(&self, query: &str) -> bool {
        self.id == query || self.aliases.iter().any(|a| a == query)
    }
}

/// 启动时拉取 + 缓存 + 快照兜底的模型目录。
#[derive(Debug, Clone)]
pub struct ModelCatalog {
    models: Vec<ModelInfo>,
    source: ModelSource,
}

/// 2026-10 线上抓取快照（脱敏：无个人数据），上游不可达时兜底（P1-8）。
pub const SNAPSHOT_MODELS: &[(&str, &str)] = &[
    ("gpt-5.6-terra", "duck.ai"),
    ("gpt-5.6-luna", "duck.ai"),
    ("gpt-5.4-mini", "duck.ai"),
    ("claude-sonnet-4-6", "anthropic"),
    ("claude-haiku-4-5", "anthropic"),
    ("gpt-5.6-sol", "duck.ai"),
    ("claude-opus-4-8", "anthropic"),
    ("mistral-small-2603", "mistral"),
    ("tinfoil/gpt-oss-120b", "tinfoil"),
    ("tinfoil/gemma4-31b", "tinfoil"),
];

impl Default for ModelCatalog {
    fn default() -> Self {
        Self::snapshot()
    }
}

impl ModelCatalog {
    /// 快照兜底目录。
    pub fn snapshot() -> Self {
        Self {
            models: SNAPSHOT_MODELS
                .iter()
                .map(|(id, owner)| ModelInfo::snapshot(*id, *owner))
                .collect(),
            source: ModelSource::Snapshot,
        }
    }

    /// 以上游返回构建（启动拉取成功时调用）。
    pub fn from_upstream(models: Vec<ModelInfo>) -> Self {
        if models.is_empty() {
            return Self::snapshot();
        }
        Self {
            models,
            source: ModelSource::Upstream,
        }
    }

    pub fn source(&self) -> ModelSource {
        self.source
    }

    pub fn list(&self) -> &[ModelInfo] {
        &self.models
    }

    /// 按 id 或别名解析；未知模型 → None（API 层 404，不再透传上游，P1-8）。
    pub fn resolve(&self, query: &str) -> Option<&ModelInfo> {
        self.models.iter().find(|m| m.matches(query))
    }
}
