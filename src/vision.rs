//! 能不能看图用到时才确认（同 Friday 2026-11-26 的 VisionProbe / VisionProbes）：没定的在线模型先当能看，第一次带图发出去时——
//! - 接口说不收图（chat_request::refused 的 `vision: false`）：refused 记看不了（brain::refuse_images 把图换成说明重发）；
//! - 接口收了：confirm 在后台发一张写着随机四位数的小图核对一次（个别兼容接口会把图悄悄丢掉，模型只好瞎猜），读对记能看、读不对记看不了；
//! - 被限流：在一次核对的时限（TIMEOUT）里等一会儿重发（同 Friday：免费模型常回 429，一次就放弃的话多半测不出来）；
//! - 没测出来（额度、到时限还在限流、连不上）：不改，10 分钟内再用到也不再测。
//!
//! 结果记在这个进程里，也写回 config.json（home::remember_vision：supportsVision、visionChecked「接口地址|模型名」）。
//! 一轮做完时 brain::run 等核对的结果（settle，最多 60 秒），核对出看不了就把这一轮的图先用看图能力描述成文字、按 vision.dropped 接着做

use std::collections::{HashMap, HashSet};
use std::sync::{Condvar, LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use friday_core::chat_stream::{Parser, StreamError};
use friday_core::engine_loop::{self, Loop, Settings};
use friday_core::quota::{self, Limit};
use friday_core::{chat_request, responses};
use serde_json::{Value, json};

use crate::home::{self, Model};
use crate::http::{self, Failure};

/// 让模型读数字的提示（中英文都给：有的模型只认一种；同 Friday）
pub const PROMPT: &str =
    "图片里写着一个四位数，只回答这个数字，不要别的。What 4-digit number is written in the image? Reply with the digits only.";
/// 纯文字对照：接口拒收图片时用它确认模型本身能用
const CONTROL_PROMPT: &str = "只回答 OK。Reply with OK only.";
/// 一次核对最多等多久（推理模型关不掉思考时要多想一会儿；被限流时等着重发也算在里面）
const TIMEOUT: Duration = Duration::from_secs(120);
/// 没测出来的，隔多久再用到才再测
const RETRY_AFTER: Duration = Duration::from_secs(600);
/// 一轮做完时最多等核对多久（核对和这一轮是同时发的，多半早就有了）
pub const SETTLE: Duration = Duration::from_secs(60);
/// 服务繁忙（不算接口拒收图）
const BUSY_STATUSES: [u16; 5] = [500, 502, 503, 504, 529];

#[derive(Default)]
struct State {
    /// 正在核对的（引擎 id）
    running: HashSet<String>,
    /// 没测出来的：到这个时刻之前不再测
    failures: HashMap<String, Instant>,
    /// 这个进程里确认过的（接口拒收、核对的结果）：盖过读进来的 supportsVision
    verdicts: HashMap<String, bool>,
}

static STATE: LazyLock<(Mutex<State>, Condvar)> = LazyLock::new(|| (Mutex::new(State::default()), Condvar::new()));

fn state() -> MutexGuard<'static, State> {
    STATE.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 这个模型现在能不能看图：这个进程里确认过的照确认的，没确认的照读进来的（没定的在线模型读进来就当能看，friday-core engines.rs）
pub fn sees(model: &Model) -> bool {
    state().verdicts.get(&model.engine_id()).copied().unwrap_or(model.supports_vision)
}

/// 这个进程里确认过的结果（没确认是 None）
pub fn verdict(model: &Model) -> Option<bool> {
    state().verdicts.get(&model.engine_id()).copied()
}

/// 要不要核对：在线的（http / https；本地大模型按模型文件定、Claude Code 这类交给别的智能体的不测）、还没定
pub fn needs_probe(model: &Model) -> bool {
    let online = model.base_url.starts_with("http://") || model.base_url.starts_with("https://");
    online && !model.vision_settled && !state().verdicts.contains_key(&model.engine_id())
}

/// 记下确认的结果（这个进程里 + config.json），叫醒等着的
fn settle_verdict(model: &Model, sees: bool) {
    {
        let mut state = state();
        state.verdicts.insert(model.engine_id(), sees);
        state.failures.remove(&model.engine_id());
    }
    let _ = home::remember_vision(&model.base_url, &model.model, sees);
    STATE.1.notify_all();
}

/// 接口说不收图：记看不了
pub fn refused(model: &Model) {
    settle_verdict(model, false);
}

/// 带图的请求接口收了、这个模型还没确认：在后台发一张小图核对一次，不挡正在做的事（正在核对、10 分钟内没测出来过的不再发）
pub fn confirm(model: &Model) {
    if !needs_probe(model) {
        return;
    }
    let id = model.engine_id();
    {
        let mut state = state();
        if state.running.contains(&id) || state.failures.get(&id).is_some_and(|until| *until > Instant::now()) {
            return;
        }
        state.running.insert(id.clone());
    }
    let model = model.clone();
    std::thread::spawn(move || {
        let verdict = run(&model, &random_code());
        match verdict {
            Verdict::Vision => settle_verdict(&model, true),
            Verdict::TextOnly(_) => settle_verdict(&model, false),
            Verdict::Unknown(_) => {
                state().failures.insert(id.clone(), Instant::now() + RETRY_AFTER);
            }
        }
        state().running.remove(&id);
        STATE.1.notify_all();
    });
}

/// 等正在核对的结果：没在核对就马上返回，最多等 timeout
pub fn settle(model: &Model, timeout: Duration) {
    let id = model.engine_id();
    let until = Instant::now() + timeout;
    let mut state = state();
    while state.running.contains(&id) {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        state = STATE.1.wait_timeout(state, left).unwrap_or_else(|poisoned| poisoned.into_inner()).0;
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Vision,
    TextOnly(String),
    Unknown(String),
}

/// 核对一次：读对了（差一位也算）能看图；回了别的（接口悄悄丢了图）看不了；接口拒收图——再发一句同样的纯文字，
/// 纯文字能过就是不收图 = 看不了，纯文字也不过就是这个模型本身用不了、算没测出来；额度、限流、连不上：没测出来
pub fn run(model: &Model, code: &str) -> Verdict {
    let url = format!("data:image/png;base64,{}", base64_of(&png(code)));
    match ask(model, PROMPT, Some(&url)) {
        Ok(reply) if reads_code(&reply, code) => Verdict::Vision,
        Ok(reply) => {
            let shown: String = strip_think(&reply).trim().chars().take(80).collect();
            Verdict::TextOnly(format!("没读出图里的数字 {code}（回的是「{shown}」）"))
        }
        Err(Asked::Refused(reason)) => Verdict::TextOnly(reason),
        Err(Asked::Status { status, body })
            if (400..600).contains(&status) && !BUSY_STATUSES.contains(&status) && ![401, 403, 429].contains(&status) =>
        {
            match ask(model, CONTROL_PROMPT, None) {
                Ok(_) => Verdict::TextOnly(format!("接口不收图片（HTTP {status}：{}）", body.chars().take(200).collect::<String>())),
                Err(error) => Verdict::Unknown(format!("模型本身用不了：{}", error.message())),
            }
        }
        Err(error) => Verdict::Unknown(error.message()),
    }
}

enum Asked {
    /// 接口说不收图
    Refused(String),
    Status {
        status: u16,
        body: String,
    },
    Other(String),
}

impl Asked {
    fn message(&self) -> String {
        match self {
            Asked::Refused(reason) | Asked::Other(reason) => reason.clone(),
            Asked::Status { status, body } => format!("HTTP {status}：{}", body.chars().take(300).collect::<String>()),
        }
    }
}

/// 发一次（不带工具、关思考、温度 0）：请求体照这个引擎的规则拼（思考参数、记下的「不接受的参数」）；
/// 接口说不接受某个参数、思考关不掉的按 chat_request::refused 改了重发；被限流（不是额度、余额用完）的在 TIMEOUT 里等一会儿重发
fn ask(model: &Model, prompt: &str, image: Option<&str>) -> Result<String, Asked> {
    let engine_id = model.engine_id();
    let config = engine_loop::Config {
        engine_id: engine_id.clone(),
        model: model.model.clone(),
        base_url: model.base_url.clone(),
        supports_vision: true,
        supports_tools: false,
        thinking: model.thinking.clone(),
        extra_body: model.extra_body.clone(),
    };
    let mut brain = Loop::new(config, Vec::new());
    brain.begin(Settings {
        temperature: 0.0,
        enable_thinking: false,
        max_tool_rounds: 1,
        continues_when_truncated: false,
        ..Settings::default()
    });
    let content = match image {
        Some(url) => json!([{ "type": "image_url", "image_url": { "url": url } }, { "type": "text", "text": prompt }]),
        None => json!(prompt),
    };
    brain.append(vec![json!({ "role": "user", "content": content })]);
    let mut last = Asked::Other("没发出去".into());
    let deadline = Instant::now() + TIMEOUT;
    let (mut adjusted, mut rate_limits) = (0, 0);
    while adjusted < 3 {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let use_responses = chat_request::uses_responses(&engine_id);
        let mut body = brain.body(false, None, false);
        if use_responses {
            body = responses::body(body, &engine_id);
        }
        let url = format!("{}/{}", model.base_url.trim_end_matches('/'), if use_responses { "responses" } else { "chat/completions" });
        let mut parser = Parser::for_engine(false, true, &engine_id);
        let mut failed: Option<String> = None;
        let mut headers = Vec::new();
        let outcome = http::post_sse_within(&url, &model.api_key, &body, &mut headers, Some(left), |data| {
            let Ok(chunk) = serde_json::from_str::<Value>(data) else { return true };
            let feed = parser.feed(&chunk, chrono::Utc::now());
            if let Some(error) = feed.error {
                failed = Some(match error {
                    StreamError::RateLimit { message } | StreamError::Quota { message, .. } | StreamError::Failed { message } => message,
                });
                return false;
            }
            !feed.done
        });
        match outcome {
            Ok(_) => {
                if let Some(message) = failed {
                    return Err(Asked::Other(message));
                }
                let result = parser.result();
                let content = result["content"].as_str().unwrap_or("").to_string();
                return Ok(content);
            }
            Err(Failure::Network(message)) => return Err(Asked::Other(format!("网络出错：{message}"))),
            Err(Failure::Status { status, body: reply, retry_after }) => {
                if let Some(fixed) = chat_request::refused(i64::from(status), &reply, &body, &engine_id) {
                    if fixed["vision"] == json!(false) {
                        return Err(Asked::Refused(format!(
                            "接口不收图片（HTTP {status}：{}）",
                            reply.chars().take(200).collect::<String>()
                        )));
                    }
                    // 改了什么记在引擎上，下一次拼的请求体照它
                    last = Asked::Status { status, body: reply };
                    adjusted += 1;
                    continue;
                }
                // 2026-11-26 被限流（239 上实测：智谱免费模型的核对第一次就回 429 1305，以前直接算没测出来、丢图认不出来）：
                // 同 Friday 的核对，时限里等一会儿重发
                if matches!(quota::classify(i64::from(status), &reply), Some(Limit::RateLimit)) {
                    rate_limits += 1;
                    let wait = Duration::from_secs_f64(chat_request::rate_limit_delay(rate_limits, retry_after, 0));
                    if Instant::now() + wait < deadline {
                        std::thread::sleep(wait);
                        last = Asked::Status { status, body: reply };
                        continue;
                    }
                }
                return Err(Asked::Status { status, body: reply });
            }
        }
    }
    Err(last)
}

fn strip_think(text: &str) -> String {
    static THINK: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"(?s)<think>.*?</think>").unwrap());
    THINK.replace_all(text, "").into_owned()
}

/// 模型的回复里有没有这个数：去掉思考，取每段连续数字和全部数字连起来（「4 7 2 9」），长度相同、最多差一位就算读对（同 Friday）
pub fn reads_code(reply: &str, code: &str) -> bool {
    let visible = strip_think(reply);
    // 全角数字也认
    let normalized: String =
        visible.chars().map(|c| if ('０'..='９').contains(&c) { char::from(b'0' + (c as u32 - '０' as u32) as u8) } else { c }).collect();
    let mut candidates: Vec<String> =
        normalized.split(|c: char| !c.is_ascii_digit()).filter(|part| !part.is_empty()).map(str::to_string).collect();
    let all: String = normalized.chars().filter(char::is_ascii_digit).collect();
    if all.len() <= 6 {
        candidates.push(all);
    }
    candidates
        .iter()
        .any(|candidate| candidate.len() == code.len() && candidate.chars().zip(code.chars()).filter(|(a, b)| a != b).count() <= 1)
}

/// 随机四位数：首位不是 0，四位不全一样
pub fn random_code() -> String {
    loop {
        let bytes = uuid::Uuid::new_v4().into_bytes();
        let number = 1000 + u16::from_le_bytes([bytes[0], bytes[1]]) % 9000;
        let code = number.to_string();
        if code.chars().collect::<HashSet<_>>().len() > 1 {
            return code;
        }
    }
}

/// 5×7 点阵数字（不依赖字体，同 Friday 两端画的是同一张图）
const GLYPHS: [[&str; 7]; 10] = [
    ["01110", "10001", "10011", "10101", "11001", "10001", "01110"],
    ["00100", "01100", "00100", "00100", "00100", "00100", "01110"],
    ["01110", "10001", "00001", "00010", "00100", "01000", "11111"],
    ["11111", "00010", "00100", "00010", "00001", "10001", "01110"],
    ["00010", "00110", "01010", "10010", "11111", "00010", "00010"],
    ["11111", "10000", "11110", "00001", "00001", "10001", "01110"],
    ["00110", "01000", "10000", "11110", "10001", "10001", "01110"],
    ["11111", "00001", "00010", "00100", "01000", "01000", "01000"],
    ["01110", "10001", "10001", "01110", "10001", "10001", "01110"],
    ["01110", "10001", "10001", "01111", "00001", "00010", "01100"],
];
/// 一格多少像素、四周留几格
const SCALE: usize = 12;
const MARGIN: usize = 3;

/// 白底黑字的灰度像素（行优先）和宽高
pub fn pixels(code: &str) -> (usize, usize, Vec<u8>) {
    let digits: Vec<usize> = code.chars().filter_map(|c| c.to_digit(10)).map(|d| d as usize).collect();
    let columns = (digits.len() * 6).saturating_sub(1) + MARGIN * 2;
    let (width, height) = (columns * SCALE, (7 + MARGIN * 2) * SCALE);
    let mut bytes = vec![255u8; width * height];
    for y in 0..height {
        let Some(row) = (y / SCALE).checked_sub(MARGIN).filter(|row| *row < 7) else { continue };
        for x in 0..width {
            let Some(column) = (x / SCALE).checked_sub(MARGIN) else { continue };
            let (index, offset) = (column / 6, column % 6);
            if index < digits.len() && offset < 5 && GLYPHS[digits[index]][row].as_bytes()[offset] == b'1' {
                bytes[y * width + x] = 0;
            }
        }
    }
    (width, height, bytes)
}

/// 画成 PNG（8 位灰度，不压缩的 deflate 块：几十 KB，不用另加依赖）
pub fn png(code: &str) -> Vec<u8> {
    let (width, height, bytes) = pixels(code);
    let mut raw = Vec::with_capacity((width + 1) * height);
    for row in bytes.chunks(width) {
        raw.push(0);
        raw.extend_from_slice(row);
    }
    let mut zlib = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = raw.chunks(65535).collect();
    for (index, block) in blocks.iter().enumerate() {
        zlib.push(u8::from(index + 1 == blocks.len()));
        let length = block.len() as u16;
        zlib.extend_from_slice(&length.to_le_bytes());
        zlib.extend_from_slice(&(!length).to_le_bytes());
        zlib.extend_from_slice(block);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for byte in &raw {
        a = (a + u32::from(*byte)) % 65521;
        b = (b + a) % 65521;
    }
    zlib.extend_from_slice(&((b << 16) | a).to_be_bytes());
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut chunk = |kind: &[u8], data: &[u8]| {
        png.extend_from_slice(&(data.len() as u32).to_be_bytes());
        png.extend_from_slice(kind);
        png.extend_from_slice(data);
        png.extend_from_slice(&crc32(&[kind, data].concat()).to_be_bytes());
    };
    let mut header = Vec::new();
    header.extend_from_slice(&(width as u32).to_be_bytes());
    header.extend_from_slice(&(height as u32).to_be_bytes());
    header.extend_from_slice(&[8, 0, 0, 0, 0]);
    chunk(b"IHDR", &header);
    chunk(b"IDAT", &zlib);
    chunk(b"IEND", &[]);
    png
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for byte in data {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

fn base64_of(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_codes_like_friday() {
        assert!(reads_code("4729", "4729"));
        assert!(reads_code("图里是 4 7 2 9。", "4729"));
        assert!(reads_code("<think>是 1111 吗</think>４７２９", "4729"));
        assert!(reads_code("4723", "4729"), "差一位也算");
        assert!(!reads_code("4713", "4729"));
        assert!(!reads_code("我看不到图片", "4729"));
        assert!(!reads_code("<think>4729</think>看不到", "4729"), "思考里的不算");
        for _ in 0..50 {
            let code = random_code();
            assert_eq!(code.len(), 4);
            assert_ne!(code.as_bytes()[0], b'0');
            assert!(code.chars().collect::<HashSet<_>>().len() > 1, "{code}");
        }
    }

    #[test]
    fn draws_a_valid_png() {
        let (width, height, bytes) = pixels("4729");
        assert_eq!((width, height), (29 * 12, 13 * 12));
        // 「4」的第一行是 00010：第 4 格黑、第 1 格白
        let y = MARGIN * SCALE + SCALE / 2;
        assert_eq!(bytes[y * width + (MARGIN + 3) * SCALE + SCALE / 2], 0);
        assert_eq!(bytes[y * width + MARGIN * SCALE + SCALE / 2], 255);
        let png = png("4729");
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(&png[12..16], b"IHDR");
        assert!(png.ends_with(&[0, 0, 0, 0, b'I', b'E', b'N', b'D', 0xAE, 0x42, 0x60, 0x82]), "IEND 的 CRC 对");
    }

    fn model(online: bool, settled: bool) -> Model {
        Model {
            base_url: if online { "https://probe.example/v1".into() } else { "power://llm".into() },
            api_key: String::new(),
            model: format!("m-{online}-{settled}"),
            thinking: "none".into(),
            enable_thinking: false,
            temperature: 0.7,
            supports_vision: true,
            vision_settled: settled,
            extra_body: String::new(),
            secret_name: String::new(),
        }
    }

    /// 2026-11-26 239 上实测：智谱免费模型核对时第一次回 429 1305，以前直接算没测出来——现在等一会儿重发
    #[test]
    fn rate_limited_probe_retries() {
        use std::io::{BufRead, BufReader, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let mut served = 0;
            for reply in [
                "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nRetry-After: 0\r\nConnection: close\r\n\r\n{\"error\":{\"code\":\"1305\",\"message\":\"该模型当前访问量过大，请您稍后再试\"}}".to_string(),
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{}",
                    "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"4729\"}}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"
                ),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line.trim().is_empty() {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                stream.write_all(reply.as_bytes()).unwrap();
                served += 1;
            }
            served
        });
        let probe = Model { base_url: format!("http://127.0.0.1:{port}/v1"), model: "rate-limited".into(), ..model(true, false) };
        assert_eq!(run(&probe, "4729"), Verdict::Vision);
        assert_eq!(server.join().unwrap(), 2, "限流的那次之后重发了一次");
    }

    #[test]
    fn only_unsettled_online_models_are_probed() {
        assert!(needs_probe(&model(true, false)));
        assert!(!needs_probe(&model(true, true)));
        assert!(!needs_probe(&model(false, false)), "本地大模型不测");
        // 这个进程里确认过的不再测；看不看得到照确认的
        let blind = Model { model: "blind".into(), ..model(true, false) };
        state().verdicts.insert(blind.engine_id(), false);
        assert!(!needs_probe(&blind) && !sees(&blind));
        // 没在核对的不用等
        let started = Instant::now();
        settle(&model(true, false), Duration::from_secs(5));
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
