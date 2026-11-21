//! A runtime's MCP servers: connecting them, keeping their tool lists current, and their prompts.

use super::*;

impl Runtime {
    /// Start configured MCP servers and add their tools. Returns one message per failed server
    /// (also raised as alerts), so a broken server never prevents the agent from starting.
    pub async fn connect_mcp(&mut self) -> Vec<String> {
        if self.settings.mcp_servers.is_empty() {
            return Vec::new();
        }
        let connections = crate::mcp::connect_all(&self.settings.mcp_servers).await;
        for (client, tools) in connections.servers {
            match client.list_prompts().await {
                Ok(prompts) => self.mcp_prompts.extend(prompts),
                Err(error) => self.inspector.alert(Alert::new(
                    AlertLevel::Warning,
                    format!("MCP server {}: cannot list prompts: {error:#}", client.name),
                )),
            }
            let names: Vec<String> = tools.iter().map(|t| t.definition().name).collect();
            self.inspector.record("harness", json!({ "mcpServer": client.name, "tools": names }));
            self.tool_names.extend(names.iter().cloned());
            self.agent.config.tools.extend(tools);
            self.mcp.push((client, names));
        }
        for error in &connections.errors {
            self.inspector.alert(Alert::new(AlertLevel::Warning, error.clone()));
        }
        self.refresh_system_prompt();
        connections.errors
    }

    /// Re-list the tools of servers that announced a change. The new list applies from the next
    /// model request; a failed re-list keeps the old tools and alerts.
    pub(super) async fn refresh_mcp_tools(&mut self, sink: &RuntimeSink) {
        for index in 0..self.mcp.len() {
            let client = self.mcp[index].0.clone();
            if client.take_prompts_changed()
                && let Ok(prompts) = client.list_prompts().await
            {
                self.mcp_prompts.retain(|p| p.server != client.name);
                self.mcp_prompts.extend(prompts);
            }
            if !client.take_tools_changed() {
                continue;
            }
            match crate::mcp::server_tools(&client).await {
                Ok(tools) => {
                    let old = std::mem::take(&mut self.mcp[index].1);
                    self.agent.config.tools.retain(|t| !old.contains(&t.definition().name));
                    self.tool_names.retain(|n| !old.contains(n));
                    let names: Vec<String> = tools.iter().map(|t| t.definition().name).collect();
                    self.inspector.record("harness", json!({ "mcpServer": client.name, "toolsChanged": names }));
                    self.tool_names.extend(names.iter().cloned());
                    self.agent.config.tools.extend(tools);
                    self.mcp[index].1 = names;
                }
                Err(error) => self.raise(
                    sink,
                    Alert::new(
                        AlertLevel::Warning,
                        format!("MCP server {}: cannot refresh tools: {error:#}", client.name),
                    ),
                ),
            }
        }
    }

    /// Prompt templates offered by connected MCP servers.
    pub fn mcp_prompts(&self) -> &[McpPrompt] {
        &self.mcp_prompts
    }

    /// Render `/mcp:<server>:<name> <raw>` into prompt text.
    pub async fn expand_mcp_prompt(&self, server: &str, name: &str, raw: &str) -> anyhow::Result<String> {
        let prompt = self
            .mcp_prompts
            .iter()
            .find(|p| p.server == server && p.name == name)
            .ok_or_else(|| anyhow!("Unknown MCP prompt {server}:{name}. /prompts lists them."))?;
        let arguments = parse_prompt_arguments(prompt, raw).map_err(|e| anyhow!(e))?;
        let client = self
            .mcp
            .iter()
            .map(|(client, _)| client)
            .find(|c| c.name == server)
            .ok_or_else(|| anyhow!("MCP server {server} is not connected"))?;
        self.inspector.record("harness", json!({ "mcpPrompt": format!("{server}:{name}"), "arguments": arguments }));
        client.get_prompt(name, &arguments).await
    }
}
