//! 首页静态解析：`x-fe-version`、entry bundle 哈希、meta.stack 构造。
//!
//! 纯文本解析（无 I/O）：`duckai-upstream` 负责取回 `GET {DUCKAI_BASE}/`，本层从 HTML 里
//! 提取两个服务端要校验的动态值：
//!
//! - `data-version-tag` + `data-version-sha` → `x-fe-version: <tag>-<sha>`（§2.2.3）；
//! - `entry.duckai.<hash>.js` → `meta.stack` 中的 bundle 坈架（挑战环境指纹的一部分）。
//!
//! 解析失败时调用方回落到本模块提供的最近一次已知常量（P1-8 精神：永远有可用兜底）。

/// 兜底 `x-fe-version`（与线上抓取一致；首页不可达时使用）。
pub const DEFAULT_FE_VERSION: &str =
    "serp_20261002_113931_ET-16077f4eafa643a1d3b77a0392138879174cae48";
/// 兜底 entry bundle 哈希（构造 `meta.stack` 用）。
pub const DEFAULT_ENTRY_BUNDLE_HASH: &str = "7eaf8f9b262254b03739";

/// `entry.duckai.<hash>.js` 调用栈里观测到的两处坐标（当前 bundle，线上验证过被接受）。
const STACK_COORD_FN: u64 = 1_952_136;
const STACK_COORD_ASYNC: u64 = 1_709_351;

/// 首页解析结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeMeta {
    /// `x-fe-version` 请求头完整值：`<tag>-<sha>`。
    pub fe_version: String,
    /// entry bundle 哈希（`entry.duckai.{hash}.js`）。
    pub bundle_hash: String,
    /// 原始 `data-version-tag`，例如 `serp_20261002_113931_ET`。
    pub tag: String,
}

/// 从首页 HTML 提取 [`FeMeta`]；失败返回 `None`（调用方回落 DEFAULT_*）。
pub fn parse_home(html: &str) -> Option<FeMeta> {
    let tag = attr(html, "data-version-tag")?;
    let sha = attr(html, "data-version-sha")?;
    let bundle_hash = bundle_hash_of(html).unwrap_or_else(|| DEFAULT_ENTRY_BUNDLE_HASH.to_string());
    Some(FeMeta {
        fe_version: format!("{tag}-{sha}"),
        bundle_hash,
        tag,
    })
}

/// 兜底解析：直接给出当前已知常量（首页抓取失败时的降级）。
pub fn fallback_fe_meta() -> FeMeta {
    let tag = DEFAULT_FE_VERSION
        .rsplit_once('-')
        .map(|(tag, _)| tag)
        .unwrap_or(DEFAULT_FE_VERSION);
    FeMeta {
        fe_version: DEFAULT_FE_VERSION.to_string(),
        bundle_hash: DEFAULT_ENTRY_BUNDLE_HASH.to_string(),
        tag: tag.to_string(),
    }
}

/// 在任意 HTML 中查找 `attr="value"` 形式的属性值。
fn attr(html: &str, attr_name: &str) -> Option<String> {
    let needle = format!("{attr_name}=\"");
    let start = html.find(&needle)? + needle.len();
    let rest = &html[start..];
    let end = rest.find('"')?;
    let value = &rest[..end];
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

/// 提取 `entry.duckai.<hash>.js` 的哈希。
pub fn bundle_hash_of(html: &str) -> Option<String> {
    let idx = html.find("entry.duckai.")?;
    let rest = &html[idx + "entry.duckai.".len()..];
    let end = rest.find(".js")?;
    let hash = &rest[..end];
    if hash.len() >= 8 && hash.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(hash.to_string())
    } else {
        None
    }
}

/// 构造挑战 `meta.stack`：与线上观测帧同构。
///
/// `entry.duckai.{hash}.js` 来自当次首页（随发布更新），坐标为当前 bundle 实测值；
/// 发布变更后如服务端收紧校验，可在浏览器适配器（§5.3）里改用真实 `Error.stack` 注入。
pub fn stack_for_bundle(bundle_hash: &str) -> String {
    format!(
        "Error\nat l (https://duck.ai/dist/duckai-dist/entry.duckai.{hash}.js:2:{fn_c})\nat async https://duck.ai/dist/duckai-dist/entry.duckai.{hash}.js:2:{async_c}",
        hash = bundle_hash,
        fn_c = STACK_COORD_FN,
        async_c = STACK_COORD_ASYNC,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_homepage() {
        let html = include_str!("../../../fixtures/home.html");
        let meta = parse_home(html).expect("parse");
        assert_eq!(meta.tag, "serp_20261002_113931_ET");
        assert_eq!(meta.fe_version, DEFAULT_FE_VERSION);
        assert_eq!(meta.bundle_hash, DEFAULT_ENTRY_BUNDLE_HASH);
    }

    #[test]
    fn missing_attrs_return_none() {
        assert!(parse_home("<html><body>nothing</body></html>").is_none());
    }

    #[test]
    fn stack_shape() {
        let s = stack_for_bundle("abc123def4567890");
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "Error");
        assert!(lines[1].starts_with(
            "at l (https://duck.ai/dist/duckai-dist/entry.duckai.abc123def4567890.js:2:"
        ));
        assert!(lines[2].starts_with("at async https://duck.ai/dist/duckai-dist/entry.duckai."));
        assert!(lines[2].ends_with(":2:1709351"));
    }
}
