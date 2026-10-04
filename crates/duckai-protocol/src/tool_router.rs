//! Relay 侧工具意图路由（原 Python `toolrouter.py`，**按 §1.2 P0-2 重写**）。
//!
//! Duck.ai 无原生函数工具，客户端（Claude Code / Codex 等）带 `tools` 数组进来时，
//! relay 从**最后一条用户话术**里判定意图并**合成**一个 `tool_use`（只合成，不执行）。
//!
//! P0-2 修正要点（对应架构文档缺陷清单）：
//! - 锚定提取：`Write the file main.py with content foo` → `main.py`+`foo`
//!   （原贪婪正则截成 `n.py`/空内容）；
//! - 引号容错：`Write 'hi' to notes.md` → 路由成功（原落回普通聊天）；
//! - `create file … with …` 动词支持（原正则只认 `\bwrite\b`，注释与实现不一致）；
//! - **参数完整性校验**：`Edit` 必须同时给出非空 `old_string`/`new_string`，
//!   `Write` 必须给出非空 `content`——不完整宁可不路由，绝不发出空参数 tool_use；
//! - 超长命令：上限 8192 字节（原 `{2,300}` 静默截断），超限不路由；
//! - 无扩展名 Read：`read README` → `README`（原要求必带 `.ext` 或 `/`）；
//! - 文件名提取不截断：先剥离引号与内容片段，再取第一个“像路径”的完整 token。

use std::collections::BTreeSet;

use regex::Regex;
use serde_json::{Map, Value};

use duckai_types::{ChatTurn, ContentBlock, Role, TurnContent};

use crate::tool_envelope::ToolCallEnvelope;

/// 本 relay 认识的内置工具（名字对齐 Claude Code built-ins）。
pub const KNOWN_TOOLS: [&str; 7] = ["Read", "Glob", "Grep", "Write", "Edit", "WebFetch", "Bash"];

/// Bash 命令长度上限（字节）；超限视为不完整，不路由。
pub const MAX_BASH_COMMAND: usize = 8192;

/// 路由优先级：Bash 兜底最后，Read/Glob 先赢重叠。
const ORDER: [&str; 7] = ["Read", "Glob", "Grep", "Write", "Edit", "WebFetch", "Bash"];

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).expect("tool_router regex must compile")
}

/// 去掉 token 首尾的引号/反引号/常见标点。
fn clean_token(t: &str) -> String {
    t.trim_matches(|c: char| {
        matches!(
            c,
            '"' | '\'' | '`' | ',' | ';' | ':' | ')' | '(' | '[' | ']' | '。' | '，'
        )
    })
    .to_string()
}

/// “像路径”的完整 token：含 `/`，或含 `.扩展` 且扩展含字母（`main.py`、`.gitignore`）。
/// 纯版本号（`2.0`）与空串不算——防止截断与误配。
fn looks_like_path(t: &str) -> bool {
    let t = clean_token(t);
    if t.is_empty() || t.len() > 4096 {
        return false;
    }
    if t.contains('/') {
        return true;
    }
    if let Some(idx) = t.rfind('.') {
        if idx == 0 {
            return t.len() >= 2; // .gitignore
        }
        let suf = &t[idx + 1..];
        return !suf.is_empty()
            && suf.chars().all(|c| c.is_ascii_alphanumeric())
            && suf.chars().any(|c| c.is_ascii_alphabetic());
    }
    false
}

/// 从文本中取第一个“像路径”的完整 token。
fn first_path_token(text: &str) -> Option<String> {
    text.split_whitespace()
        .map(clean_token)
        .find(|t| looks_like_path(t))
}

/// 把引号片段替换为占位符（路径扫描时忽略内容里的点/斜杠）。
fn mask_quoted(text: &str) -> String {
    let pair = re(r#""[^"]*"|'[^']*'"#);
    pair.replace_all(text, " <quoted> ").into_owned()
}

// ------------------------- 各意图提取器 -------------------------

/// Read：动词 + 路径 token；裸词（README）仅在紧跟动词时接受，防“show me the plan”误路由。
fn read_args(text: &str) -> Option<Map<String, Value>> {
    let m = re(r"(?i)\b(?:read|cat|show|open|display|print|view)\b\s+(.+?)\s*$").captures(text)?;
    let rest = &m[1];
    let filler = re(r"(?i)^(?:the|me|a|an|please|file|this|that|my|following)\s+");
    let mut trimmed = rest.trim();
    let mut consumed_any = false;
    while let Some(fm) = filler.find(trimmed) {
        if fm.start() == 0 && !fm.is_empty() {
            trimmed = &trimmed[fm.end()..];
            consumed_any = true;
        } else {
            break;
        }
    }
    let token = clean_token(trimmed.split_whitespace().next()?);
    if token.is_empty() {
        return None;
    }
    // 填充词被吃掉（说明是散文语境）时必须是真路径；直接跟动词的裸词（README）放行。
    if consumed_any && !looks_like_path(&token) {
        return None;
    }
    // 多词散文（下一个词是小写单词且整体无路径特征）不认
    if !looks_like_path(&token) {
        let first = trimmed.split_whitespace().next()?;
        if first != trimmed {
            // "read the whole readme" 形态：首词后还有别的词且不是路径 → 不完整
            return None;
        }
    }
    Some(json_map(&[("file_path", Value::String(token))]))
}

/// Glob：`glob P` / `find files matching P` / `list all files matching P` / `find *.py`。
fn glob_args(text: &str) -> Option<Map<String, Value>> {
    let pattern = re(
        r"(?i)\b(?:glob\b|find\s+files?\s+matching|list\s+(?:all\s+)?files?\s+matching|list\s+all|find)\s+(\S+)",
    )
    .captures(text)?
    .get(1)?
    .as_str()
    .to_string();
    let pattern = clean_token(&pattern);
    if pattern.is_empty() {
        return None;
    }
    if !(pattern.contains('*') || pattern.contains('/') || pattern.contains('.')) {
        return None;
    }
    Some(json_map(&[("pattern", Value::String(pattern))]))
}

const GREP_STOPWORDS: &[&str] = &[
    "the", "a", "an", "this", "that", "file", "files", "some", "all", "it", "them",
];

/// Grep：带引号 pattern 优先；否则动词后第一个词；`in/under/within` 后取 path。
fn grep_args(text: &str) -> Option<Map<String, Value>> {
    let mut pattern: Option<String> = None;
    if let Some(m) =
        re(r#"(?i)\b(?:grep|search\s+for|find)\b[^"'`]*["']([^"'`]+)["']"#).captures(text)
    {
        pattern = Some(m[1].trim().to_string());
    }
    if pattern.is_none() {
        if let Some(m) = re(r#"(?i)\b(?:grep|search\s+for)\s+([^\s"'`]+)"#).captures(text) {
            let p = clean_token(&m[1]);
            if !p.is_empty() && !GREP_STOPWORDS.contains(&p.to_lowercase().as_str()) {
                pattern = Some(p);
            }
        }
    }
    let pattern = pattern?;
    if pattern.is_empty() {
        return None;
    }
    let mut args = vec![("pattern", Value::String(pattern))];
    if let Some(m) = re(r#"(?i)\b(?:in|under|within)\s+([^\s"'`]+)"#).captures(text) {
        let path = clean_token(&m[1]);
        if !path.is_empty() {
            args.push(("path", Value::String(path)));
        }
    }
    Some(json_map(&args))
}

/// Write：动词（write / create file / save to）+ 完整路径 + 非空内容；缺一不路由。
fn write_args(text: &str) -> Option<Map<String, Value>> {
    let verb = re(
        r"(?i)\b(?:write|create\s+(?:a\s+|the\s+)?file|save\s+(?:the\s+)?(?:file\s+)?to|make\s+(?:a\s+|the\s+)?file)\b",
    )
    .find(text)?;
    let after_verb = &text[verb.end()..];

    // 1) 引号内容：`Write 'hi' to notes.md`
    let quoted = re(r#""([^"]+)"|'([^']+)'"#).captures(after_verb);
    let (content, path_segment) = if let Some(q) = quoted {
        let content = q.get(1).or_else(|| q.get(2))?.as_str().to_string();
        // 路径在引号之外（可能在前也可能在后，如 `Write 'hi' to notes.md`）：
        // 屏蔽全部引号片段后扫描整个动词后缀
        let _ = q.get(0)?;
        (content, mask_quoted(after_verb))
    } else {
        // 2) `with [content] X` → X 是内容，路径在其前
        let with = re(r"(?i)\bwith\s+(?:content\s+|contents\s+|text\s+)?").find(after_verb)?;
        let content = clean_token_trailing(after_verb[with.end()..].trim());
        if content.is_empty() {
            return None; // 内容缺失 → 不完整
        }
        (content, after_verb[..with.start()].to_string())
    };
    if content.is_empty() {
        return None;
    }

    let path = first_path_token(&mask_quoted(&path_segment))?;
    if path == content {
        return None;
    }
    Some(json_map(&[
        ("file_path", Value::String(path)),
        ("content", Value::String(content)),
    ]))
}

fn clean_token_trailing(s: &str) -> String {
    s.trim()
        .trim_end_matches(['。', '，', ',', ';'])
        .to_string()
}

/// Edit：路径 + 显式 old/new 对（replace A with B / swap A and B）；任何一角为空 → 不路由。
fn edit_args(text: &str) -> Option<Map<String, Value>> {
    let verb = re(r"(?i)\b(?:edit|modify|patch)\b").find(text)?;
    let after = &text[verb.end()..];

    // old/new：引号对优先，再退无引号 token 对
    let (old, new) = if let Some(m) =
        re(r#"(?i)\breplace\s+["']([^"']{1,})["']\s+with\s+["']([^"']{1,})["']"#).captures(after)
    {
        (m[1].to_string(), m[2].to_string())
    } else if let Some(m) =
        re(r#"(?i)\breplace\s+([^\s"'`]+)\s+with\s+([^\s"'`]+)"#).captures(after)
    {
        (clean_token(&m[1]), clean_token(&m[2]))
    } else {
        // 不完整：没有 old/new 对 → 宁可不路由
        let m = re(r#"(?i)\bswap\s+([^\s"'`]+)\s+and\s+([^\s"'`]+)"#).captures(after)?;
        (clean_token(&m[1]), clean_token(&m[2]))
    };
    if old.is_empty() || new.is_empty() || old == new {
        return None;
    }

    // 路径：动词后、replace/swap 关键词之前的片段里的第一个像路径 token（避免把 old/new 当文件）
    let pair_kw = re(r"(?i)\b(?:replace|swap)\b").find(after)?;
    let before_pair = mask_quoted(&after[..pair_kw.start()]);
    let path = first_path_token(&before_pair)?;
    Some(json_map(&[
        ("file_path", Value::String(path)),
        ("old_string", Value::String(old)),
        ("new_string", Value::String(new)),
    ]))
}

/// Bash：围栏命令优先；否则 `run/execute [the] [command] …`；>8192 字节 → 不路由。
fn bash_args(text: &str) -> Option<Map<String, Value>> {
    if let Some(m) = re(r"(?is)```(?:sh|bash|shell|zsh|cmd|console)?\s*\n?(.*?)```").captures(text)
    {
        let cmd = m[1].trim().to_string();
        if cmd.is_empty() {
            return None;
        }
        if cmd.len() > MAX_BASH_COMMAND {
            return None;
        }
        return Some(json_map(&[("command", Value::String(cmd))]));
    }
    let m = re(r#"(?i)\b(?:run|execute)\b\s*(?:the\s+)?(?:command\s+)?[:`"']?\s*([^\n]+)"#)
        .captures(text)?;
    let cmd = m[1]
        .trim()
        .trim_matches(|c: char| matches!(c, '`' | '"' | '\''))
        .trim()
        .to_string();
    if cmd.is_empty() || cmd.len() > MAX_BASH_COMMAND {
        return None;
    }
    Some(json_map(&[("command", Value::String(cmd))]))
}

/// WebFetch：同时要 URL 与 fetch/visit 动词。
fn webfetch_args(text: &str) -> Option<Map<String, Value>> {
    let url = re(r#"(https?://[^\s`"')\]]+)"#).captures(text)?[1].to_string();
    if !re(r"(?i)\b(?:fetch|web\s*fetch|open\s+url|visit)\b").is_match(text) {
        return None;
    }
    Some(json_map(&[("url", Value::String(url))]))
}

fn json_map(pairs: &[(&str, Value)]) -> Map<String, Value> {
    let mut m = Map::new();
    for (k, v) in pairs {
        m.insert(k.to_string(), v.clone());
    }
    m
}

// ------------------------- 路由入口 -------------------------

/// 客户端是否已经回过 tool_result（循环执行中 → 不再路由）。
pub fn has_tool_result(turns: &[ChatTurn]) -> bool {
    turns.iter().any(|t| {
        t.role == Role::Tool
            || matches!(&t.content, TurnContent::Blocks(blocks)
                if blocks.iter().any(|b| matches!(b, ContentBlock::ToolResult { .. })))
    })
}

/// 最后一条用户文本（`ChatTurn` 形态）；含 tool_result 轮时返回空串。
pub fn last_user_text(turns: &[ChatTurn]) -> String {
    if has_tool_result(turns) {
        return String::new();
    }
    turns
        .iter()
        .rev()
        .find(|t| t.role == Role::User)
        .map(|t| t.content.text())
        .unwrap_or_default()
}

/// 从话术合成工具调用；`tool_names` 是客户端实际注册的工具名。
/// 仅 `KNOWN_TOOLS ∩ 客户端` 非空时按固定优先级尝试，全部不完整 → `None`（落回普通聊天）。
pub fn route_intent<S: AsRef<str>>(text: &str, tool_names: &[S]) -> Option<ToolCallEnvelope> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let offered: BTreeSet<&str> = tool_names.iter().map(|s| s.as_ref()).collect();
    for name in ORDER {
        if !offered.contains(name) {
            continue;
        }
        let args = match name {
            "Read" => read_args(text),
            "Glob" => glob_args(text),
            "Grep" => grep_args(text),
            "Write" => write_args(text),
            "Edit" => edit_args(text),
            "WebFetch" => webfetch_args(text),
            "Bash" => bash_args(text),
            _ => None,
        };
        if let Some(input) = args {
            return Some(ToolCallEnvelope {
                name: name.to_string(),
                input,
            });
        }
    }
    None
}

/// `ChatTurn` 列表形态的路由：跳过 tool_result 循环中的请求，取最后一条用户话术。
pub fn route_from_turns<S: AsRef<str>>(
    turns: &[ChatTurn],
    tool_names: &[S],
) -> Option<ToolCallEnvelope> {
    if has_tool_result(turns) {
        return None;
    }
    let text = last_user_text(turns);
    route_intent(&text, tool_names)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [&str; 7] = ["Read", "Glob", "Grep", "Write", "Edit", "WebFetch", "Bash"];

    fn route(text: &str) -> Option<(String, Value)> {
        route_intent(text, &ALL).map(|c| (c.name, Value::Object(c.input)))
    }

    // ---- §1.2 P0-2 实测反例（全部必须修正） ----

    #[test]
    fn p0_2_write_main_py_not_truncated() {
        let (name, input) = route("Write the file main.py with content foo").unwrap();
        assert_eq!(name, "Write");
        assert_eq!(input["file_path"], "main.py", "不得截成 n.py");
        assert_eq!(input["content"], "foo", "内容不得丢失");
    }

    #[test]
    fn p0_2_edit_incomplete_never_routed() {
        assert_eq!(
            route("Edit src/lib.rs swapping alpha"),
            None,
            "没有 old/new 对 → 不完整，宁可不路由"
        );
    }

    #[test]
    fn p0_2_quoted_write_routes() {
        let (name, input) = route("Write 'hi' to notes.md").unwrap();
        assert_eq!(name, "Write");
        assert_eq!(input["file_path"], "notes.md");
        assert_eq!(input["content"], "hi");
    }

    #[test]
    fn p0_2_create_file_verb_supported() {
        let (name, input) = route("create file /tmp/x.txt with hello").unwrap();
        assert_eq!(name, "Write");
        assert_eq!(input["file_path"], "/tmp/x.txt");
        assert_eq!(input["content"], "hello");
    }

    #[test]
    fn p0_2_read_without_extension() {
        let (name, input) = route("read README").unwrap();
        assert_eq!(name, "Read");
        assert_eq!(input["file_path"], "README");
    }

    #[test]
    fn p0_2_long_command_not_silently_truncated() {
        let long = format!("run ls {}", "x".repeat(3000));
        let (name, input) = route(&long).unwrap();
        assert_eq!(name, "Bash");
        assert_eq!(
            input["command"].as_str().unwrap().len(),
            3003,
            "300 字符上限已移除"
        );
        let huge = format!("run ls {}", "x".repeat(9000));
        assert_eq!(route(&huge), None, "超 8192 字节 → 不路由");
    }

    #[test]
    fn p0_2_edit_requires_nonempty_old_and_new() {
        let (name, input) = route("Edit src/lib.rs replace alpha with beta").unwrap();
        assert_eq!(name, "Edit");
        assert_eq!(input["file_path"], "src/lib.rs");
        assert_eq!(input["old_string"], "alpha");
        assert_eq!(input["new_string"], "beta");
    }

    // ---- 黄金用例（≥20） ----

    #[test]
    fn golden_read_paths() {
        assert_eq!(
            route("read /tmp/x.txt").unwrap().1["file_path"],
            "/tmp/x.txt"
        );
        assert_eq!(
            route("cat src/main.go").unwrap().1["file_path"],
            "src/main.go"
        );
        assert_eq!(
            route("show me the file /home/me/a.conf").unwrap().1["file_path"],
            "/home/me/a.conf"
        );
        // 散文语境且无路径特征 → 不路由
        assert_eq!(route("show me the plan"), None);
        assert_eq!(route("please print the report"), None);
    }

    #[test]
    fn golden_glob() {
        assert_eq!(route("glob **/*.py").unwrap().1["pattern"], "**/*.py");
        assert_eq!(
            route("find files matching *.go").unwrap().1["pattern"],
            "*.go"
        );
        assert_eq!(
            route("list all files matching src/*.ts").unwrap().1["pattern"],
            "src/*.ts"
        );
        assert_eq!(
            route("find files matching README"),
            None,
            "非 glob 形态不路由"
        );
    }

    #[test]
    fn golden_grep() {
        let (name, input) = route(r#"grep "TODO" in src/"#).unwrap();
        assert_eq!(name, "Grep");
        assert_eq!(input["pattern"], "TODO");
        assert_eq!(input["path"], "src/");
        let (name, input) = route("grep TODO in src").unwrap();
        assert_eq!(name, "Grep");
        assert_eq!(input["pattern"], "TODO");
        assert_eq!(input["path"], "src");
        let (name, input) = route("search for panic in main.go").unwrap();
        assert_eq!(name, "Grep");
        assert_eq!(input["pattern"], "panic");
        assert_eq!(input["path"], "main.go");
        assert_eq!(route("grep the plan"), None, "stopword 不路由");
    }

    #[test]
    fn golden_webfetch_needs_url_and_verb() {
        let (name, input) = route("fetch https://example.com/docs").unwrap();
        assert_eq!(name, "WebFetch");
        assert_eq!(input["url"], "https://example.com/docs");
        assert_eq!(
            route("https://example.com/docs"),
            None,
            "有 URL 无动词不路由"
        );
    }

    #[test]
    fn golden_bash_fence_and_run() {
        let (name, input) = route("run this:\n```sh\nls -la | wc -l\n```").unwrap();
        assert_eq!(name, "Bash");
        assert_eq!(input["command"], "ls -la | wc -l");
        let (name, input) = route("execute the command cargo test --workspace").unwrap();
        assert_eq!(name, "Bash");
        assert_eq!(input["command"], "cargo test --workspace");
        assert_eq!(route("execute "), None);
    }

    #[test]
    fn precedence_read_before_bash_glob_before_grep() {
        // Read 先于 Bash
        assert_eq!(route("read main.py then run make").unwrap().0, "Read");
        // Glob 先于 Grep
        assert_eq!(route("find files matching *.py").unwrap().0, "Glob");
    }

    #[test]
    fn client_tool_filter_respected() {
        assert_eq!(
            route_intent("read README", &["Grep".to_string()]),
            None,
            "客户端没注册 Read → 不路由"
        );
        assert_eq!(
            route_intent("read README", &["Read".to_string()])
                .unwrap()
                .name,
            "Read"
        );
        assert_eq!(route_intent("read README", &[] as &[&str]), None);
    }

    #[test]
    fn from_turns_and_tool_result_skip() {
        let turns = vec![ChatTurn::text(Role::User, "read README")];
        assert_eq!(
            route_from_turns(&turns, &ALL).unwrap().input["file_path"],
            "README"
        );
        // 中间夹了 tool 轮 → 客户端正在执行工具，落回普通聊天
        let mut tool_turn = ChatTurn::text(Role::Tool, "contents");
        tool_turn.tool_call_id = Some("c1".into());
        let turns = vec![
            ChatTurn::text(Role::User, "read README"),
            tool_turn,
            ChatTurn::text(Role::User, "read /tmp/y"),
        ];
        assert_eq!(route_from_turns(&turns, &ALL), None);
        assert!(has_tool_result(&turns));
        assert_eq!(last_user_text(&turns), "");
    }

    #[test]
    fn no_intent_falls_through() {
        assert_eq!(route("你好，介绍一下你自己"), None);
        assert_eq!(route(""), None);
        assert_eq!(route("Write only prose without any target file"), None);
    }

    #[test]
    fn write_missing_content_not_routed() {
        assert_eq!(route("write the file todo.py"), None, "无内容 → 不完整");
        assert_eq!(
            route("write something interesting"),
            None,
            "无路径 → 不完整"
        );
    }

    #[test]
    fn golden_count_at_least_twenty() {
        // 21 条独立话术，确保覆盖 ≥20 黄金用例
        let cases: &[(&str, Option<&str>)] = &[
            ("read README", Some("Read")),
            ("read /tmp/x.txt", Some("Read")),
            ("cat src/main.go", Some("Read")),
            ("show me the plan", None),
            ("glob **/*.py", Some("Glob")),
            ("find files matching *.go", Some("Glob")),
            ("list all files matching src/*.ts", Some("Glob")),
            (r#"grep "TODO" in src/"#, Some("Grep")),
            ("grep TODO in src", Some("Grep")),
            ("search for panic in main.go", Some("Grep")),
            ("grep the plan", None),
            ("Write the file main.py with content foo", Some("Write")),
            ("Write 'hi' to notes.md", Some("Write")),
            ("create file /tmp/x.txt with hello", Some("Write")),
            ("write the file todo.py", None),
            ("Edit src/lib.rs replace alpha with beta", Some("Edit")),
            ("Edit src/lib.rs swapping alpha", None),
            (r#"fetch https://example.com/docs"#, Some("WebFetch")),
            ("https://example.com/docs", None),
            ("execute the command cargo test", Some("Bash")),
            ("execute ", None),
        ];
        for (text, want) in cases {
            assert_eq!(
                route(text).as_ref().map(|(n, _)| n.as_str()),
                *want,
                "case: {text}"
            );
        }
        assert!(cases.len() >= 20);
    }
}
