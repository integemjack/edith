# WebSocket 能力（edith-websocket）

EDITH 大脑只认本机 stdio（[能力协议](CAPABILITY-PROTOCOL.md)）。要让别的机器、网页、手机连 EDITH，就接这个能力：
EDITH 经 stdio 起它，它对外开一个 WebSocket 端口，把远程客户端交来的事转给大脑、把干活的过程实时推给客户端。
它只讲能力协议、不用大脑的代码（[`capabilities/websocket/main.rs`](../capabilities/websocket/main.rs)），和 `edith` 一起打包发布。

## 接上

```bash
edith capabilities add ws edith-websocket --listen 127.0.0.1:7789
edith capabilities add ws edith-websocket --listen 0.0.0.0:7789 --token 一串随机的令牌
edith run
```

- `--listen 地址:端口`：默认 `127.0.0.1:7789`。
- `--token 令牌`（或环境变量 `EDITH_WEBSOCKET_TOKEN`）：**不是只听本机（127.0.0.1 / ::1 / localhost）时必须给**，不给不启动。
- 它对 EDITH 报的是：感知 `message`（远程客户端交来的话）、没有动作、订全部事件、所有会话。

## 连接

```
ws://<地址>:<端口>/?token=<令牌>&name=<客户端名字>
```

- 令牌放查询参数 `token`，或者请求头 `Authorization: Bearer <令牌>`；不对回 401。
- `name`：客户端的名字（不给是 `websocket-<序号>`），交来的话会标上「说话的是 <name>」。
- 每个 WebSocket 文本帧是一条 JSON-RPC 2.0 消息。

## 客户端能发的

| 方法 | 说明 |
|---|---|
| `ask`、`sense` | 同 [能力协议 §4.1–4.3](CAPABILITY-PROTOCOL.md#41-sense主动推内容请求或通知)；没写 `from` 的填上客户端名字，没写 `kind` 的填 `message` |
| `sessions.list` / `create` / `get` / `focus` / `stop` | 同 [§4.4](CAPABILITY-PROTOCOL.md#44-会话请求) |
| `ping` | 回 `{}` |
| `subscribe` | `{sessions?: [会话 id], types?: [事件类型]}`：只收这些会话、这些类型的事件；不给（或不是数组）是全部。连上时默认全部 |

请求原样转给 EDITH，回话原样还回来（`id` 换回客户端自己的）。别的方法回 `-32601`。

## 客户端收到的

- 请求的回话；
- `event` 通知：同 [能力协议 §7](CAPABILITY-PROTOCOL.md#7-事件event)（`{session, turn, type, data}`），按 `subscribe` 过滤。

## 例子（浏览器）

```js
const ws = new WebSocket("ws://127.0.0.1:7789/?name=browser");
ws.onopen = () => ws.send(JSON.stringify({ jsonrpc: "2.0", id: 1, method: "ask", params: { text: "列一下桌面上有什么" } }));
ws.onmessage = ({ data }) => {
  const message = JSON.parse(data);
  if (message.method === "event" && message.params.type === "text") console.log(message.params.data.text);
  if (message.id === 1) console.log("回答：", message.result.answer);
};
```
