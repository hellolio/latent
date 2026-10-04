//! Skill 机制(pi skills.ts 移植):skill 是数据目录
//! (`<cwd>/.latent/skills/<name>/SKILL.md` 项目优先 → `~/.latent/skills/`)。
//!
//! skill 摘要的唯一出口是 `load_skill` 工具声明 —— `description()` 动态渲染
//! 可用清单,随每轮请求发给模型(主会话与勾选本工具的子 agent 一视同仁);
//! `execute` 按名读取 SKILL.md 全文,作为 tool result 进入当前对话上下文。
//! 不注入系统提示词节:子 agent 的系统提示词被定义 md 正文整体替换,
//! 节内容到不了子会话,工具描述才是两处通用的通道。

pub mod defs;
pub mod tool;

pub use defs::{discover_skill_defs, parse_skill_def, SkillDef};
pub use tool::{LoadSkillDeps, LoadSkillTool};
