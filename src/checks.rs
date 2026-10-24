//! 一轮快结束时的检查（照 Friday 的 DeliveryCheck、TurnCheck；规则在 friday-core 的 delivery）：
//! - 写完没验证：这一轮写了代码、配置，之后却没有运行或读回确认 → 追一条提醒，让它先验证再汇报；
//! - 交付检查：用户交代的是要做的事，交回来的却是方案、建议、「要不要我…」（又没真正动手），或者只说了一句下一步就停了，
//!   或者发现了问题却把修不修交回给用户 → 追一条提醒，让它对照用户的原话分析、该做的做完再交付；
//! - 子智能体交回之前同样查一遍（没写结果、只说下一步、只交方案、写完没验证 → 原地打回），交给上级的结果后面附验收提示。
//!
//! 每种提醒一轮里最多一次。判断只是便宜的起疑，最后由智能体自己对照原话决定。

use std::collections::{HashMap, HashSet};

use friday_core::chat_models::{BlockKind, MessageBlock, ToolEvent};
use friday_core::delivery::{self, Permissions};
use serde_json::{Map, Value};

use crate::brain::Event;

/// 一轮里发生了什么（文字、工具调用），攒成 friday-core 认的消息块
#[derive(Default)]
pub struct Recorder {
    pub blocks: Vec<MessageBlock>,
    calls: HashMap<String, usize>,
    /// 这一段从哪个块开始（打回接着做时，新的文字不接在上一次的块后面）
    boundary: usize,
}

impl Recorder {
    /// 开始新的一段，返回它从第几个块开始
    pub fn mark(&mut self) -> usize {
        self.boundary = self.blocks.len();
        self.boundary
    }

    pub fn on(&mut self, event: &Event) {
        let fresh = self.blocks.len() <= self.boundary;
        match event {
            Event::Text(text) => match self.blocks.last_mut() {
                Some(MessageBlock { kind: BlockKind::Text(existing), .. }) if !fresh => existing.push_str(text),
                _ => self.blocks.push(block(BlockKind::Text(text.clone()))),
            },
            Event::Tool { id, name, arguments } => {
                self.calls.insert(id.clone(), self.blocks.len());
                self.blocks.push(block(BlockKind::Tool(Box::new(ToolEvent {
                    id: id.clone(),
                    name: name.clone(),
                    arguments: arguments.to_string(),
                    status: "running".into(),
                    result: String::new(),
                    transcript: None,
                    queued: None,
                    extra: Map::new(),
                }))));
            }
            Event::Result { id, ok, text, .. } => {
                if let Some(&index) = self.calls.get(id)
                    && let BlockKind::Tool(call) = &mut self.blocks[index].kind
                {
                    call.status = if *ok { "succeeded" } else { "failed" }.into();
                    call.result = text.clone();
                }
            }
            _ => {}
        }
    }

    /// 从这里开始的块（打回之后只看新的这一段）
    pub fn since(&self, start: usize) -> &[MessageBlock] {
        &self.blocks[start.min(self.blocks.len())..]
    }
}

fn block(kind: BlockKind) -> MessageBlock {
    MessageBlock { id: uuid::Uuid::new_v4().to_string(), kind, extra: Map::new() }
}

/// 工具的权限（readOnly / writeFiles / execute / network）：自带的、能力的动作、MCP 的、派子智能体的
pub fn permissions(blocks: &[MessageBlock]) -> Permissions {
    let mut map = Permissions::new();
    for name in ["get_current_time", "list_directory", "read_file", "find_files", "search_files", "search_keywords", "read_agent_log"] {
        map.insert(name.into(), "readOnly".into());
    }
    for name in ["write_file", "edit_file"] {
        map.insert(name.into(), "writeFiles".into());
    }
    for name in ["run_shell", "delegate_task", "run_parallel_tasks", "run_workflow", "rework_agent"] {
        map.insert(name.into(), "execute".into());
    }
    for name in ["web_search", "fetch_url"] {
        map.insert(name.into(), "network".into());
    }
    for block in blocks {
        if let BlockKind::Tool(call) = &block.kind
            && !map.contains_key(&call.name)
        {
            let read_only = crate::capabilities::is_read_only(&call.name) || crate::mcp::is_read_only(&call.name);
            map.insert(call.name.clone(), if read_only { "readOnly" } else { "execute" }.into());
        }
    }
    map
}

/// 哪一种提醒
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Unverified,
    Plan,
    Announce,
    HandBack,
    Empty,
}

/// 一条要追的提醒：给人看的一句、给模型的话
pub struct Followup {
    pub kind: Kind,
    pub notice: String,
    pub prompt: String,
}

/// 主智能体一轮结束时：要不要追一条提醒。request：用户的原话；delegates：能派子智能体
pub fn main_followup(request: &str, blocks: &[MessageBlock], delegates: bool, done: &HashSet<Kind>) -> Option<Followup> {
    let permissions = permissions(blocks);
    // 写完没验证（用户说了不用运行 / 测试的不催）
    let files = delivery::unverified_writes(blocks, &permissions, cfg!(windows));
    if !files.is_empty() && !done.contains(&Kind::Unverified) && !delivery::user_declined_verification(request) {
        return Some(Followup {
            kind: Kind::Unverified,
            notice: format!("写了 {} 之后没有运行或读回确认，已提醒先验证", files.join("、")),
            prompt: delivery::turn_prompt(&files, delegates, None),
        });
    }
    // 交付检查：只管交代了要做的事的
    if !delivery::looks_like_task(request) {
        return None;
    }
    let reply = delivery::final_reply(blocks);
    if !delivery::asks_for_plan_explicitly(request) && delivery::hands_back_fix(&reply) && !done.contains(&Kind::HandBack) {
        return Some(Followup {
            kind: Kind::HandBack,
            notice: "发现了问题却问你要不要修，已要求先分析是不是自己做出来的、该修就修完".into(),
            prompt: delivery::fix_prompt(delegates, None),
        });
    }
    let worked = delivery::did_work(blocks, &permissions);
    if !delivery::asks_for_plan(request) && !worked && delivery::looks_like_plan(&reply) && !done.contains(&Kind::Plan) {
        return Some(Followup {
            kind: Kind::Plan,
            notice: "交给你的像是方案而不是结果，已要求直接做完".into(),
            prompt: delivery::main_prompt("", "", None),
        });
    }
    // 只说了一句下一步就停了（这一轮派了子智能体的不算：「交给子 Agent 去做，做完汇总」是正常的收尾）
    let delegated =
        blocks.iter().any(|block| matches!(&block.kind, BlockKind::Tool(call) if delivery::TEAM_TOOLS.contains(&call.name.as_str())));
    if delivery::looks_like_announcement(&reply) && !delegated && !done.contains(&Kind::Announce) {
        return Some(Followup {
            kind: Kind::Announce,
            notice: "只说了下一步就结束了，已要求接着做".into(),
            prompt: delivery::announce_prompt(&reply, None),
        });
    }
    None
}

/// 子智能体交回之前：要不要原地打回。task：派给它的任务
pub fn child_followup(task: &str, blocks: &[MessageBlock], done: &HashSet<Kind>) -> Option<Followup> {
    let permissions = permissions(blocks);
    let reply = delivery::final_reply(blocks);
    let worked = delivery::did_work(blocks, &permissions);
    let analysis = delivery::task_wants_analysis(task);
    let fresh = |kind: Kind| !done.contains(&kind);
    if reply.trim().is_empty() && fresh(Kind::Empty) {
        return Some(Followup {
            kind: Kind::Empty,
            notice: "没写结果，已原地打回".into(),
            prompt: friday_core::prompts::text("delivery.empty", &Default::default(), None),
        });
    }
    if delivery::looks_like_announcement(&reply) && (analysis || worked || !delivery::looks_like_plan(&reply)) && fresh(Kind::Announce) {
        return Some(Followup {
            kind: Kind::Announce,
            notice: "只说了下一步就结束，已原地打回".into(),
            prompt: delivery::announce_prompt(&reply, None),
        });
    }
    if !analysis && !worked && delivery::looks_like_plan(&reply) && fresh(Kind::Plan) {
        return Some(Followup {
            kind: Kind::Plan,
            notice: "只交了方案，已原地打回".into(),
            prompt: friday_core::prompts::text("delivery.sub-plan", &Default::default(), None),
        });
    }
    let files = delivery::unverified_writes(blocks, &permissions, cfg!(windows));
    if !files.is_empty() && !delivery::user_declined_verification(task) && fresh(Kind::Unverified) {
        return Some(Followup {
            kind: Kind::Unverified,
            notice: "写完没验证，已原地打回".into(),
            prompt: delivery::turn_prompt(&files, false, None),
        });
    }
    None
}

/// 子智能体交给上级的结果后面附的验收提示（仍像方案、写了没验证、「未完成与风险」不是无）
pub fn review_hint(task: &str, result: &str, blocks: &[MessageBlock]) -> Option<String> {
    let permissions = permissions(blocks);
    let worked = delivery::did_work(blocks, &permissions);
    let still_plan = !delivery::task_wants_analysis(task) && !worked && delivery::looks_like_plan(result);
    let unverified = if delivery::user_declined_verification(task) {
        Vec::new()
    } else {
        delivery::unverified_writes(blocks, &permissions, cfg!(windows))
    };
    delivery::review_hint(result, &unverified, still_plan, "", "", None)
}

/// 一段运行记录给模型看（read_agent_log）
pub fn log_text(name: &str, status: &str, blocks: &[MessageBlock]) -> String {
    let mut extra = Map::new();
    extra.insert("agentName".into(), Value::from(name));
    let event = ToolEvent {
        id: String::new(),
        name: "agent".into(),
        arguments: String::new(),
        status: status.into(),
        result: String::new(),
        transcript: Some(blocks.to_vec()),
        queued: None,
        extra,
    };
    delivery::log_text(&event)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn record(events: &[Event]) -> Recorder {
        let mut recorder = Recorder::default();
        for event in events {
            recorder.on(event);
        }
        recorder
    }

    #[test]
    fn spots_unverified_writes_and_plans() {
        crate::brain::setup_prompts();
        let wrote = record(&[
            Event::Tool { id: "1".into(), name: "write_file".into(), arguments: json!({ "path": "a.py", "content": "x" }) },
            Event::Result { id: "1".into(), name: "write_file".into(), ok: true, text: "写好了".into() },
            Event::Text("写好了 a.py".into()),
        ]);
        let followup = main_followup("帮我写个脚本", &wrote.blocks, true, &HashSet::new()).unwrap();
        assert_eq!(followup.kind, Kind::Unverified);
        assert!(followup.prompt.contains("a.py"), "{}", followup.prompt);
        // 用户说了不用运行
        assert!(main_followup("写个脚本，不用运行", &wrote.blocks, true, &HashSet::new()).is_none());
        let planned = record(&[Event::Text("建议你先装好依赖，然后你可以运行 make。要不要我帮你做？".into())]);
        assert_eq!(main_followup("帮我把项目编译好", &planned.blocks, true, &HashSet::new()).unwrap().kind, Kind::Plan);
        // 要的就是方案：不催
        assert!(main_followup("给我一个编译的方案", &planned.blocks, true, &HashSet::new()).is_none());
        // 闲聊不催
        assert!(main_followup("你好", &planned.blocks, true, &HashSet::new()).is_none());
        let empty = record(&[]);
        assert_eq!(child_followup("改好 a.py", &empty.blocks, &HashSet::new()).unwrap().kind, Kind::Empty);
    }
}
