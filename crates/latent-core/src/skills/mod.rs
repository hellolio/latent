//! Skill 机制(pi skills.ts 移植):skill 是数据目录
//! (`<cwd>/.latent/skills/<name>/SKILL.md` 项目优先 → 用户数据目录
//! `skills/`,见 `crate::paths`)。
//!
//! skill 摘要的唯一出口是 `load_skill` 工具声明 —— `description()` 动态渲染
//! 可用清单,随每轮请求发给模型(主会话与勾选本工具的子 agent 一视同仁);
//! `execute` 按名读取 SKILL.md 全文,包成 `<skill name="…">` 形态返回。
//! 不注入系统提示词节:子 agent 的系统提示词被定义 md 正文整体替换,
//! 节内容到不了子会话,工具描述才是两处通用的通道。
//!
//! 模型上下文里技能正文一律以 **developer 角色**出现(`expand` 模块):
//! load_skill 工具结果换短确认 + 正文进紧随的 developer 消息;输入框伪命令
//! `/skill <名称>`(与工具调用无关)在 user 消息前注入 developer 消息。

pub mod defs;
pub mod expand;
pub mod tool;

pub use defs::{discover_skill_defs, parse_skill_def, SkillDef};
pub use expand::expand_skill_references;
pub use tool::{LoadSkillDeps, LoadSkillTool, TOOL_NAME};
