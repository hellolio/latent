//! `load_skill` 工具:skill 信息的唯一出口 —— `description()` 动态渲染可用
//! skill 清单(工具声明随每轮请求发给模型,主会话与勾选本工具的子 agent
//! 一视同仁),`execute` 按名读取 SKILL.md 全文,作为 tool result 进入上下文。

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use latent_agent::{Tool, ToolCall, ToolError, ToolOutput};

use super::defs::SkillDef;

pub const TOOL_NAME: &str = "load_skill";

pub struct LoadSkillDeps {
    pub skills: Vec<SkillDef>,
}

pub struct LoadSkillTool {
    skills: Vec<SkillDef>,
    /// 构造期从 deps 拼定(Tool::description 返回 &str)
    description: String,
}

impl LoadSkillTool {
    pub fn new(deps: LoadSkillDeps) -> Self {
        let description = Self::build_description(&deps.skills);
        LoadSkillTool {
            skills: deps.skills,
            description,
        }
    }

    fn build_description(skills: &[SkillDef]) -> String {
        let mut text = String::from(
            "Load a skill's full SKILL.md instructions by name. Call it before doing work \
             covered by one of the available skills, then follow the loaded instructions.",
        );
        if skills.is_empty() {
            text.push_str("\nNo skills are currently available.");
        } else {
            text.push_str("\nAvailable skills:");
            for skill in skills {
                text.push_str(&format!("\n- {}: {}", skill.name, skill.description));
            }
        }
        text
    }

    fn fail(message: String) -> ToolError {
        ToolError::Failed {
            name: TOOL_NAME.into(),
            message,
        }
    }

    fn available_names(&self) -> String {
        self.skills
            .iter()
            .map(|skill| skill.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[async_trait]
impl Tool for LoadSkillTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "skill": {"type": "string", "description": "Name of the skill to load (see the list in this tool's description)"}
            },
            "required": ["skill"],
            "additionalProperties": false
        })
    }

    fn prompt_snippet(&self) -> Option<String> {
        Some(
            "load_skill(...): load a skill's full SKILL.md instructions by name before \
             doing skill-covered work"
                .into(),
        )
    }

    async fn execute(
        &self,
        call: ToolCall,
        _cancel: CancellationToken,
        _updater: &dyn latent_agent::ToolUpdater,
    ) -> Result<ToolOutput, ToolError> {
        let Some(name) = call.args.get("skill").and_then(|value| value.as_str()) else {
            return Err(Self::fail("`skill` (string) is required".into()));
        };
        let Some(skill) = self.skills.iter().find(|skill| skill.name == name) else {
            return Err(Self::fail(format!(
                "unknown skill `{name}`; available: {}",
                self.available_names()
            )));
        };
        // 调用时读取而非启动缓存:会话中途修改 SKILL.md 即时生效
        let content = std::fs::read_to_string(&skill.path).map_err(|error| {
            Self::fail(format!("failed to read {}: {error}", skill.path.display()))
        })?;
        Ok(ToolOutput::text(content))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct NopUpdater;

    #[async_trait::async_trait]
    impl latent_agent::ToolUpdater for NopUpdater {
        async fn update(&self, _partial: String) {}
    }

    fn skill(name: &str, description: &str) -> SkillDef {
        SkillDef {
            name: name.into(),
            description: description.into(),
            path: PathBuf::from(format!("/tmp/latent-load-skill-test/{name}/SKILL.md")),
        }
    }

    fn tool() -> LoadSkillTool {
        LoadSkillTool::new(LoadSkillDeps {
            skills: vec![skill("review", "Reviews code"), skill("deploy", "Deploys stuff")],
        })
    }

    fn call(args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "call-1".into(),
            name: TOOL_NAME.into(),
            args,
        }
    }

    #[tokio::test]
    async fn loads_skill_file_content() {
        let dir = std::env::temp_dir().join(format!("latent-load-skill-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("review")).unwrap();
        std::fs::write(dir.join("review").join("SKILL.md"), "# Review\nFollow these steps.").unwrap();

        let mut t = tool();
        t.skills[0].path = dir.join("review").join("SKILL.md");

        let output = t
            .execute(
                call(serde_json::json!({"skill": "review"})),
                CancellationToken::new(),
                &NopUpdater,
            )
            .await
            .unwrap();
        assert_eq!(output.output, "# Review\nFollow these steps.");
        assert!(!output.terminate);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn unknown_skill_lists_available() {
        let error = tool()
            .execute(
                call(serde_json::json!({"skill": "nope"})),
                CancellationToken::new(),
                &NopUpdater,
            )
            .await
            .unwrap_err();
        let ToolError::Failed { message, .. } = error else {
            panic!("{error:?}");
        };
        assert!(message.contains("unknown skill `nope`"), "{message}");
        assert!(message.contains("review, deploy"), "{message}");
    }

    #[tokio::test]
    async fn missing_argument_is_error() {
        let error = tool()
            .execute(
                call(serde_json::json!({})),
                CancellationToken::new(),
                &NopUpdater,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("`skill` (string) is required"));
    }

    #[test]
    fn description_renders_catalog() {
        let t = tool();
        let description = t.description();
        assert!(description.contains("Load a skill's full SKILL.md instructions"), "{description}");
        assert!(description.contains("- review: Reviews code"), "{description}");
        assert!(description.contains("- deploy: Deploys stuff"), "{description}");
    }

    #[test]
    fn description_without_skills_says_none() {
        let t = LoadSkillTool::new(LoadSkillDeps { skills: vec![] });
        let description = t.description();
        assert!(description.contains("No skills are currently available"), "{description}");
    }
}
