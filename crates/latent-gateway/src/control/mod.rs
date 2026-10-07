//! WebSocket 控制面(§4.12,上游 `gateway/protocol/*.md` + 
//! `packages/gateway-protocol/src/schema.ts` —— 帧格式逐字对齐):
//!
//! ```text
//! 文本帧 JSON。首帧必须是 connect,否则服务端立即断连。
//! 请求:  {"type":"req","id":1,"method":"chat.send","params":{…}}
//! 响应:  {"type":"res","id":1,"ok":true,"payload":{…}}  /  {"type":"res","id":1,"ok":false,"error":"…"}
//! 事件:  {"type":"event","event":"agent","payload":{…},"seq":42}
//! ```

pub mod auth;
pub mod client;
pub mod events;
pub mod methods;
pub mod server;
