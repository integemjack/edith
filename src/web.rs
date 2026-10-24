//! 搜网页（web_search）、看网页（fetch_url）。
//! - 搜：同时查 Bing、DuckDuckGo、百度（照 Friday 的 BuiltinSearch：按浏览器的样子请求结果页、各自解析、轮流合并去重）；
//!   `~/.edith/config.json` 写了 `"searxng": "http://…"` 就只问 SearXNG（JSON 接口）。
//! - 看：按页面的编码解码（GBK 之类的中文网页也行），取标题、正文（标题层级、列表、段落）和页面里的链接（编号列在后面）；
//!   正文几乎是空的、页面又带脚本（要 JS 才渲染的）时，本机装了 Chrome / Edge / Chromium 就用它无头渲染一遍再读。
//!
//! 测试可以用 EDITH_BING_URL / EDITH_DDG_URL / EDITH_BAIDU_URL 把三家的结果页换成本机的假页面。

use std::collections::HashSet;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use friday_core::tool_files;
use regex::Regex;
use serde_json::Value;

use crate::tools::Outcome;

/// 结果页按浏览器来的请求给：带 EDITH 字样的 UA 常被当成爬虫给空页或验证码
const BROWSER: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0 Safari/537.36";
const TIMEOUT: Duration = Duration::from_secs(20);

static AGENT: LazyLock<ureq::Agent> = LazyLock::new(|| {
    ureq::Agent::config_builder().http_status_as_error(false).timeout_global(Some(TIMEOUT)).user_agent(BROWSER).build().into()
});

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).unwrap()
}

// MARK: - 文字

/// HTML 实体
fn unescape(text: &str) -> String {
    static ENTITY: LazyLock<Regex> = LazyLock::new(|| regex(r"&(#[0-9]{1,7}|#[xX][0-9a-fA-F]{1,6}|[a-zA-Z]{2,8});"));
    ENTITY
        .replace_all(text, |captures: &regex::Captures| {
            let name = &captures[1];
            let decoded = if let Some(number) = name.strip_prefix('#') {
                let code = match number.strip_prefix(['x', 'X']) {
                    Some(hex) => u32::from_str_radix(hex, 16).ok(),
                    None => number.parse().ok(),
                };
                code.filter(|code| *code != 0)
                    .and_then(char::from_u32)
                    .map(|c| if matches!(c, '\u{a0}' | '\u{2002}' | '\u{2003}') { ' ' } else { c }.to_string())
            } else {
                Some(
                    match name {
                        "amp" => "&",
                        "lt" => "<",
                        "gt" => ">",
                        "quot" => "\"",
                        "apos" => "'",
                        "nbsp" | "ensp" | "emsp" | "thinsp" => " ",
                        "middot" => "·",
                        "hellip" => "…",
                        "mdash" => "—",
                        "ndash" => "–",
                        "ldquo" => "“",
                        "rdquo" => "”",
                        "lsquo" => "‘",
                        "rsquo" => "’",
                        "times" => "×",
                        "copy" => "©",
                        "reg" => "®",
                        _ => return captures[0].to_string(),
                    }
                    .to_string(),
                )
            };
            decoded.unwrap_or_else(|| captures[0].to_string())
        })
        .into_owned()
}

/// 去标签、解实体、空白并成一个空格
fn plain(html: &str) -> String {
    static TAGS: LazyLock<Regex> = LazyLock::new(|| regex(r"<[^>]+>"));
    static SPACES: LazyLock<Regex> = LazyLock::new(|| regex(r"\s+"));
    SPACES.replace_all(&unescape(&TAGS.replace_all(html, " ")), " ").trim().to_string()
}

fn attribute(tag: &str, name: &str) -> String {
    let pattern = regex(&format!(r#"\b{}="([^"]*)""#, regex::escape(name)));
    pattern.captures(tag).map(|captures| unescape(&captures[1])).unwrap_or_default()
}

fn clip(text: String) -> String {
    if text.chars().count() > 300 { format!("{}…", text.chars().take(300).collect::<String>()) } else { text }
}

/// 网址查询参数里的一项（百分号解码）
fn query_value(url: &str, key: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    let raw = query.split('&').filter_map(|pair| pair.split_once('=')).find(|(name, _)| *name == key)?.1;
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("zz"), 16)
        {
            out.push(byte);
            index += 3;
            continue;
        }
        out.push(if bytes[index] == b'+' { b' ' } else { bytes[index] });
        index += 1;
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}

/// 网址编码（查询参数的值）
fn encode(text: &str) -> String {
    text.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (byte as char).to_string(),
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

// MARK: - 搜

#[derive(Debug, Clone)]
struct Hit {
    title: String,
    url: String,
    snippet: String,
}

struct EngineResult {
    engine: &'static str,
    hits: Vec<Hit>,
    failure: Option<String>,
}

fn has_han(text: &str) -> bool {
    text.chars().any(|c| matches!(c as u32, 0x4E00..=0x9FFF | 0x3400..=0x4DBF))
}

fn engines(query: &str, language: &str) -> Vec<&'static str> {
    if language.starts_with("en") {
        vec!["DuckDuckGo", "Bing"]
    } else if language.starts_with("zh") || has_han(query) {
        vec!["百度", "Bing", "DuckDuckGo"]
    } else {
        vec!["DuckDuckGo", "Bing", "百度"]
    }
}

fn request_url(engine: &str, query: &str, range: &str, language: &str) -> String {
    let now = chrono::Utc::now().timestamp();
    let span = match range {
        "day" => Some(1),
        "week" => Some(7),
        "month" => Some(31),
        "year" => Some(366),
        _ => None,
    };
    let base = |variable: &str, default: &str| {
        std::env::var(variable).ok().filter(|value| !value.is_empty()).unwrap_or_else(|| default.to_string())
    };
    let mut parts = Vec::new();
    let url = match engine {
        "Bing" => {
            parts.push(format!("q={}", encode(query)));
            if language.starts_with("en") {
                parts.push("setlang=en".into());
            } else if language.starts_with("zh") {
                parts.push("setlang=zh-Hans".into());
            }
            // ez1 / ez2 / ez3 = 一天 / 一周 / 一月；一年用 ez5_<起>_<止>（自 1970 年起的天数）
            match range {
                "day" | "week" | "month" => {
                    let code = match range {
                        "day" => "ez1",
                        "week" => "ez2",
                        _ => "ez3",
                    };
                    parts.push(format!("filters={}", encode(&format!("ex1:\"{code}\""))));
                }
                "year" => {
                    let today = now / 86400;
                    parts.push(format!("filters={}", encode(&format!("ex1:\"ez5_{}_{today}\"", today - 365))));
                }
                _ => {}
            }
            base("EDITH_BING_URL", "https://www.bing.com/search")
        }
        "DuckDuckGo" => {
            parts.push(format!("q={}", encode(query)));
            if language.starts_with("en") {
                parts.push("kl=us-en".into());
            } else if language.starts_with("zh") {
                parts.push("kl=cn-zh".into());
            }
            if span.is_some() {
                parts.push(format!("df={}", &range[..1]));
            }
            base("EDITH_DDG_URL", "https://html.duckduckgo.com/html/")
        }
        _ => {
            parts.push(format!("wd={}", encode(query)));
            parts.push("ie=utf-8".into());
            if let Some(days) = span {
                parts.push(format!("gpc={}", encode(&format!("stf={},{now}|stftype=1", now - days * 86400))));
            }
            base("EDITH_BAIDU_URL", "https://www.baidu.com/s")
        }
    };
    format!("{url}?{}", parts.join("&"))
}

/// 一段一段：每条结果的开头到下一条的开头（最后一段最多 limit 个字节）
fn segments<'a>(html: &'a str, start: &Regex, limit: usize) -> Vec<&'a str> {
    let starts: Vec<usize> = start.find_iter(html).map(|found| found.start()).collect();
    starts
        .iter()
        .enumerate()
        .map(|(index, &begin)| {
            let mut end = starts.get(index + 1).copied().unwrap_or_else(|| (begin + limit).min(html.len()));
            while !html.is_char_boundary(end) {
                end -= 1;
            }
            &html[begin..end]
        })
        .collect()
}

/// Bing 有时把结果链接包成 bing.com/ck/a?…&u=a1<base64url>
fn bing_target(href: &str) -> String {
    if !href.contains("bing.com/ck/a") {
        return href.to_string();
    }
    let Some(encoded) = query_value(href, "u").and_then(|value| value.strip_prefix("a1").map(str::to_string)) else {
        return href.to_string();
    };
    let standard = encoded.replace('-', "+").replace('_', "/");
    let padded = format!("{standard}{}", "=".repeat((4 - standard.len() % 4) % 4));
    friday_core::mcp_client::decode_base64(&padded)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .filter(|target| target.starts_with("http"))
        .unwrap_or_else(|| href.to_string())
}

fn parse_bing(html: &str) -> Vec<Hit> {
    static START: LazyLock<Regex> = LazyLock::new(|| regex(r#"<li class="b_algo""#));
    static LINK: LazyLock<Regex> = LazyLock::new(|| regex(r"(?s)<h2[^>]*>\s*(<a\s[^>]*>)(.*?)</a>"));
    static PARAGRAPH: LazyLock<Regex> = LazyLock::new(|| regex(r"(?s)<p[^>]*>(.*?)</p>"));
    segments(html, &START, 20000)
        .into_iter()
        .filter_map(|segment| {
            let title = LINK.captures(segment)?;
            let hit = Hit {
                url: bing_target(&attribute(&title[1], "href")),
                title: plain(&title[2]),
                snippet: PARAGRAPH.captures(segment).map(|snippet| clip(plain(&snippet[1]))).unwrap_or_default(),
            };
            (!hit.title.is_empty() && hit.url.starts_with("http")).then_some(hit)
        })
        .collect()
}

fn parse_duckduckgo(html: &str) -> Vec<Hit> {
    static ANCHOR: LazyLock<Regex> = LazyLock::new(|| regex(r#"(?s)(<a\s[^>]*class="result__a"[^>]*>)(.*?)</a>"#));
    static SNIPPET: LazyLock<Regex> = LazyLock::new(|| regex(r#"(?s)class="result__snippet"[^>]*>(.*?)</a>"#));
    let anchors: Vec<regex::Captures> = ANCHOR.captures_iter(html).collect();
    let mut hits = Vec::new();
    for (index, anchor) in anchors.iter().enumerate() {
        let mut href = attribute(&anchor[1], "href");
        if href.starts_with("//") {
            href = format!("https:{href}");
        }
        // 结果链接经 duckduckgo.com/l/?uddg=<编码后的真实网址>；广告走 y.js，跳过
        if href.contains("duckduckgo.com/y.js") {
            continue;
        }
        if href.contains("uddg=")
            && let Some(target) = query_value(&href, "uddg").filter(|target| !target.is_empty())
        {
            href = target;
        }
        let whole = anchor.get(0).unwrap();
        let mut end =
            anchors.get(index + 1).map(|next| next.get(0).unwrap().start()).unwrap_or_else(|| (whole.end() + 4000).min(html.len()));
        while !html.is_char_boundary(end) {
            end -= 1;
        }
        let rest = &html[whole.end()..end.max(whole.end())];
        let hit = Hit {
            url: href,
            title: plain(&anchor[2]),
            snippet: SNIPPET.captures(rest).map(|snippet| clip(plain(&snippet[1]))).unwrap_or_default(),
        };
        if !hit.title.is_empty() && hit.url.starts_with("http") {
            hits.push(hit);
        }
    }
    hits
}

fn parse_baidu(html: &str) -> Vec<Hit> {
    static START: LazyLock<Regex> = LazyLock::new(|| regex(r#"<div class="result(?:-op)? c-container"#));
    static OPEN: LazyLock<Regex> = LazyLock::new(|| regex(r"^<div[^>]*>"));
    static HEADING: LazyLock<Regex> = LazyLock::new(|| regex(r"(?s)<h3[^>]*>(.*?)</h3>"));
    static HEADING_LINK: LazyLock<Regex> = LazyLock::new(|| regex(r"<a\s[^>]*>"));
    static SUMMARY: LazyLock<Regex> = LazyLock::new(|| {
        regex(r#"(?s)<(?:span|div)[^>]*class="[^"]*(?:summary-text|c-abstract|content-right)[^"]*"[^>]*>(.*?)</(?:span|div)>"#)
    });
    segments(html, &START, 30000)
        .into_iter()
        .filter_map(|segment| {
            let title = HEADING.captures(segment)?;
            // mu 是真实网址；没有时用标题上的百度跳转链接（fetch_url 打开时会跳过去）
            let mut url = OPEN.find(segment).map(|tag| attribute(tag.as_str(), "mu")).unwrap_or_default();
            if !url.starts_with("http") {
                url = HEADING_LINK.find(&title[1]).map(|tag| attribute(tag.as_str(), "href")).unwrap_or_default();
            }
            let hit = Hit {
                title: plain(&title[1]),
                url,
                snippet: SUMMARY.captures(segment).map(|snippet| clip(plain(&snippet[1]))).unwrap_or_default(),
            };
            (!hit.title.is_empty() && hit.url.starts_with("http")).then_some(hit)
        })
        .collect()
}

fn fetch_engine(engine: &'static str, query: &str, range: &str, language: &str) -> EngineResult {
    let failed = |failure: String| EngineResult { engine, hits: Vec::new(), failure: Some(failure) };
    let url = request_url(engine, query, range, language);
    let accept_language = if language.starts_with("en") { "en-US,en;q=0.9" } else { "zh-CN,zh;q=0.9,en;q=0.8" };
    let mut response =
        match AGENT.get(&url).header("Accept", "text/html,application/xhtml+xml").header("Accept-Language", accept_language).call() {
            Ok(response) => response,
            Err(error) => return failed(error.to_string()),
        };
    // DuckDuckGo 限流时回 202 + 验证页；百度跳安全验证
    let status = response.status().as_u16();
    if status != 200 {
        return failed(if status == 202 { "要人机验证".into() } else { format!("HTTP {status}") });
    }
    let html = response.body_mut().with_config().limit(8 * 1024 * 1024).read_to_string().unwrap_or_default();
    if html.contains("anomaly-modal") || html.contains("wappass.baidu.com") || html.contains("百度安全验证") {
        return failed("要人机验证".into());
    }
    let hits = match engine {
        "Bing" => parse_bing(&html),
        "DuckDuckGo" => parse_duckduckgo(&html),
        _ => parse_baidu(&html),
    };
    let failure = (hits.is_empty() && html.len() > 20000).then(|| "没解析出结果（页面可能改版了）".to_string());
    EngineResult { engine, hits, failure }
}

fn dedupe_key(url: &str) -> String {
    let mut key = url.trim().to_lowercase();
    for prefix in ["https://", "http://"] {
        if let Some(rest) = key.strip_prefix(prefix) {
            key = rest.to_string();
            break;
        }
    }
    if let Some(rest) = key.strip_prefix("www.") {
        key = rest.to_string();
    }
    key.trim_end_matches('/').to_string()
}

/// 各家轮流取：第 1 条、第 2 条……去掉重复的网址
fn merge(results: &[EngineResult], count: usize) -> Vec<Hit> {
    let mut merged = Vec::new();
    let mut seen = HashSet::new();
    for rank in 0.. {
        let mut any = false;
        for result in results {
            let Some(hit) = result.hits.get(rank) else { continue };
            any = true;
            if seen.insert(dedupe_key(&hit.url)) {
                merged.push(hit.clone());
                if merged.len() >= count {
                    return merged;
                }
            }
        }
        if !any {
            break;
        }
    }
    merged
}

fn format(results: &[EngineResult], query: &str, count: usize) -> Outcome {
    let hits = merge(results, count);
    if hits.is_empty() {
        let failed: Vec<String> =
            results.iter().filter_map(|result| result.failure.as_ref().map(|failure| format!("{}（{failure}）", result.engine))).collect();
        let mut text = format!("搜索「{query}」没有结果。");
        if failed.is_empty() {
            text.push_str("\n换个说法、换语言（language）或去掉 time_range 再试。");
        } else {
            text.push_str(&format!(
                "\n没响应的搜索引擎：{}\n多半是网络问题（超时、要人机验证、连不上）。换个说法过一会儿再试，或用 fetch_url 直接打开已知的可靠网址（官方文档、GitHub 发布页等）。",
                failed.join("、")
            ));
        }
        return Outcome { ok: false, text };
    }
    let lines: Vec<String> = hits
        .iter()
        .enumerate()
        .map(|(index, hit)| {
            let mut entry = format!("{}. {}\n   {}", index + 1, hit.title, hit.url);
            if !hit.snippet.is_empty() {
                entry.push_str(&format!("\n   {}", hit.snippet));
            }
            entry
        })
        .collect();
    Outcome {
        ok: true,
        text: format!(
            "搜索「{query}」的前 {} 条结果：\n{}\n\n摘要可能过时或不完整：重要结论用 fetch_url 打开原文核实，回答时附上来源网址。",
            lines.len(),
            lines.join("\n")
        ),
    }
}

/// SearXNG（config.json 写了 searxng 地址时）：它自己合好了各家
fn searxng(base: &str, query: &str, range: &str, language: &str) -> EngineResult {
    let mut url = format!("{}/search?q={}&format=json", base.trim_end_matches('/'), encode(query));
    if !range.is_empty() {
        url.push_str(&format!("&time_range={range}"));
    }
    if !language.is_empty() {
        url.push_str(&format!("&language={language}"));
    }
    let outcome = AGENT.get(&url).call().map_err(|error| error.to_string()).and_then(|mut response| {
        let status = response.status().as_u16();
        let body = response.body_mut().read_to_string().map_err(|error| error.to_string())?;
        if status != 200 {
            return Err(format!("HTTP {status}"));
        }
        serde_json::from_str::<Value>(&body).map_err(|error| error.to_string())
    });
    match outcome {
        Ok(value) => EngineResult {
            engine: "SearXNG",
            hits: value["results"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|item| {
                    let url = item["url"].as_str()?.to_string();
                    Some(Hit {
                        title: item["title"].as_str().unwrap_or("").to_string(),
                        url,
                        snippet: clip(item["content"].as_str().unwrap_or("").to_string()),
                    })
                })
                .collect(),
            failure: None,
        },
        Err(failure) => EngineResult { engine: "SearXNG", hits: Vec::new(), failure: Some(failure) },
    }
}

/// web_search
pub fn search(query: &str, range: &str, language: &str, count: usize) -> Outcome {
    let query = query.trim();
    if query.is_empty() {
        return Outcome { ok: false, text: "query 是空的".into() };
    }
    let count = count.clamp(1, 20);
    let configured =
        crate::home::read_json(&crate::home::dir().join("config.json")).and_then(|config| config["searxng"].as_str().map(str::to_string));
    if let Some(base) = configured.filter(|base| !base.is_empty()) {
        return format(&[searxng(&base, query, range, language)], query, count);
    }
    // 几家一起查
    let handles: Vec<_> = engines(query, language)
        .into_iter()
        .map(|engine| {
            let (query, range, language) = (query.to_string(), range.to_string(), language.to_string());
            std::thread::spawn(move || fetch_engine(engine, &query, &range, &language))
        })
        .collect();
    let results: Vec<EngineResult> = handles.into_iter().filter_map(|handle| handle.join().ok()).collect();
    format(&results, query, count)
}

// MARK: - 看

/// 网页的正文：标题层级、列表、段落留着，导航、脚本、样式去掉；链接编号列在后面
fn readable(html: &str, base: &str) -> (String, String, Vec<(String, String)>) {
    static TITLE: LazyLock<Regex> = LazyLock::new(|| regex(r"(?is)<title[^>]*>(.*?)</title>"));
    static DROP: LazyLock<Regex> = LazyLock::new(|| {
        regex(r"(?is)<(script|style|noscript|svg|template|iframe)\b.*?</(script|style|noscript|svg|template|iframe)>|<!--.*?-->")
    });
    static CHROME: LazyLock<Regex> = LazyLock::new(|| regex(r"(?is)<(nav|footer|aside)\b.*?</(nav|footer|aside)>"));
    static HEADING: LazyLock<Regex> = LazyLock::new(|| regex(r"(?is)<h([1-6])[^>]*>(.*?)</h[1-6]>"));
    static ITEM: LazyLock<Regex> = LazyLock::new(|| regex(r"(?is)<li[^>]*>"));
    static BLOCK: LazyLock<Regex> =
        LazyLock::new(|| regex(r"(?is)</?(p|div|section|article|br|tr|table|ul|ol|blockquote|pre|header|main|form)\b[^>]*>"));
    static LINK: LazyLock<Regex> = LazyLock::new(|| regex(r##"(?is)<a\s[^>]*href="([^"#][^"]*)"[^>]*>(.*?)</a>"##));
    static TAGS: LazyLock<Regex> = LazyLock::new(|| regex(r"<[^>]+>"));
    static SPACES: LazyLock<Regex> = LazyLock::new(|| regex(r"[ \t\u{a0}]+"));
    static LINES: LazyLock<Regex> = LazyLock::new(|| regex(r"\n\s*\n(\s*\n)*"));
    let title = TITLE.captures(html).map(|captures| plain(&captures[1])).unwrap_or_default();
    let body = html.split_once("<body").and_then(|(_, rest)| rest.split_once('>')).map_or(html, |(_, rest)| rest);
    let mut text = DROP.replace_all(body, " ").into_owned();
    // 正文够长时连导航、页脚一起去掉（不够长的页面它们可能就是内容）
    let without_chrome = CHROME.replace_all(&text, " ").into_owned();
    if plain(&without_chrome).chars().count() > 500 {
        text = without_chrome;
    }
    let mut links: Vec<(String, String)> = Vec::new();
    text = LINK
        .replace_all(&text, |captures: &regex::Captures| {
            let label = plain(&captures[2]);
            let href = absolute(base, &unescape(&captures[1]));
            if label.is_empty() || !href.starts_with("http") {
                return label;
            }
            let index = match links.iter().position(|(_, url)| *url == href) {
                Some(index) => index,
                None => {
                    links.push((label.clone(), href));
                    links.len() - 1
                }
            };
            format!("{label}[{}]", index + 1)
        })
        .into_owned();
    text = HEADING
        .replace_all(&text, |captures: &regex::Captures| {
            format!("\n\n{} {}\n\n", "#".repeat(captures[1].parse().unwrap_or(2)), plain(&captures[2]))
        })
        .into_owned();
    text = ITEM.replace_all(&text, "\n- ").into_owned();
    text = BLOCK.replace_all(&text, "\n").into_owned();
    text = unescape(&TAGS.replace_all(&text, " "));
    text = SPACES.replace_all(&text, " ").into_owned();
    let text: String = text.lines().map(str::trim).collect::<Vec<_>>().join("\n");
    let text = LINES.replace_all(&text, "\n\n").trim().to_string();
    (title, text, links)
}

/// 相对链接 → 绝对网址
fn absolute(base: &str, href: &str) -> String {
    if href.starts_with("http://") || href.starts_with("https://") {
        return href.to_string();
    }
    let Some((scheme, rest)) = base.split_once("://") else { return href.to_string() };
    let host = rest.split('/').next().unwrap_or("");
    if let Some(rest) = href.strip_prefix("//") {
        return format!("{scheme}://{rest}");
    }
    if href.starts_with('/') {
        return format!("{scheme}://{host}{href}");
    }
    let path = rest.split(['?', '#']).next().unwrap_or("");
    let folder = path.rsplit_once('/').map_or(host, |(folder, _)| folder);
    format!("{scheme}://{folder}/{href}")
}

/// 按页面说的编码解码（HTTP 头的 charset，没有就看 <meta charset>）
fn decode(bytes: &[u8], content_type: &str) -> String {
    static META: LazyLock<Regex> = LazyLock::new(|| regex(r#"(?i)<meta[^>]+charset=["']?([a-zA-Z0-9_-]+)"#));
    let declared = content_type
        .split(';')
        .find_map(|part| part.trim().strip_prefix("charset=").map(|charset| charset.trim_matches('"').to_string()))
        .or_else(|| {
            let head = String::from_utf8_lossy(&bytes[..bytes.len().min(4096)]).into_owned();
            META.captures(&head).map(|captures| captures[1].to_string())
        });
    let encoding = declared.and_then(|label| encoding_rs::Encoding::for_label(label.as_bytes())).unwrap_or(encoding_rs::UTF_8);
    encoding.decode(bytes).0.into_owned()
}

/// 本机的 Chrome / Edge / Chromium（要 JS 才渲染的页面用它）
fn browser() -> Option<String> {
    let candidates: &[&str] = if cfg!(target_os = "macos") {
        &[
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
        ]
    } else if cfg!(windows) {
        &[
            r"C:\Program Files\Google\Chrome\Application\chrome.exe",
            r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
            r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
            r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
        ]
    } else {
        &[
            "/usr/bin/google-chrome",
            "/usr/bin/google-chrome-stable",
            "/usr/bin/chromium",
            "/usr/bin/chromium-browser",
            "/usr/bin/microsoft-edge",
            "/snap/bin/chromium",
        ]
    };
    if let Ok(configured) = std::env::var("EDITH_BROWSER")
        && !configured.is_empty()
    {
        return Some(configured);
    }
    candidates.iter().find(|path| std::path::Path::new(path).is_file()).map(|path| path.to_string())
}

/// 用无头浏览器渲染完再拿 HTML（最多 30 秒）
fn render(url: &str) -> Option<String> {
    let program = browser()?;
    let profile = std::env::temp_dir().join(format!("edith-browser-{}", std::process::id()));
    let mut child = Command::new(program)
        .args([
            "--headless=new",
            "--disable-gpu",
            "--no-first-run",
            "--no-default-browser-check",
            "--virtual-time-budget=8000",
            "--dump-dom",
        ])
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut html = String::new();
        let _ = stdout.read_to_string(&mut html);
        html
    });
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline && matches!(child.try_wait(), Ok(None)) {
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
    let html = reader.join().ok()?;
    let _ = std::fs::remove_dir_all(&profile);
    (!html.trim().is_empty()).then_some(html)
}

/// fetch_url：看网页
pub fn fetch(url: &str, offset: Option<i64>) -> Outcome {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Outcome { ok: false, text: "url 要以 http:// 或 https:// 开头".into() };
    }
    let mut response = match AGENT.get(url).header("Accept", "text/html,application/xhtml+xml,text/plain;q=0.9,*/*;q=0.8").call() {
        Ok(response) => response,
        Err(error) => return Outcome { ok: false, text: format!("打不开 {url}：{error}") },
    };
    let status = response.status().as_u16();
    let content_type = response.headers().get("content-type").and_then(|value| value.to_str().ok()).unwrap_or("").to_lowercase();
    let bytes = match response.body_mut().with_config().limit(10 * 1024 * 1024).read_to_vec() {
        Ok(bytes) => bytes,
        Err(error) => return Outcome { ok: false, text: format!("没读完 {url}：{error}") },
    };
    if content_type.contains("pdf") || bytes.starts_with(b"%PDF") {
        return Outcome {
            ok: false,
            text: format!(
                "{url} 是 PDF（{}），现在读不了：下载下来用命令行工具转文字（比如 pdftotext）",
                tool_files::byte_count(bytes.len() as i64)
            ),
        };
    }
    let body = decode(&bytes, &content_type);
    let html = content_type.contains("html") || body.trim_start().starts_with('<');
    if !(200..300).contains(&status) {
        let text = if html { readable(&body, url).1 } else { body };
        return Outcome { ok: false, text: format!("HTTP {status}\n{}", tool_files::truncate(&text)) };
    }
    if !html {
        return Outcome { ok: true, text: tool_files::page(&body, offset) };
    }
    let (mut title, mut text, mut links) = readable(&body, url);
    let mut rendered = false;
    // 正文几乎是空的、页面带脚本：多半要 JS 渲染
    if text.chars().count() < 200
        && body.contains("<script")
        && let Some(dom) = render(url)
    {
        let (new_title, new_text, new_links) = readable(&dom, url);
        if new_text.chars().count() > text.chars().count() {
            (title, text, links, rendered) = (if new_title.is_empty() { title } else { new_title }, new_text, new_links, true);
        }
    }
    let mut page = String::new();
    if !title.is_empty() {
        page.push_str(&format!("# {title}\n"));
    }
    page.push_str(&format!("{url}{}\n\n", if rendered { "（用浏览器渲染后读的）" } else { "" }));
    page.push_str(&text);
    if !links.is_empty() {
        page.push_str("\n\n链接：\n");
        for (index, (label, href)) in links.iter().take(60).enumerate() {
            page.push_str(&format!("[{}] {} {href}\n", index + 1, label.chars().take(60).collect::<String>()));
        }
        if links.len() > 60 {
            page.push_str(&format!("…（共 {} 个链接）\n", links.len()));
        }
    }
    Outcome { ok: true, text: tool_files::page(page.trim_end(), offset) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_results() {
        let bing = r#"<li class="b_algo"><h2><a href="https://example.com/a">Example &amp; A</a></h2><p>Snippet one</p></li><li class="b_algo"><h2><a href="https://example.com/b">B</a></h2></li>"#;
        let hits = parse_bing(bing);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "Example & A");
        assert_eq!(hits[0].snippet, "Snippet one");
        let ddg = r#"<a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Frust-lang.org%2F&rut=x">Rust</a><a class="result__snippet" href="x">A language</a>"#;
        let hits = parse_duckduckgo(ddg);
        assert_eq!(hits[0].url, "https://rust-lang.org/");
        assert_eq!(hits[0].snippet, "A language");
        let baidu = r#"<div class="result c-container" mu="https://zh.example.com/"><h3><a href="https://www.baidu.com/link?x">中文 标题</a></h3><span class="content-right_1">摘要</span></div>"#;
        let hits = parse_baidu(baidu);
        assert_eq!(hits[0].url, "https://zh.example.com/");
        assert_eq!(hits[0].title, "中文 标题");
    }

    #[test]
    fn readable_page() {
        let html = r#"<html><head><title>Doc</title><script>x()</script></head><body><h1>Intro</h1><p>Hello <a href="/next">next page</a>.</p><ul><li>one</li><li>two</li></ul></body></html>"#;
        let (title, text, links) = readable(html, "https://example.com/docs/index.html");
        assert_eq!(title, "Doc");
        assert!(
            text.contains("# Intro") && text.contains("Hello next page[1]") && text.contains("- one") && !text.contains("x()"),
            "{text}"
        );
        assert_eq!(links[0].1, "https://example.com/next");
        assert_eq!(absolute("https://example.com/docs/index.html", "a.html"), "https://example.com/docs/a.html");
        // GBK 的中文页
        let (bytes, _, _) = encoding_rs::GBK.encode("<meta charset=\"gbk\"><p>你好</p>");
        assert!(decode(&bytes, "text/html").contains("你好"));
    }
}

#[cfg(test)]
mod live {
    /// 真上网：cargo test --bin edith live -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_search_and_fetch() {
        let found = super::search("rust regex crate", "", "", 5);
        println!("{}", found.text);
        let found = super::search("北京天气", "week", "", 5);
        println!("{}", found.text);
        let page = super::fetch("https://www.rust-lang.org/", None);
        println!("{}", page.text.chars().take(1500).collect::<String>());
        let page = super::fetch("https://www.sina.com.cn/", None);
        println!("{}", page.text.chars().take(600).collect::<String>());
    }
}
