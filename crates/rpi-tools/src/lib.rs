//! rpi-tools —— 内置工具(05 文档):默认集 read/bash/edit/write,只读集
//! read/grep/find/ls,全量集含 powershell(8 工具)。
//!
//! 只依赖 rpi-agent 的 `Tool` trait(方针文档 §1);装配经 `create_default_tools()`
//! 等工厂,本 crate 整体可拆卸。全部输出型工具经 `OutputLimits` 注入输出上限
//! (默认派生自 agent 转录裁剪上限 - 2k 余量,见 truncate 模块);*_with_limits
//! 工厂由装配层接线,无参工厂使用默认派生值。

mod bash;
mod edit;
mod find;
mod grep;
mod ls;
mod output_accumulator;
mod powershell;
mod read;
mod sanitize;
mod search_ignore;
mod truncate;
mod write;

// T9/T10:shell 工具的会话环境/前缀/钩子工厂经 crate 根出厂(其余工具经
// default_tools/all_tools 注册表工厂装配)
pub use bash::{
    create_bash_tool_with, create_bash_tool_with_limits, create_bash_tool_with_session_env,
    create_powershell_tool_with, BackgroundNotifier, SessionEnvFn, ShellSpawnHook,
    ShellSpawnOptions, ShellTimeoutPolicy,
};
pub use read::create_read_tool_with_limits;
pub use find::{create_find_tool, create_find_tool_with_limits};
pub use grep::{create_grep_tool, create_grep_tool_with_limits};
pub use ls::{create_ls_tool, create_ls_tool_with_limits};
pub use sanitize::{sanitize_control_chars, sanitize_output, strip_ansi};
pub use search_ignore::SearchIgnore;
pub use truncate::{
    truncate_head, truncate_line, truncate_tail, OutputLimits, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES,
    GREP_MAX_LINE_LENGTH,
};

use std::path::Path;
use std::sync::Arc;

use rpi_agent::Tool;

/// 工具注册表:装配期注册,运行期只读(09 A4 状态所有权)。
#[derive(Default)]
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        assert!(
            !self.tools.iter().any(|t| t.name() == tool.name()),
            "duplicate tool name: {}",
            tool.name()
        );
        self.tools.push(tool);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.iter().find(|t| t.name() == name).cloned()
    }

    pub fn all(&self) -> &[Arc<dyn Tool>] {
        &self.tools
    }
}

/// 工厂:默认工具集 read/bash/edit/write(05 文档 §1,pi 的 createCodingTools)。
pub fn create_default_tools() -> ToolRegistry {
    create_tools_at(Path::new("."))
}

/// 工厂:以指定工作目录构造默认工具集(相对路径解析基于 cwd)。
pub fn create_tools_at(cwd: &Path) -> ToolRegistry {
    create_tools_at_with_shell(cwd, bash::ShellSpawnOptions::default())
}

/// 工厂:默认工具集 + shell 装配选项(T9/T10,cli 装配点使用)。
pub fn create_tools_at_with_shell(cwd: &Path, shell: bash::ShellSpawnOptions) -> ToolRegistry {
    create_tools_at_with_shell_and_limits(cwd, shell, OutputLimits::default())
}

/// 工厂:默认工具集 + shell 装配选项 + 输出上限注入(装配层统一派生值)。
pub fn create_tools_at_with_shell_and_limits(
    cwd: &Path,
    shell: bash::ShellSpawnOptions,
    limits: OutputLimits,
) -> ToolRegistry {
    let mut registry = ToolRegistry::default();
    for tool in default_tools_with_shell_and_limits(cwd, shell, limits) {
        registry.register(tool);
    }
    registry
}

/// 默认工具列表(read/bash/edit/write,pi 的 createCodingToolDefinitions)。
pub fn default_tools(cwd: &Path) -> Vec<Arc<dyn Tool>> {
    vec![
        read::create_read_tool(cwd),
        bash::create_bash_tool(cwd),
        edit::create_edit_tool(cwd),
        write::create_write_tool(cwd),
    ]
}

/// 默认工具列表 + shell 装配选项 + 输出上限注入(装配层统一派生值)。
pub fn default_tools_with_shell_and_limits(
    cwd: &Path,
    shell: bash::ShellSpawnOptions,
    limits: OutputLimits,
) -> Vec<Arc<dyn Tool>> {
    vec![
        read::create_read_tool_with_limits(cwd, limits),
        bash::create_bash_tool_with_limits(cwd, shell, limits),
        edit::create_edit_tool(cwd),
        write::create_write_tool(cwd),
    ]
}

/// 只读工具列表(read/grep/find/ls,pi 的 createReadOnlyTools)。
pub fn read_only_tools(cwd: &Path) -> Vec<Arc<dyn Tool>> {
    read_only_tools_with_limits(cwd, OutputLimits::default(), Arc::new(SearchIgnore::builtin()))
}

/// 只读工具列表 + 输出上限注入 + 检索忽略列表(装配层统一派生值)。
pub fn read_only_tools_with_limits(
    cwd: &Path,
    limits: OutputLimits,
    ignore: Arc<SearchIgnore>,
) -> Vec<Arc<dyn Tool>> {
    vec![
        read::create_read_tool_with_limits(cwd, limits),
        grep::create_grep_tool_with_limits(cwd, limits, ignore.clone()),
        find::create_find_tool_with_limits(cwd, limits, ignore.clone()),
        ls::create_ls_tool_with_limits(cwd, limits, ignore),
    ]
}

/// 全量工具列表(8 工具,pi 的 createAllTools)+ 输出上限注入 + 检索忽略列表
/// (装配层统一派生值)。
pub fn all_tools(cwd: &Path, ignore: Arc<SearchIgnore>) -> Vec<Arc<dyn Tool>> {
    let limits = OutputLimits::default();
    vec![
        read::create_read_tool_with_limits(cwd, limits),
        bash::create_bash_tool(cwd),
        powershell::create_powershell_tool(cwd),
        edit::create_edit_tool(cwd),
        write::create_write_tool(cwd),
        grep::create_grep_tool_with_limits(cwd, limits, ignore.clone()),
        find::create_find_tool_with_limits(cwd, limits, ignore.clone()),
        ls::create_ls_tool_with_limits(cwd, limits, ignore),
    ]
}

/// 工厂:以指定工作目录构造只读工具集。
pub fn create_read_only_tools_at(cwd: &Path) -> ToolRegistry {
    let mut registry = ToolRegistry::default();
    for tool in read_only_tools(cwd) {
        registry.register(tool);
    }
    registry
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_registry_has_four_tools() {
        let registry = create_default_tools();
        for name in ["read", "bash", "edit", "write"] {
            assert!(registry.get(name).is_some(), "missing tool: {name}");
        }
        assert!(registry.get("missing").is_none());
    }

    #[test]
    fn read_only_registry_has_four_tools() {
        let registry = create_read_only_tools_at(Path::new("."));
        for name in ["read", "grep", "find", "ls"] {
            assert!(registry.get(name).is_some(), "missing tool: {name}");
        }
        assert!(registry.get("bash").is_none());
    }

    #[test]
    fn all_tools_registry_has_eight_tools() {
        let mut registry = ToolRegistry::default();
        for tool in all_tools(Path::new("."), Arc::new(SearchIgnore::builtin())) {
            registry.register(tool);
        }
        for name in [
            "read",
            "bash",
            "powershell",
            "edit",
            "write",
            "grep",
            "find",
            "ls",
        ] {
            assert!(registry.get(name).is_some(), "missing tool: {name}");
        }
    }

    #[test]
    #[should_panic(expected = "duplicate tool name")]
    fn duplicate_name_panics() {
        let mut registry = create_default_tools();
        let duplicate = read::create_read_tool(Path::new("."));
        registry.register(duplicate);
    }
}

#[cfg(test)]
mod shell_validation_tests {
    use super::*;

    /// T7 坑位(reviewer P2):全部 8 个内置工具的**真实 schema** 必须能过
    /// jsonschema 编译——错误信息以 "invalid tool schema" 开头即 compile 失败。
    #[test]
    fn all_builtin_tool_schemas_pass_jsonschema() {
        for tool in all_tools(Path::new("."), Arc::new(SearchIgnore::builtin())) {
            let schema = tool.schema();
            let empty = serde_json::json!({});
            let outcome = rpi_agent::validate_arguments(&schema, &empty);
            if let Err(message) = outcome {
                assert!(
                    !message.starts_with("invalid tool schema"),
                    "工具 `{}` 的 schema 无法编译: {message}",
                    tool.name()
                );
            }
        }
    }

    /// 合法参数经真实 schema 校验通过(bash/read 各一)。
    #[test]
    fn valid_arguments_pass_real_tool_schemas() {
        let registry = create_tools_at(Path::new("."));
        let bash = registry.get("bash").unwrap();
        assert!(rpi_agent::validate_arguments(
            &bash.schema(),
            &serde_json::json!({"command": "ls"})
        )
        .is_ok());
        let read = registry.get("read").unwrap();
        assert!(rpi_agent::validate_arguments(
            &read.schema(),
            &serde_json::json!({"path": "a.txt", "offset": 1})
        )
        .is_ok());
    }
}

