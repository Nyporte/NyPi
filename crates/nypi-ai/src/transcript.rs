//! Transcript-carried system prompt and tools (decision D40), ported from Pi's
//! `packages/ai/src/utils/transcript.ts`. The leading system message holds the prompt and tools;
//! later ones patch sections and change the tool set, so a change appends to the transcript
//! instead of rewriting its start, and the provider's cached prefix survives. Providers that
//! accept system messages mid-conversation send each update in place; the others, and models
//! not verified to accept them, get the transcript collapsed into one leading message, which is
//! exactly the prompt and tools a rebuild would give.

use std::collections::BTreeMap;

use crate::types::{Context, Message, Sections, SystemMessage, ToolDefinition, ToolReference};

fn systems(messages: &[Message]) -> impl Iterator<Item = &SystemMessage> {
    messages.iter().filter_map(|m| match m {
        Message::System(system) => Some(system),
        _ => None,
    })
}

/// Whether the transcript starts with a system message: it carries its own prompt and tools.
pub fn has_leading_system(messages: &[Message]) -> bool {
    matches!(messages.first(), Some(Message::System(_)))
}

/// The tools available after applying every change in order.
pub fn current_tools(messages: &[Message]) -> Vec<ToolDefinition> {
    let mut order: Vec<String> = Vec::new();
    let mut tools: BTreeMap<String, ToolDefinition> = BTreeMap::new();
    for system in systems(messages) {
        for removed in &system.tools_removed {
            tools.remove(&removed.name);
            order.retain(|n| *n != removed.name);
        }
        for tool in &system.tools_added {
            if tools.insert(tool.name.clone(), tool.clone()).is_none() {
                order.push(tool.name.clone());
            }
        }
    }
    order.iter().filter_map(|n| tools.get(n).cloned()).collect()
}

/// Every system message replayed into one: the base prompt with later content appended,
/// sections patched by name, and the current tools.
pub fn current_system(messages: &[Message]) -> Option<SystemMessage> {
    let mut content: Vec<String> = Vec::new();
    let mut sections = Sections::default();
    let mut timestamp = None;
    for system in systems(messages) {
        timestamp.get_or_insert(system.timestamp);
        if !system.content.is_empty() {
            content.push(system.content.clone());
        }
        for (name, value) in &system.sections.0 {
            match value {
                Some(text) => sections.set(name, Some(text.clone())),
                None => sections.0.retain(|(n, _)| n != name),
            }
        }
    }
    let tools = current_tools(messages);
    if timestamp.is_none() && tools.is_empty() {
        return None;
    }
    Some(SystemMessage {
        content: content.join("\n\n"),
        sections,
        tools_added: tools,
        tools_removed: Vec::new(),
        timestamp: timestamp.unwrap_or(0),
    })
}

/// A system message as a complete prompt: its content, then its sections.
pub fn render(system: &SystemMessage) -> String {
    std::iter::once(system.content.as_str())
        .chain(system.sections.0.iter().filter_map(|(_, v)| v.as_deref()))
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// A later system message for APIs that accept it mid-conversation: changed sections framed
/// by name, so the model can relate them to the leading prompt.
pub fn render_update(system: &SystemMessage) -> String {
    let mut parts = Vec::new();
    if !system.content.is_empty() {
        parts.push(system.content.clone());
    }
    for (name, value) in &system.sections.0 {
        parts.push(match value {
            Some(text) => format!("Updated system prompt section \"{name}\":\n\n{text}"),
            None => format!("Removed system prompt section \"{name}\"."),
        });
    }
    parts.join("\n\n")
}

/// The changes from one complete tool state to another; a changed definition is a removal
/// followed by an addition.
pub fn tool_changes(
    previous: &[ToolDefinition],
    current: &[ToolDefinition],
) -> (Vec<ToolDefinition>, Vec<ToolReference>) {
    let added = current.iter().filter(|t| !previous.contains(t)).cloned().collect();
    let removed =
        previous.iter().filter(|t| !current.contains(t)).map(|t| ToolReference { name: t.name.clone() }).collect();
    (added, removed)
}

/// Sections that differ between what the transcript holds and what the prompt should be now:
/// changed or new ones with their text, dropped ones as `None`. Empty when nothing changed.
pub fn section_changes(held: &Sections, wanted: &[(String, String)]) -> Sections {
    let mut changes = Sections::default();
    for (name, text) in wanted {
        let same = held.0.iter().any(|(n, v)| n == name && v.as_deref() == Some(text.as_str()));
        if !same {
            changes.0.push((name.clone(), Some(text.clone())));
        }
    }
    for (name, _) in &held.0 {
        if !wanted.iter().any(|(n, _)| n == name) {
            changes.0.push((name.clone(), None));
        }
    }
    changes
}

/// The request as a provider sends it. With `in_place`, later system messages stay where they
/// are (the provider renders them with `render_update`); otherwise they are folded into the
/// leading prompt. Tools are always the current set in the top-level field: no transport NyPi
/// speaks is verified to declare tool changes in place. A transcript without a leading system
/// message takes `system_prompt` and `tools` from the context, as before.
pub fn prepare(ctx: &Context, in_place: bool) -> Context {
    if !has_leading_system(&ctx.messages) {
        return Context {
            system_prompt: ctx.system_prompt.clone(),
            messages: ctx.messages.iter().filter(|m| !matches!(m, Message::System(_))).cloned().collect(),
            tools: ctx.tools.clone(),
        };
    }
    let tools = current_tools(&ctx.messages);
    if in_place {
        let Some(Message::System(head)) = ctx.messages.first() else { unreachable!() };
        return Context { system_prompt: render(head), messages: ctx.messages[1..].to_vec(), tools };
    }
    let head = current_system(&ctx.messages).map(|s| render(&s)).unwrap_or_default();
    Context {
        system_prompt: head,
        messages: ctx.messages.iter().filter(|m| !matches!(m, Message::System(_))).cloned().collect(),
        tools,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(name: &str, description: &str) -> ToolDefinition {
        ToolDefinition { name: name.into(), description: description.into(), parameters: json!({}) }
    }

    fn system(
        content: &str,
        sections: &[(&str, Option<&str>)],
        added: Vec<ToolDefinition>,
        removed: &[&str],
    ) -> Message {
        Message::System(SystemMessage {
            content: content.into(),
            sections: Sections(sections.iter().map(|(n, v)| (n.to_string(), v.map(str::to_string))).collect()),
            tools_added: added,
            tools_removed: removed.iter().map(|n| ToolReference { name: n.to_string() }).collect(),
            timestamp: 1,
        })
    }

    #[test]
    fn replays_sections_and_tools_and_collapses_or_keeps_updates() {
        let messages = vec![
            system(
                "core",
                &[("goals", Some("<goals>a</goals>")), ("env", Some("<env>x</env>"))],
                vec![tool("read", "r"), tool("bash", "b")],
                &[],
            ),
            Message::user_text("hi"),
            system(
                "",
                &[("goals", Some("<goals>b</goals>")), ("portrait", Some("<portrait>p</portrait>"))],
                vec![tool("bash", "b2"), tool("grep", "g")],
                &["bash"],
            ),
            Message::user_text("again"),
            system("", &[("env", None)], vec![], &["read"]),
        ];
        let current = current_system(&messages).unwrap();
        assert_eq!(
            render(&current),
            "core\n\n<goals>b</goals>\n\n<portrait>p</portrait>",
            "patched in place, new ones last"
        );
        let names: Vec<&str> = current.tools_added.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["bash", "grep"]);
        assert_eq!(current.tools_added[0].description, "b2", "a redefinition replaces the old one");

        let collapsed =
            prepare(&Context { system_prompt: "ignored".into(), messages: messages.clone(), tools: vec![] }, false);
        assert_eq!(collapsed.system_prompt, render(&current));
        assert!(collapsed.messages.iter().all(|m| !matches!(m, Message::System(_))));
        let kept = prepare(&Context { system_prompt: String::new(), messages: messages.clone(), tools: vec![] }, true);
        assert_eq!(kept.system_prompt, "core\n\n<goals>a</goals>\n\n<env>x</env>", "the cached head stays as it was");
        assert_eq!(kept.messages.len(), 4);
        let Message::System(update) = &kept.messages[1] else { panic!() };
        assert!(render_update(update).starts_with("Updated system prompt section \"goals\":\n\n<goals>b</goals>"));
        let Message::System(last) = &kept.messages[3] else { panic!() };
        assert_eq!(render_update(last), "Removed system prompt section \"env\".");

        let legacy = prepare(
            &Context {
                system_prompt: "old".into(),
                messages: vec![Message::user_text("x")],
                tools: vec![tool("ls", "l")],
            },
            true,
        );
        assert_eq!((legacy.system_prompt.as_str(), legacy.tools.len()), ("old", 1), "old sessions work as before");
    }

    #[test]
    fn diffs_sections_and_tools_and_keeps_pi_json() {
        let held = Sections(vec![("goals".into(), Some("a".into())), ("env".into(), Some("x".into()))]);
        let changes = section_changes(&held, &[("goals".into(), "a".into()), ("portrait".into(), "p".into())]);
        assert_eq!(changes.0, [("portrait".to_string(), Some("p".to_string())), ("env".to_string(), None)]);
        assert!(section_changes(&held, &[("goals".into(), "a".into()), ("env".into(), "x".into())]).is_empty());
        let (added, removed) =
            tool_changes(&[tool("read", "r"), tool("bash", "b")], &[tool("read", "r"), tool("bash", "b2")]);
        assert_eq!((added.len(), removed[0].name.as_str()), (1, "bash"));

        let message = system("", &[("goals", Some("g")), ("env", None)], vec![], &["ls"]);
        let json = serde_json::to_value(&message).unwrap();
        assert_eq!(
            json,
            json!({ "role": "system", "content": "", "sections": { "goals": "g", "env": null }, "toolsRemoved": [{ "name": "ls" }], "timestamp": 1 })
        );
        let back: Message = serde_json::from_value(
            json!({ "role": "system", "content": [{ "type": "text", "text": "hi" }], "timestamp": 2 }),
        )
        .unwrap();
        assert!(matches!(back, Message::System(s) if s.content == "hi"));
    }
}
