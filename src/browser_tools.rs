//! 智能体的浏览器工具（照 Friday 的 BrowserTools，Friday 仓库的 qt/docs/BROWSER.md §6）：browser_navigate / read / act / screenshot /
//! resize / tabs / hand_over / run_js，操作的是这段会话在大脑里的浏览器标签（browser.rs）——用户在 Friday 的浏览器卡片里看得见、能一起操作。
//! 定义和说明在 friday-core（tool_defs、prompts-tools.md 的 tool.browser_*）；按需加载（deferred.rs 的「浏览器」一组）。
//! 智能体开的标签这一轮结束时关掉（带 keep 的留给用户）；截图随下一段请求给能看图的模型。

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

#[cfg(test)]
use serde_json::json;
use serde_json::{Map, Value};

use crate::browser;
use crate::tools::Outcome;

pub const TOOLS: &[&str] = &[
    "browser_navigate",
    "browser_read",
    "browser_act",
    "browser_screenshot",
    "browser_resize",
    "browser_tabs",
    "browser_hand_over",
    "browser_run_js",
];

/// 结果只留这么多字
const MAX_TEXT: usize = 16000;
/// 元素列表最多列这么多个
const MAX_ELEMENTS: usize = 300;

pub fn is_tool(name: &str) -> bool {
    TOOLS.contains(&name)
}

/// 这几个工具的定义（这台有浏览器才给）
pub fn specs() -> Vec<Value> {
    if !browser::available() {
        return Vec::new();
    }
    let builtins = friday_core::tool_defs::builtins(&Default::default(), None);
    TOOLS.iter().filter_map(|name| builtins.iter().find(|tool| tool.name == *name).map(|tool| tool.spec.clone())).collect()
}

fn clip(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    format!("{}\n…（后面还有 {} 个字符没列出）", text.chars().take(max).collect::<String>(), count - max)
}

fn short(id: &str) -> String {
    id.chars().take(8).collect::<String>().to_lowercase()
}

fn text_arg(arguments: &Map<String, Value>, key: &str) -> Option<String> {
    arguments.get(key).and_then(Value::as_str).map(str::trim).filter(|value| !value.is_empty()).map(str::to_string)
}

fn bool_arg(arguments: &Map<String, Value>, key: &str, fallback: bool) -> bool {
    match arguments.get(key) {
        Some(Value::Bool(value)) => *value,
        Some(Value::String(text)) => match text.trim().to_lowercase().as_str() {
            "true" | "1" | "yes" => true,
            "false" | "0" | "no" => false,
            _ => fallback,
        },
        Some(Value::Number(number)) => number.as_f64().is_some_and(|value| value != 0.0),
        _ => fallback,
    }
}

fn number_arg(arguments: &Map<String, Value>, key: &str) -> Option<f64> {
    match arguments.get(key) {
        Some(Value::Number(number)) => number.as_f64(),
        Some(Value::String(text)) => text.trim().parse().ok(),
        _ => None,
    }
}

fn quoted(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| "\"\"".into())
}

// MARK: - 页面上的脚本（edith 隔离世界）

/// 元素列表：可交互、看得见的元素编上号（编号表存在隔离世界里，同一个元素一直是同一个号）
fn elements_script() -> String {
    r#"
(function (maxCount) {
  const W = window;
  if (!W.__edithRefs) { W.__edithRefs = new Map(); W.__edithIds = new WeakMap(); W.__edithNext = 1; }
  const selector = 'a[href],button,input,textarea,select,summary,[role=button],[role=link],[role=checkbox],[role=radio],[role=tab],' +
    '[role=menuitem],[role=option],[role=switch],[role=combobox],[role=textbox],[role=searchbox],[contenteditable=""],[contenteditable=true],' +
    '[onclick],[tabindex]:not([tabindex="-1"])';
  const vw = W.innerWidth, vh = W.innerHeight;
  const out = [];
  let total = 0;
  const textOf = (el) => (el.innerText || el.textContent || '').replace(/\s+/g, ' ').trim();
  for (const el of document.querySelectorAll(selector)) {
    const r = el.getBoundingClientRect();
    if (r.width < 1 || r.height < 1) continue;
    const st = getComputedStyle(el);
    if (st.visibility === 'hidden' || st.display === 'none' || Number(st.opacity) === 0) continue;
    total++;
    if (out.length >= maxCount) continue;
    let id = W.__edithIds.get(el);
    if (!id) { id = 'e' + (W.__edithNext++); W.__edithIds.set(el, id); }
    W.__edithRefs.set(id, new WeakRef(el));
    const tag = el.tagName.toLowerCase();
    const type = (el.getAttribute('type') || '').toLowerCase();
    let label = el.getAttribute('aria-label') || '';
    if (!label && el.id) { const l = document.querySelector('label[for="' + CSS.escape(el.id) + '"]'); if (l) label = textOf(l); }
    if (!label && (tag === 'input' || tag === 'textarea' || tag === 'select')) { const l = el.closest('label'); if (l) label = textOf(l); }
    let name = label || textOf(el) || el.getAttribute('title') || el.getAttribute('alt') || '';
    if (!name && tag === 'a') { const img = el.querySelector('img[alt]'); if (img) name = img.getAttribute('alt'); }
    const item = { ref: id, tag: tag, role: el.getAttribute('role') || '', type: type, name: name.slice(0, 80),
      inView: r.bottom > 0 && r.top < vh && r.right > 0 && r.left < vw };
    if (tag === 'input' || tag === 'textarea') {
      item.placeholder = (el.getAttribute('placeholder') || '').slice(0, 60);
      item.value = type === 'password' ? (el.value ? '••••' : '') : String(el.value || '').slice(0, 60);
      if (type === 'checkbox' || type === 'radio') item.checked = el.checked;
      item.autocomplete = el.getAttribute('autocomplete') || '';
      item.field = (el.getAttribute('name') || '').slice(0, 40);
    }
    if (tag === 'select') {
      item.value = el.selectedOptions.length ? textOf(el.selectedOptions[0]).slice(0, 60) : '';
      item.options = Array.from(el.options).slice(0, 20).map(o => textOf(o).slice(0, 40));
    }
    if (el.isContentEditable) item.editable = true;
    if (el.disabled) item.disabled = true;
    if (tag === 'a') item.href = (el.getAttribute('href') || '').slice(0, 120);
    out.push(item);
  }
  const d = document.documentElement;
  return { url: location.href, title: document.title, elements: out, total: total,
    scrollY: Math.round(W.scrollY), scrollHeight: Math.round(d.scrollHeight), viewportHeight: vh, viewportWidth: vw };
})(MAX)
"#
    .replace("MAX", &MAX_ELEMENTS.to_string())
}

const TEXT_SCRIPT: &str = r#"
(function () {
  const d = document.documentElement;
  return { url: location.href, title: document.title, text: document.body ? document.body.innerText : '',
    scrollY: Math.round(window.scrollY), scrollHeight: Math.round(d.scrollHeight), viewportHeight: window.innerHeight };
})()
"#;

/// 按编号找元素：滚到视口中间，返回中心点（视口里的 CSS 像素）、是不是被别的东西盖着
fn locate_script(reference: &str) -> String {
    r#"
(function (ref) {
  const map = window.__edithRefs;
  const weak = map && map.get(ref);
  const el = weak && weak.deref();
  if (!el || !el.isConnected) return { missing: true };
  el.scrollIntoView({ block: 'center', inline: 'center', behavior: 'instant' });
  const r = el.getBoundingClientRect();
  const x = r.left + r.width / 2, y = r.top + r.height / 2;
  const top = document.elementFromPoint(x, y);
  const covered = !!top && top !== el && !el.contains(top) && !top.contains(el);
  return { x: x, y: y, covered: covered, tag: el.tagName.toLowerCase(), type: (el.getAttribute('type') || '').toLowerCase(),
    name: (el.getAttribute('aria-label') || el.innerText || el.value || el.getAttribute('placeholder') || '').replace(/\s+/g, ' ').trim().slice(0, 60) };
})(REF)
"#
    .replace("REF", &quoted(reference))
}

/// 下拉框选一项（按选项文字或值）
fn select_script(reference: &str, option: &str) -> String {
    r#"
(function (ref, wanted) {
  const map = window.__edithRefs;
  const weak = map && map.get(ref);
  const el = weak && weak.deref();
  if (!el || !el.isConnected) return { missing: true };
  if (el.tagName.toLowerCase() !== 'select') return { error: 'not-select' };
  const text = (o) => (o.textContent || '').replace(/\s+/g, ' ').trim();
  let match = Array.from(el.options).find(o => text(o) === wanted || o.value === wanted);
  if (!match) match = Array.from(el.options).find(o => text(o).includes(wanted));
  if (!match) return { error: 'no-option', options: Array.from(el.options).map(text).slice(0, 30) };
  el.value = match.value;
  el.dispatchEvent(new Event('input', { bubbles: true }));
  el.dispatchEvent(new Event('change', { bubbles: true }));
  return { selected: text(match) };
})(REF, WANTED)
"#
    .replace("REF", &quoted(reference))
    .replace("WANTED", &quoted(option))
}

const STATE_SCRIPT: &str = r#"
(function () {
  const a = document.activeElement;
  let focus = '';
  if (a && a !== document.body) {
    const ids = window.__edithIds;
    focus = a.tagName.toLowerCase() + (a.getAttribute('type') ? '[' + a.getAttribute('type') + ']' : '') + (ids && ids.get(a) ? ' ' + ids.get(a) : '');
  }
  return { url: location.href, title: document.title, focus: focus };
})()
"#;

// MARK: - 元素的说明（给模型看）、审批用的缓存

#[derive(Clone, Default)]
struct Element {
    kind: String,
    name: String,
    autocomplete: String,
    field: String,
}

/// 标签 → 编号 → 元素（最近一次 browser_read elements 列出的）
static ELEMENTS: LazyLock<Mutex<HashMap<String, HashMap<String, Element>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn field<'a>(item: &'a Value, key: &str) -> &'a str {
    item.get(key).and_then(Value::as_str).unwrap_or("")
}

fn role_name(item: &Value) -> String {
    let (tag, role, kind) = (field(item, "tag"), field(item, "role"), field(item, "type"));
    let roles = [
        ("button", "按钮"),
        ("link", "链接"),
        ("checkbox", "复选框"),
        ("radio", "单选"),
        ("tab", "标签页"),
        ("menuitem", "菜单项"),
        ("option", "选项"),
        ("switch", "开关"),
        ("combobox", "下拉框"),
        ("textbox", "输入框"),
        ("searchbox", "搜索框"),
    ];
    if let Some((_, title)) = roles.iter().find(|(name, _)| *name == role) {
        return title.to_string();
    }
    match tag {
        "a" => "链接".into(),
        "button" | "summary" => "按钮".into(),
        "select" => "下拉框".into(),
        "textarea" => "多行输入框".into(),
        "input" => match kind {
            "checkbox" => "复选框".into(),
            "radio" => "单选".into(),
            "submit" | "button" | "reset" | "image" => "按钮".into(),
            "password" => "密码框".into(),
            "file" => "选文件".into(),
            "" | "text" => "输入框".into(),
            other => format!("输入框（{other}）"),
        },
        _ if item.get("editable").and_then(Value::as_bool) == Some(true) => "可编辑区".into(),
        _ => format!("可点的{tag}"),
    }
}

fn describe(item: &Value) -> String {
    let mut line = format!("[{}] {}", field(item, "ref"), role_name(item));
    let name = field(item, "name");
    if !name.is_empty() {
        line += &format!("「{name}」");
    }
    let placeholder = field(item, "placeholder");
    if !placeholder.is_empty() && placeholder != name {
        line += &format!(" 提示「{placeholder}」");
    }
    if !field(item, "value").is_empty() {
        line += &format!(" 现在是「{}」", field(item, "value"));
    }
    if let Some(checked) = item.get("checked").and_then(Value::as_bool) {
        line += if checked { " 已勾" } else { " 没勾" };
    }
    if let Some(options) = item.get("options").and_then(Value::as_array) {
        let options: Vec<&str> = options.iter().filter_map(Value::as_str).collect();
        line += &format!(" 选项：{}", options.join(" / "));
    }
    let href = field(item, "href");
    if !href.is_empty() && !href.starts_with("javascript:") {
        line += &format!(" → {href}");
    }
    if item.get("disabled").and_then(Value::as_bool) == Some(true) {
        line += "（不可用）";
    }
    if item.get("inView").and_then(Value::as_bool) != Some(true) {
        line += "（不在视口里）";
    }
    line
}

fn remember(tab: &str, elements: &[Value]) {
    let map = elements
        .iter()
        .map(|item| {
            let element = Element {
                kind: field(item, "type").into(),
                name: field(item, "name").into(),
                autocomplete: field(item, "autocomplete").into(),
                field: field(item, "field").into(),
            };
            (field(item, "ref").to_string(), element)
        })
        .collect();
    ELEMENTS.lock().unwrap().insert(tab.to_string(), map);
}

// MARK: - 标签、等待

/// 工具要操作的标签：tab 参数（编号前几位、标题里的字、第几个）或当前标签；没有标签时 create 为真就开一个空白的
fn resolve(session: &str, arguments: &Map<String, Value>, create: bool) -> Result<String, String> {
    let tabs = browser::tabs_of(session);
    if let Some(wanted) = text_arg(arguments, "tab") {
        if let Ok(index) = wanted.parse::<usize>()
            && (1..=tabs.len()).contains(&index)
        {
            return Ok(tabs[index - 1].id.clone());
        }
        let lower = wanted.to_lowercase();
        return tabs
            .iter()
            .find(|tab| tab.id.to_lowercase().starts_with(&lower) || tab.title.to_lowercase().contains(&lower))
            .map(|tab| tab.id.clone())
            .ok_or_else(|| format!("没有叫「{wanted}」的标签；用 browser_tabs 看这段对话的标签"));
    }
    if let Some(current) = browser::current_tab(session) {
        browser::wake_now(&current)?;
        return Ok(current);
    }
    if !create {
        return Err("这段对话还没有打开网页：先用 browser_navigate 打开".into());
    }
    browser::open_for_agent(session, "")
}

/// 等页面加载完再稳一会儿（最多 limit；加载慢的不算错，结果里说一声）
fn wait_for_load(tab: &str, limit: Duration) -> bool {
    let start = Instant::now();
    std::thread::sleep(Duration::from_millis(150));
    let mut quiet = 0;
    while start.elapsed() < limit {
        let Some(info) = browser::info(tab) else { return true };
        quiet = if info.loading { 0 } else { quiet + 1 };
        if quiet >= 4 {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// 用户在卡片里动着：等一会儿（最多 10 秒），还在动就让模型稍后再来
fn yield_to_user(tab: &str) -> Result<(), String> {
    let start = Instant::now();
    while browser::user_active(tab) {
        if start.elapsed() > Duration::from_secs(10) {
            return Err("用户正在操作浏览器，过一会儿再试（或者先问问用户在做什么）".into());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Ok(())
}

/// 动作之后附上的现状：网址、标题、焦点、没回答的对话框、刚下载完的文件
fn after_action(tab: &str, url_before: &str) -> String {
    let mut text = String::new();
    if let Ok(state) = browser::evaluate(tab, STATE_SCRIPT, true) {
        let url = field(&state, "url");
        if !url_before.is_empty() && url != url_before {
            text += &format!("\n网址变了：{url}");
        }
        text += &format!("\n标题：{}", field(&state, "title"));
        if !field(&state, "focus").is_empty() {
            text += &format!("\n焦点在：{}", field(&state, "focus"));
        }
    }
    let dialog = browser::pending_dialog(tab);
    if !dialog.is_empty() {
        text += &format!("\n网页弹了对话框，还没回答：{dialog}（用 browser_act 的 dialog 回答）");
    }
    for path in browser::take_downloads(tab) {
        text += &format!("\n下载完了：{path}");
    }
    text
}

/// 动作的目标：ref（先滚到它）或 x、y
fn target(tab: &str, arguments: &Map<String, Value>, required: bool) -> Result<Option<(f64, f64, String)>, String> {
    if let Some(reference) = text_arg(arguments, "ref") {
        let located = browser::evaluate(tab, &locate_script(&reference), true)?;
        if located.get("missing").and_then(Value::as_bool) == Some(true) {
            return Err(format!("找不到元素 {reference}（页面变了）：先用 browser_read 重新看元素"));
        }
        std::thread::sleep(Duration::from_millis(120));
        let mut what = format!("[{reference}]");
        if !field(&located, "name").is_empty() {
            what += &format!("「{}」", field(&located, "name"));
        }
        if located.get("covered").and_then(Value::as_bool) == Some(true) {
            what += "（它被别的东西盖着，点到的可能是盖在上面的那个：弹窗、遮罩先关掉）";
        }
        return Ok(Some((located["x"].as_f64().unwrap_or(0.0), located["y"].as_f64().unwrap_or(0.0), what)));
    }
    if let (Some(x), Some(y)) = (number_arg(arguments, "x"), number_arg(arguments, "y")) {
        return Ok(Some((x, y, format!("({}, {})", x as i64, y as i64))));
    }
    if required {
        return Err("要给 ref（browser_read elements 列出的编号）或 x、y（视口里的坐标）".into());
    }
    Ok(None)
}

// MARK: - 各个工具

fn navigate(session: &str, arguments: &Map<String, Value>) -> Result<String, String> {
    let url = text_arg(arguments, "url");
    let action = text_arg(arguments, "action");
    let keep = bool_arg(arguments, "keep", false);
    if url.is_none() && action.is_none() {
        // 只给 keep：把已经开着的标签留给用户（这一轮结束时不关）
        if keep {
            let tab = resolve(session, arguments, false)?;
            browser::keep(&tab);
            return Ok(format!("标签 {} 留给用户，这一轮结束时不关。", short(&tab)));
        }
        return Err("要给 url（要打开的网址），或 action（back / forward / reload）".into());
    }
    let tab = if let (Some(url), true) = (&url, bool_arg(arguments, "new_tab", false)) {
        browser::open_for_agent(session, url)?
    } else {
        let tab = resolve(session, arguments, true)?;
        yield_to_user(&tab)?;
        match (&url, &action) {
            (Some(url), _) => browser::load(&tab, url),
            (None, Some(action)) => browser::navigate_action(&tab, &action.to_lowercase())?,
            _ => {}
        }
        tab
    };
    let loaded = wait_for_load(&tab, Duration::from_secs(30));
    let page = browser::evaluate(&tab, TEXT_SCRIPT, true).unwrap_or(Value::Null);
    let url_now = if field(&page, "url").is_empty() {
        browser::info(&tab).map(|info| info.url).unwrap_or_default()
    } else {
        field(&page, "url").to_string()
    };
    let mut text = format!("标签 {}：{}\n网址：{url_now}", short(&tab), field(&page, "title"));
    if !loaded {
        text += "\n（30 秒还没加载完，下面是现在的样子）";
    }
    let body = field(&page, "text").trim();
    text += &format!("\n\n页面开头的文字：\n{}", if body.is_empty() { "（没有文字）".to_string() } else { clip(body, 2000) });
    text += "\n\n要点、要填就先用 browser_read（mode=elements）看可交互的元素和编号。";
    if keep {
        browser::keep(&tab);
    }
    if browser::closes_at_turn_end(session, &tab) {
        text += "\n这个标签是你开的，这一轮结束时自动关掉；用户要看、要接着用的带上 keep=true（只给 tab 和 keep 也行）。";
    }
    let after = after_action(&tab, "");
    // 标题上面已经有了
    let after: Vec<&str> = after.lines().filter(|line| !line.starts_with("标题：")).collect();
    text += &after.join("\n");
    Ok(text)
}

fn screens(scroll: i64, height: i64, viewport: i64) -> (i64, i64) {
    let viewport = viewport.max(1);
    (scroll / viewport + 1, ((height + viewport - 1) / viewport).max(1))
}

fn read(session: &str, arguments: &Map<String, Value>) -> Result<String, String> {
    let tab = resolve(session, arguments, false)?;
    wait_for_load(&tab, Duration::from_secs(8));
    let mode = text_arg(arguments, "mode").unwrap_or_else(|| "text".into()).to_lowercase();
    let number = |value: &Value, key: &str| value.get(key).and_then(Value::as_i64).unwrap_or(0);
    if mode == "elements" {
        let object = browser::evaluate(&tab, &elements_script(), true)?;
        let elements = object["elements"].as_array().cloned().unwrap_or_default();
        remember(&tab, &elements);
        let (screen, total_screens) =
            screens(number(&object, "scrollY"), number(&object, "scrollHeight"), number(&object, "viewportHeight"));
        let mut text = format!(
            "标签 {}：{}\n网址：{}\n视口 {}×{}，滚到第 {screen} 屏（共 {total_screens} 屏）\n",
            short(&tab),
            field(&object, "title"),
            field(&object, "url"),
            number(&object, "viewportWidth"),
            number(&object, "viewportHeight")
        );
        let total = number(&object, "total") as usize;
        let partial = if total > elements.len() { format!("（只列前 {} 个）", elements.len()) } else { String::new() };
        text += &format!("可交互的元素 {total} 个{partial}：\n");
        let lines: Vec<String> = elements.iter().map(describe).collect();
        text += &if lines.is_empty() { "（没有）".to_string() } else { lines.join("\n") };
        text += "\n\n用 browser_act 的 ref 填编号来点、输入；编号在这个页面里一直有效，页面换了再读一次。";
        let dialog = browser::pending_dialog(&tab);
        if !dialog.is_empty() {
            text += &format!("\n网页弹了对话框，还没回答：{dialog}");
        }
        return Ok(clip(&text, MAX_TEXT));
    }
    let object = browser::evaluate(&tab, TEXT_SCRIPT, true)?;
    let (screen, total_screens) = screens(number(&object, "scrollY"), number(&object, "scrollHeight"), number(&object, "viewportHeight"));
    let mut text = format!(
        "标签 {}：{}\n网址：{}\n页面共 {total_screens} 屏，现在在第 {screen} 屏\n\n",
        short(&tab),
        field(&object, "title"),
        field(&object, "url")
    );
    let body = field(&object, "text").trim();
    text += if body.is_empty() { "（页面上没有文字）" } else { body };
    Ok(clip(&text, MAX_TEXT))
}

fn act(session: &str, arguments: &Map<String, Value>) -> Result<String, String> {
    let action = text_arg(arguments, "action").unwrap_or_default().to_lowercase();
    if action.is_empty() {
        return Err("要给 action：click / double_click / right_click / type / key / scroll / hover / select / dialog".into());
    }
    let tab = resolve(session, arguments, false)?;
    if action == "dialog" {
        let accept = bool_arg(arguments, "accept", true);
        let text = arguments.get("text").and_then(Value::as_str).unwrap_or("");
        if !browser::answer_dialog(&tab, accept, text)? {
            return Err("这个标签没有等着回答的对话框".into());
        }
        wait_for_load(&tab, Duration::from_secs(5));
        return Ok(format!("{}{}", if accept { "已确认对话框" } else { "已取消对话框" }, after_action(&tab, "")));
    }
    yield_to_user(&tab)?;
    let url_before = browser::info(&tab).map(|info| info.url).unwrap_or_default();
    let done = match action.as_str() {
        "click" | "double_click" | "right_click" => {
            let (x, y, what) = target(&tab, arguments, true)?.unwrap_or_default();
            let button = if action == "right_click" { "right" } else { "left" };
            browser::click(&tab, x, y, button, if action == "double_click" { 2 } else { 1 })?;
            let verb = match action.as_str() {
                "click" => "点击",
                "double_click" => "双击",
                _ => "右键点了",
            };
            format!("已{verb} {what}")
        }
        "type" => {
            let text = arguments.get("text").and_then(Value::as_str).ok_or("type 要给 text")?;
            let point = target(&tab, arguments, false)?;
            if let Some((x, y, _)) = &point {
                browser::click(&tab, *x, *y, "left", 1)?;
                std::thread::sleep(Duration::from_millis(80));
            }
            if !bool_arg(arguments, "append", false) {
                browser::press_key(&tab, "Ctrl+A")?;
                browser::press_key(&tab, "Backspace")?;
            }
            browser::insert_text(&tab, text)?;
            let place = point.map(|(_, _, what)| what).unwrap_or_else(|| "焦点处".into());
            let mut done = format!("已在 {place} 输入 {} 个字", text.chars().count());
            if bool_arg(arguments, "submit", false) {
                std::thread::sleep(Duration::from_millis(80));
                browser::press_key(&tab, "Enter")?;
                done += "，按了回车";
            }
            done
        }
        "key" => {
            let key = text_arg(arguments, "key").ok_or("key 要给按键，如 Enter、Tab、Escape、ArrowDown、Ctrl+A")?;
            if !browser::press_key(&tab, &key)? {
                return Err(format!("认不出按键「{key}」"));
            }
            format!("已按 {key}")
        }
        "scroll" => {
            let direction = text_arg(arguments, "direction").unwrap_or_else(|| "down".into()).to_lowercase();
            let amount = number_arg(arguments, "amount").unwrap_or(5.0).clamp(1.0, 50.0) as i64;
            if text_arg(arguments, "ref").is_some() {
                let (_, _, what) = target(&tab, arguments, true)?.unwrap_or_default();
                format!("已滚到 {what}")
            } else {
                let (width, height) = browser::viewport_of(&tab);
                let (x, y) = target(&tab, arguments, false)?.map(|(x, y, _)| (x, y)).unwrap_or((width as f64 / 2.0, height as f64 / 2.0));
                // CDP 的滚轮向下、向右为正
                let step = 120.0 * amount as f64;
                let delta = match direction.as_str() {
                    "up" => (0.0, -step),
                    "left" => (-step, 0.0),
                    "right" => (step, 0.0),
                    _ => (0.0, step),
                };
                browser::mouse(&tab, "mouseWheel", x, y, "none", 0, delta)?;
                let word = match direction.as_str() {
                    "up" => "上",
                    "left" => "左",
                    "right" => "右",
                    _ => "下",
                };
                format!("已往{word}滚了 {amount} 格")
            }
        }
        "hover" => {
            let (x, y, what) = target(&tab, arguments, true)?.unwrap_or_default();
            browser::mouse(&tab, "mouseMoved", x, y, "none", 0, (0.0, 0.0))?;
            format!("鼠标移到了 {what}")
        }
        "select" => {
            let (Some(reference), Some(option)) = (text_arg(arguments, "ref"), arguments.get("text").and_then(Value::as_str)) else {
                return Err("select 要给 ref（下拉框）和 text（选项的文字或值）".into());
            };
            let result = browser::evaluate(&tab, &select_script(&reference, option), true)?;
            if result.get("missing").and_then(Value::as_bool) == Some(true) {
                return Err(format!("找不到元素 {reference}：先用 browser_read 重新看元素"));
            }
            match field(&result, "error") {
                "not-select" => return Err(format!("{reference} 不是下拉框（<select>）：自绘的下拉框先 click 打开再点选项")),
                "no-option" => {
                    let options: Vec<&str> = result["options"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
                    return Err(format!("没有「{option}」这一项；有：{}", options.join(" / ")));
                }
                _ => {}
            }
            format!("已在 [{reference}] 选了「{}」", field(&result, "selected"))
        }
        other => return Err(format!("不认识的 action「{other}」")),
    };
    // 点击、回车可能引起导航：稍等再看
    std::thread::sleep(Duration::from_millis(300));
    wait_for_load(&tab, Duration::from_secs(10));
    Ok(done + &after_action(&tab, &url_before))
}

fn screenshot(session: &str, arguments: &Map<String, Value>) -> Result<String, String> {
    let tab = resolve(session, arguments, false)?;
    wait_for_load(&tab, Duration::from_secs(8));
    std::thread::sleep(Duration::from_millis(200));
    let bytes = browser::screenshot(&tab)?;
    let dir = crate::home::project_dir().join("browser").join("shots");
    std::fs::create_dir_all(&dir).map_err(|error| format!("存不了截图：{error}"))?;
    let path = dir.join(format!("{}-{}.jpg", short(&tab), chrono::Local::now().format("%Y%m%d-%H%M%S%3f")));
    std::fs::write(&path, &bytes).map_err(|error| format!("存不了截图：{error}"))?;
    let (width, height) = browser::viewport_of(&tab);
    let title = browser::info(&tab).map(|info| info.title).unwrap_or_default();
    let mut text =
        format!("截了标签 {}「{title}」的视口（{width}×{height}，坐标就是 browser_act 的 x、y），存在 {}。", short(&tab), path.display());
    crate::tools::note_image(&path.to_string_lossy());
    text += "截图跟在后面（这个模型看不了图的话只有这句：用 browser_read 读页面文字和元素）。";
    Ok(text)
}

fn resize(session: &str, arguments: &Map<String, Value>) -> Result<String, String> {
    let tab = resolve(session, arguments, true)?;
    if bool_arg(arguments, "fit", false) {
        browser::set_fixed(&tab, None)?;
        std::thread::sleep(Duration::from_millis(300));
        let (width, height) = browser::viewport_of(&tab);
        return Ok(format!("视口改回跟随卡片，现在是 {width}×{height}"));
    }
    let (Some(width), Some(height)) = (number_arg(arguments, "width"), number_arg(arguments, "height")) else {
        return Err("要给 width、height（CSS 像素），或 fit=true 回到跟随卡片".into());
    };
    let size = ((width as u32).clamp(320, 3840), (height as u32).clamp(240, 2160));
    browser::set_fixed(&tab, Some(size))?;
    std::thread::sleep(Duration::from_millis(300));
    Ok(format!("视口改成了 {}×{}（网页按这个大小排版；卡片大小不变，卡片里按比例缩小显示）", size.0, size.1))
}

fn tabs(session: &str, arguments: &Map<String, Value>) -> Result<String, String> {
    let action = text_arg(arguments, "action").unwrap_or_else(|| "list".into()).to_lowercase();
    match action.as_str() {
        "select" | "close" => {
            if text_arg(arguments, "tab").is_none() {
                return Err(format!("{action} 要给 tab（编号、标题里的字或第几个）"));
            }
            let tab = resolve(session, arguments, false)?;
            if action == "select" {
                browser::select(&tab)?;
            } else {
                browser::close_tab(&tab)?;
            }
        }
        "list" => {}
        _ => return Err("action 是 list / select / close（开新标签用 browser_navigate 的 new_tab）".into()),
    }
    let list = browser::tabs_of(session);
    if list.is_empty() {
        return Ok("这段对话没有打开的网页".into());
    }
    let lines: Vec<String> = list
        .iter()
        .enumerate()
        .map(|(index, tab)| {
            format!("{}. {}「{}」{}{}", index + 1, short(&tab.id), tab.title, tab.url, if tab.current { "（当前）" } else { "" })
        })
        .collect();
    Ok(format!("这段对话的标签：\n{}", lines.join("\n")))
}

fn hand_over(session: &str, arguments: &Map<String, Value>) -> Result<String, String> {
    let reason = text_arg(arguments, "reason").ok_or("要给 reason：请用户在浏览器里做什么（例如「登录 GitHub」「完成人机验证」）")?;
    let tab = resolve(session, arguments, false)?;
    browser::hand_over(&tab, &reason)?;
    Ok(format!("已打开浏览器卡片、请用户「{reason}」。现在结束这一轮、告诉用户要做什么；用户点「好了」后会发来消息，再接着做。"))
}

fn run_js(session: &str, arguments: &Map<String, Value>) -> Result<String, String> {
    let script = arguments.get("script").and_then(Value::as_str).filter(|script| !script.trim().is_empty()).ok_or("要给 script")?;
    let tab = resolve(session, arguments, false)?;
    yield_to_user(&tab)?;
    let value = browser::evaluate(&tab, script, false)?;
    let text = match &value {
        Value::Null => "（脚本没有返回值）".to_string(),
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    Ok(clip(&text, MAX_TEXT) + &after_action(&tab, ""))
}

/// 跑一个浏览器工具（不是浏览器工具回 None）
pub fn run(name: &str, arguments: &Map<String, Value>, session: &str) -> Option<Outcome> {
    if !is_tool(name) {
        return None;
    }
    if !browser::available() {
        return Some(Outcome {
            ok: false, text: "这台机器上没有 Chrome / Chromium / Edge，用不了浏览器工具（看网页用 fetch_url）".into()
        });
    }
    if session.is_empty() {
        return Some(Outcome { ok: false, text: "这次调用不属于任何一段会话，没有浏览器可用".into() });
    }
    let outcome = match name {
        "browser_navigate" => navigate(session, arguments),
        "browser_read" => read(session, arguments),
        "browser_act" => act(session, arguments),
        "browser_screenshot" => screenshot(session, arguments),
        "browser_resize" => resize(session, arguments),
        "browser_tabs" => tabs(session, arguments),
        "browser_hand_over" => hand_over(session, arguments),
        _ => run_js(session, arguments),
    };
    Some(match outcome {
        Ok(text) => Outcome { ok: true, text },
        Err(text) => Outcome { ok: false, text },
    })
}

/// 审批：这一下点 / 输入的是付款、登录、密码、卡号这类（同 Friday 的 stopFor）——什么模式下都要用户确认
pub fn needs_stop(arguments: &Map<String, Value>, session: &str) -> Option<&'static str> {
    let action = text_arg(arguments, "action").unwrap_or_default().to_lowercase();
    let reference = text_arg(arguments, "ref")?;
    let tab = match text_arg(arguments, "tab") {
        Some(wanted) => {
            let lower = wanted.to_lowercase();
            browser::tabs_of(session)
                .into_iter()
                .find(|tab| tab.id.to_lowercase().starts_with(&lower) || tab.title.to_lowercase().contains(&lower))?
                .id
        }
        None => browser::current_tab(session)?,
    };
    let element = ELEMENTS.lock().unwrap().get(&tab)?.get(&reference)?.clone();
    let has = |text: &str, words: &[&str]| {
        let lower = text.to_lowercase();
        words.iter().any(|word| lower.contains(word))
    };
    let money = [
        "支付",
        "付款",
        "付钱",
        "下单",
        "购买",
        "结算",
        "立即购买",
        "确认订单",
        "充值",
        "pay",
        "checkout",
        "place order",
        "buy now",
        "purchase",
        "subscribe",
    ];
    let account = ["登录", "登入", "注册", "sign in", "log in", "login", "sign up", "register", "create account"];
    let card = ["cc-", "card", "卡号", "cvv", "cvc"];
    match action.as_str() {
        "type" if element.kind == "password" => Some("在密码框里输入"),
        "type" if has(&format!("{} {} {}", element.autocomplete, element.field, element.name), &card) => Some("填卡号这类付款信息"),
        "click" | "double_click" if has(&element.name, &money) => Some("点付款、下单这类按钮"),
        "click" | "double_click" if has(&element.name, &account) => Some("点登录、注册这类按钮"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_elements_like_friday() {
        let item =
            json!({ "ref": "e3", "tag": "input", "type": "password", "name": "密码", "placeholder": "", "value": "••••", "inView": true });
        assert_eq!(describe(&item), "[e3] 密码框「密码」 现在是「••••」");
        let link = json!({ "ref": "e1", "tag": "a", "name": "首页", "href": "/home", "inView": false });
        assert_eq!(describe(&link), "[e1] 链接「首页」 → /home（不在视口里）");
        assert_eq!(screens(1600, 4000, 800), (3, 5));
        assert!(elements_script().contains("(300)"));
        assert!(locate_script("e\"1").contains(r#""e\"1""#));
    }
}
