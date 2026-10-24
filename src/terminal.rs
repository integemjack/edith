//! 终端上的样子（本机干活、接到别的大脑上都用它）：回答打到标准输出，过程（工具、提示、能力交来的话）打到标准错误

use std::io::Write;

use serde_json::Value;

/// 终端上的样子：回答打到标准输出，过程（工具、提示、能力交来的话）打到标准错误
pub struct Printer {
    quiet: bool,
    verbose: bool,
    thinking: bool,
    line_start: bool,
}

impl Printer {
    pub fn new(quiet: bool, verbose: bool) -> Printer {
        Printer { quiet, verbose, thinking: false, line_start: true }
    }

    fn out(&mut self, text: &str) {
        print!("{text}");
        let _ = std::io::stdout().flush();
        self.line_start = text.ends_with('\n');
    }

    fn side(&mut self, text: &str) {
        if self.thinking {
            self.thinking = false;
            eprint!("\x1b[0m");
        }
        if !self.line_start {
            println!();
            self.line_start = true;
        }
        eprintln!("{text}");
    }

    /// 打一件事（mind 的 Envelope、或者接到别的大脑上时收到的 event）。all_sessions：edith run 打所有会话的（前面标上会话）
    pub fn show(&mut self, kind: &str, session: &str, data: &Value, all_sessions: bool) {
        if self.quiet {
            if kind == "done" {
                println!("{}", data["answer"].as_str().unwrap_or("").trim());
            }
            return;
        }
        let tag = if all_sessions { format!("[{}] ", &session[..8.min(session.len())]) } else { String::new() };
        match kind {
            "turn.started" => {
                let origin = &data["origin"];
                let capability = origin["capability"].as_str().unwrap_or("");
                if !capability.is_empty() || all_sessions {
                    let from = origin["from"].as_str().filter(|from| !from.is_empty()).map(|from| format!(" {from}")).unwrap_or_default();
                    let source = if capability.is_empty() { "终端".to_string() } else { format!("能力「{capability}」") };
                    let text = data["text"].as_str().unwrap_or("").chars().take(200).collect::<String>();
                    self.side(&format!("\x1b[35m{tag}◂ {source}{from}：{text}\x1b[0m"));
                }
            }
            "text" => {
                if self.thinking {
                    self.thinking = false;
                    eprintln!("\x1b[0m");
                }
                self.out(data["text"].as_str().unwrap_or(""));
            }
            "thinking" if self.verbose => {
                if !self.thinking {
                    self.thinking = true;
                    eprint!("\x1b[2m");
                }
                eprint!("{}", data["text"].as_str().unwrap_or(""));
            }
            "tool" => {
                let brief = brief(&data["arguments"]);
                self.side(&format!("\x1b[36m{tag}▸ {}\x1b[0m {brief}", data["name"].as_str().unwrap_or("")));
            }
            "result" => {
                let text = data["text"].as_str().unwrap_or("");
                let first = text.lines().find(|line| !line.trim().is_empty()).unwrap_or("").chars().take(100).collect::<String>();
                let mark = if data["ok"].as_bool() == Some(true) { "\x1b[32m✓\x1b[0m" } else { "\x1b[31m✗\x1b[0m" };
                self.side(&format!("  {mark} \x1b[2m{first}\x1b[0m"));
            }
            "stats" if self.verbose => {
                let line = format!(
                    "  · 输入 {} token、输出 {} token",
                    data["promptTokens"].as_i64().unwrap_or(0),
                    data["generatedTokens"].as_i64().unwrap_or(0)
                );
                self.side(&format!("\x1b[2m{line}\x1b[0m"));
            }
            "notice" => self.side(&format!("\x1b[33m{tag}{}\x1b[0m", data["text"].as_str().unwrap_or(""))),
            "done" => {
                if !self.line_start {
                    println!();
                    self.line_start = true;
                }
                if data["stopped"].as_bool() == Some(true) {
                    self.side("\x1b[33m（停下了）\x1b[0m");
                }
                if let Some(error) = data["error"].as_str() {
                    self.side(&format!("\x1b[31m{error}\x1b[0m"));
                }
            }
            _ => {}
        }
    }
}

/// 工具参数的一行简述（路径、命令、关键词……）
pub fn brief(arguments: &Value) -> String {
    for key in ["command", "path", "pattern", "keywords", "url", "query", "text"] {
        if let Some(text) = arguments.get(key).and_then(Value::as_str) {
            let line = text.lines().next().unwrap_or("");
            let clipped: String = line.chars().take(90).collect();
            return if clipped.len() < text.len() { format!("{clipped}…") } else { clipped };
        }
    }
    let text = arguments.to_string();
    if text == "{}" { String::new() } else { text.chars().take(90).collect() }
}
