//! ChatBridge 异步客户端（从旧框架插件原样移植，去掉框架依赖）。
//!
//! 协议参考: <https://github.com/TISUnion/ChatBridge>
//! - TCP 传输，4 字节长度前缀 + AES-CBC 加密 JSON
//! - 登录握手 → keep-alive → ChatPayload 收发
//!
//! # 线格式保真备注（与 Python 原实现 `src/chatroom_bridge/chatbridge.py` 逐行核对）
//!
//! - **长度前缀**：Python 发送 `self._writer.write(struct.pack("I", len(encrypted)) + encrypted)`，
//!   接收 `remaining = struct.unpack("I", header)[0]`。`"I"` 不带字节序前缀 → **本机原生
//!   字节序**（x86-64/ARM 上均为小端，实测 `struct.pack("I", 12) == 0c 00 00 00`）。
//!   README 所称"4 字节大端长度前缀"与代码不符，以 Python 代码为准，本实现用
//!   `to_ne_bytes` / `from_ne_bytes` 完全对齐。
//! - **填充**：PyCryptodome 不自动填充。Python `_to_16_length` 手工补 `\0` 至 16 的倍数：
//!   `pad = (16 - (len(data) % 16)) % 16` —— 明文恰为 16 的倍数时**不额外补块**，
//!   空明文保持为空（0 块）。
//! - **密钥/IV 派生**：`sha256(密码 UTF-8 编码后补 \0 至 16 的倍数)` 得 32 字节
//!   AES-256 密钥；IV 取该哈希的前 16 字节（`AES.new(key, MODE_CBC, self._hashed_key[:16])`）。
//! - **hex 编码**：密文经 `b2a_hex` 以小写 hex ASCII 上线；解密用 `a2b_hex`（大小写均可）。
//! - **空密钥直传**：key 为空时 encrypt/decrypt 原样返回字节；空密钥分支仅校验 UTF-8
//!   且**不做** `rstrip("\0")`（与 Python 两个分支的差异保持一致）。
//! - **解密收尾**：CBC 解密 → UTF-8 解码 → `rstrip("\0")` 去掉尾部空字节。

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use aes::Aes256;
use futures_util::future::{BoxFuture, FutureExt};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::sync::watch;
use tokio::sync::Mutex as TokioMutex;

pub const PACKET_TYPE_KEEP_ALIVE: &str = "chatbridge.keep_alive";
pub const PACKET_TYPE_CHAT: &str = "chatbridge.chat";
pub const SERVER_NAME: &str = "#SERVER";

pub const KEEP_ALIVE_INTERVAL: u64 = 60;
pub const KEEP_ALIVE_TIMEOUT: u64 = 15;
pub const RECONNECT_DELAY: u64 = 5;
pub const CONNECT_TIMEOUT: u64 = 20;

/// 聊天回调：`(sender, author, message)`，内联等待（对应 Python `Callable[[str, str, str], Any]`）。
pub type ChatCallback = Arc<dyn Fn(String, String, String) -> BoxFuture<'static, ()> + Send + Sync>;
/// 连接状态回调（on_connected / on_disconnected 共用）。
pub type StateCallback = Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>;

/// 与 ChatBridge 兼容的 AES-CBC（SHA256 派生密钥、零填充、hex 输出）。
pub struct AesCryptor {
    key_empty: bool,
    /// sha256(密码 UTF-8 后补 \0 至 16 的倍数)：AES-256 密钥；前 16 字节兼作 IV。
    hashed_key: [u8; 32],
}

impl AesCryptor {
    pub fn new(key: &str) -> Self {
        let key_empty = key.is_empty();
        let digest = Sha256::digest(Self::to_16_length(key.as_bytes()));
        let mut hashed_key = [0u8; 32];
        hashed_key.copy_from_slice(digest.as_slice());
        Self { key_empty, hashed_key }
    }

    /// Python `_to_16_length`：补 `\0` 至 16 的倍数（恰好对齐时不额外补块）。
    fn to_16_length(data: &[u8]) -> Vec<u8> {
        let pad = (16 - (data.len() % 16)) % 16;
        let mut out = Vec::with_capacity(data.len() + pad);
        out.extend_from_slice(data);
        out.resize(data.len() + pad, 0);
        out
    }

    /// Python `encrypt`：空密钥 → 明文字节原样；否则 `b2a_hex(CBC 加密(零填充后明文))`。
    pub fn encrypt(&self, plaintext: &[u8]) -> Vec<u8> {
        if self.key_empty {
            return plaintext.to_vec();
        }
        let padded = Self::to_16_length(plaintext);
        let ciphertext = cbc_encrypt(&self.hashed_key, &padded);
        hex::encode(ciphertext).into_bytes() // b2a_hex → 小写 hex ASCII
    }

    /// Python `decrypt`：空密钥 → 原样返回（仅校验 UTF-8，不做 rstrip）；
    /// 否则 `a2b_hex` → CBC 解密 → UTF-8 校验 → `rstrip("\0")`。
    /// 任何畸形输入（非法 hex、长度非块对齐、非 UTF-8）一律返回 None，绝不 panic。
    pub fn decrypt(&self, wire: &[u8]) -> Option<Vec<u8>> {
        if self.key_empty {
            // data.decode("utf-8")：非 UTF-8 → None（Python 抛 UnicodeDecodeError）
            std::str::from_utf8(wire).ok()?;
            return Some(wire.to_vec());
        }
        let ciphertext = hex::decode(wire).ok()?; // a2b_hex；binascii.Error → None
        if ciphertext.len() % 16 != 0 {
            return None; // PyCryptodome CBC 要求块对齐，否则 ValueError
        }
        let plain = cbc_decrypt(&self.hashed_key, &ciphertext);
        let text = String::from_utf8(plain).ok()?; // .decode("utf-8")；UnicodeDecodeError → None
        Some(text.trim_end_matches('\0').as_bytes().to_vec()) // .rstrip("\0")
    }
}

/// AES-256-CBC 加密，对应 PyCryptodome `AES.new(key, MODE_CBC, key[:16]).encrypt(padded)`。
/// IV = 密钥前 16 字节（Python: `self._hashed_key[:16]`）；`data` 必须已按 16 字节对齐
/// （零填充由 [`AesCryptor`] 完成，这里不做任何填充）。手工串 CBC 链以精确复刻其行为。
fn cbc_encrypt(hashed_key: &[u8; 32], data: &[u8]) -> Vec<u8> {
    let cipher = Aes256::new(GenericArray::from_slice(hashed_key));
    let mut prev = [0u8; 16];
    prev.copy_from_slice(&hashed_key[..16]); // IV = key[:16]
    let mut out = Vec::with_capacity(data.len());
    for block in data.chunks_exact(16) {
        let mut buf = [0u8; 16];
        buf.copy_from_slice(block);
        for (b, p) in buf.iter_mut().zip(prev.iter()) {
            *b ^= p;
        }
        let mut state = GenericArray::clone_from_slice(&buf);
        cipher.encrypt_block(&mut state);
        prev.copy_from_slice(state.as_slice());
        out.extend_from_slice(state.as_slice());
    }
    out
}

/// AES-256-CBC 解密（密文必须已按 16 字节对齐；不做去填充 —— Python 用 `rstrip("\0")` 收尾）。
fn cbc_decrypt(hashed_key: &[u8; 32], data: &[u8]) -> Vec<u8> {
    let cipher = Aes256::new(GenericArray::from_slice(hashed_key));
    let mut prev = [0u8; 16];
    prev.copy_from_slice(&hashed_key[..16]);
    let mut out = Vec::with_capacity(data.len());
    for block in data.chunks_exact(16) {
        let mut state = GenericArray::clone_from_slice(block);
        cipher.decrypt_block(&mut state);
        for (b, p) in state.as_mut_slice().iter_mut().zip(prev.iter()) {
            *b ^= p;
        }
        out.extend_from_slice(state.as_slice());
        prev.copy_from_slice(block);
    }
    out
}

/// 编码线帧：4 字节长度前缀（原生字节序）+ 载荷。
///
/// ⚠️ 线格式保真：Python 原实现为
/// `self._writer.write(struct.pack("I", len(encrypted)) + encrypted)`
/// —— `"I"` 无字节序前缀，即**本机原生字节序**（README 的"大端"说法与代码不符，以代码为准）。
fn encode_frame(payload: &[u8]) -> Vec<u8> {
    // 长度按 Python struct.pack("I", ...) 语义为 u32（实际 JSON 载荷远小于 4GiB）
    let mut out = (payload.len() as u32).to_ne_bytes().to_vec();
    out.extend_from_slice(payload);
    out
}

/// `_receive_raw` 的两类错误，对应 Python 收包循环的两档处理：
/// - [`RecvError::Connection`]：ConnectionError / OSError / IncompleteReadError / EOFError
///   —— 立即断线重连；
/// - [`RecvError::Decode`]：解密失败 / UTF-8 失败 / JSON 解析失败 —— 计入连续错误计数。
#[derive(Debug)]
enum RecvError {
    Connection(String),
    Decode(String),
}

fn recv_error_message(err: RecvError) -> String {
    match err {
        RecvError::Connection(msg) | RecvError::Decode(msg) => msg,
    }
}

/// Python `_receive_raw`：`readexactly(4)` → `struct.unpack("I", header)[0]`（原生字节序）→
/// 循环 `read` 直到读满 → 解密 → `json.loads`。
async fn receive_raw<S: AsyncRead + Unpin>(
    read: &mut S,
    cryptor: &AesCryptor,
) -> Result<Value, RecvError> {
    let mut header = [0u8; 4];
    if let Err(err) = read.read_exact(&mut header).await {
        // Python: readexactly 在 EOF（含半包）时抛 IncompleteReadError —— 连接类错误
        return Err(RecvError::Connection(err.to_string()));
    }
    // struct.unpack("I", header)[0] —— "I" 为原生字节序（见模块注释）
    let remaining = u32::from_ne_bytes(header) as usize;

    let mut body: Vec<u8> = Vec::new();
    while body.len() < remaining {
        let want = (remaining - body.len()).min(64 * 1024);
        let mut chunk = vec![0u8; want];
        let n = read
            .read(&mut chunk)
            .await
            .map_err(|e| RecvError::Connection(e.to_string()))?;
        if n == 0 {
            return Err(RecvError::Connection("连接断开".to_string()));
        }
        body.extend_from_slice(&chunk[..n]);
    }

    let plain = cryptor
        .decrypt(&body)
        .ok_or_else(|| RecvError::Decode("解密失败（非法 hex / 非 UTF-8）".to_string()))?;
    serde_json::from_slice(&plain).map_err(|e| RecvError::Decode(e.to_string()))
}

/// 锁内克隆回调槽位（毒锁回退取值，绝不 panic）。
fn clone_slot<T: Clone>(slot: &StdMutex<Option<T>>) -> Option<T> {
    slot.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// 写入回调槽位（毒锁回退取值，绝不 panic）。
fn store_slot<T>(slot: &StdMutex<Option<T>>, value: Option<T>) {
    *slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = value;
}

/// Python `str(payload.get(key, 默认 ""))` 的语义：
/// 键不存在 → ""；null → "None"（Python 的 `str(None)`）；bool → "True"/"False"；
/// 数字 → 十进制串；数组/对象 → JSON 表示（与 Python repr 有细微差异，协议中不应出现）。
fn py_str(value: Option<&Value>) -> String {
    match value {
        None => String::new(),
        Some(Value::Null) => "None".to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(true)) => "True".to_string(),
        Some(Value::Bool(false)) => "False".to_string(),
        Some(Value::Number(n)) => n.to_string(),
        Some(other) => other.to_string(),
    }
}

/// 客户端共享内部状态（`ChatBridgeClient` 内部持有 `Arc<Inner>`，供收包循环、
/// keep-alive 任务与外部 send_chat 并发使用；对应 Python 实例上的可变字段）。
struct Inner {
    host: String,
    port: u16,
    name: String,
    password: String,
    cryptor: AesCryptor,

    /// Python `self._running`
    running: AtomicBool,
    /// Python `self._connected`
    connected: AtomicBool,
    /// Python `self._writer`（None = 已关闭；锁串行化并发写，等价事件循环的单线程写）
    writer: TokioMutex<Option<OwnedWriteHalf>>,
    /// Python `self._pong_event`（asyncio.Event）的等价物：收到任意 pong 即 send；
    /// keep-alive 周期开头 `borrow_and_update` 消费当前值，等价于 `clear()`。
    pong_tx: watch::Sender<u64>,

    on_chat: StdMutex<Option<ChatCallback>>,
    on_connected: StdMutex<Option<StateCallback>>,
    on_disconnected: StdMutex<Option<StateCallback>>,
}

impl Inner {
    /// Python `_send_raw`：`json.dumps(data, ensure_ascii=False)` → encrypt →
    /// `struct.pack("I", len(encrypted)) + encrypted` → 写出 + drain。
    async fn send_raw(&self, packet: &Value) -> Result<(), String> {
        let payload = serde_json::to_vec(packet).map_err(|e| e.to_string())?;
        let encrypted = self.cryptor.encrypt(&payload);
        let frame = encode_frame(&encrypted);
        let mut guard = self.writer.lock().await;
        match guard.as_mut() {
            Some(writer) => {
                writer.write_all(&frame).await.map_err(|e| e.to_string())?;
                writer.flush().await.map_err(|e| e.to_string())?; // 对应 await writer.drain()
                Ok(())
            }
            None => Err("writer is None".to_string()), // Python: raise ConnectionError("writer is None")
        }
    }

    /// Python `_send_packet`：未连接时**静默** return（Python 无日志）；发送失败仅告警，不向上抛。
    async fn send_packet(&self, packet: &Value) {
        if !self.connected.load(Ordering::SeqCst) {
            return; // Python: `if not self._connected or self._writer is None: return`
        }
        if let Err(err) = self.send_raw(packet).await {
            tracing::warn!("ChatBridge 发送失败: {}", err);
        }
    }
}

/// 异步 ChatBridge 客户端：断线自动重连，回调订阅聊天消息。
pub struct ChatBridgeClient {
    inner: Arc<Inner>,
}

impl ChatBridgeClient {
    pub fn new(
        host: impl Into<String>,
        port: u16,
        name: impl Into<String>,
        password: impl Into<String>,
        aes_key: impl Into<String>,
    ) -> Self {
        let (pong_tx, _pong_rx) = watch::channel(0u64);
        Self {
            inner: Arc::new(Inner {
                host: host.into(),
                port,
                name: name.into(),
                password: password.into(),
                cryptor: AesCryptor::new(&aes_key.into()),
                running: AtomicBool::new(false),
                connected: AtomicBool::new(false),
                writer: TokioMutex::new(None),
                pong_tx,
                on_chat: StdMutex::new(None),
                on_connected: StdMutex::new(None),
                on_disconnected: StdMutex::new(None),
            }),
        }
    }

    pub fn set_on_chat(&self, callback: ChatCallback) {
        store_slot(&self.inner.on_chat, Some(callback));
    }

    pub fn set_on_connected(&self, callback: StateCallback) {
        store_slot(&self.inner.on_connected, Some(callback));
    }

    pub fn set_on_disconnected(&self, callback: StateCallback) {
        store_slot(&self.inner.on_disconnected, Some(callback));
    }

    /// Python `is_connected` 属性。
    pub fn is_connected(&self) -> bool {
        self.inner.connected.load(Ordering::SeqCst)
    }

    /// 置位/清除运行标志（run() 入口置 true；stop() 置 false；测试可借此直接驱动单次会话，
    /// 等价于 Python 测试注入 `client._running = True`）。
    fn set_running(&self, value: bool) {
        self.inner.running.store(value, Ordering::SeqCst);
    }

    /// 生命周期主循环（对应 Python `run`）：连接+登录 → on_connected → keep-alive →
    /// 收包循环 → on_disconnected → `RECONNECT_DELAY` 秒后重连；`stop()` 置位后
    /// 当前轮结束即退出，不再重连。
    pub async fn run(self: Arc<Self>) {
        self.set_running(true);
        while self.inner.running.load(Ordering::SeqCst) {
            self.run_once().await;
            if !self.inner.running.load(Ordering::SeqCst) {
                break;
            }
            tracing::info!("ChatBridge {}s 后重连...", RECONNECT_DELAY);
            tokio::time::sleep(Duration::from_secs(RECONNECT_DELAY)).await;
        }
    }

    /// 单次"连接+会话"尝试（对应 Python run() 的单次 while 迭代体）。测试可不经外层
    /// 重连循环直接驱动本方法。
    async fn run_once(&self) {
        match self.connect_and_login().await {
            Ok(read) => self.run_session(read).await,
            Err(err) => {
                tracing::error!("ChatBridge 连接异常: {}", err);
                // Python finally：无论成败都关流并触发 on_disconnected
                self.finalize().await;
            }
        }
    }

    /// Python `stop`：仅置位标志；收包循环在下一个包边界退出，run() 不再重连。
    pub fn stop(&self) {
        self.set_running(false);
    }

    pub async fn send_chat(&self, target: &str, message: &str, author: &str) {
        self.inner
            .send_packet(&json!({
                "sender": self.inner.name,
                "receivers": [target],
                "broadcast": false,
                "type": PACKET_TYPE_CHAT,
                "payload": {"author": author, "message": message},
            }))
            .await;
    }

    pub async fn broadcast_chat(&self, message: &str, author: &str) {
        self.inner
            .send_packet(&json!({
                "sender": self.inner.name,
                "receivers": [],
                "broadcast": true,
                "type": PACKET_TYPE_CHAT,
                "payload": {"author": author, "message": message},
            }))
            .await;
    }

    /// Python `_connect_and_login`：整体包在 `CONNECT_TIMEOUT` 内，超时按握手失败处理。
    async fn connect_and_login(&self) -> Result<OwnedReadHalf, String> {
        tracing::info!("ChatBridge 正在连接 {}:{} ...", self.inner.host, self.inner.port);
        match tokio::time::timeout(
            Duration::from_secs(CONNECT_TIMEOUT),
            self.connect_and_login_inner(),
        )
        .await
        {
            Ok(result) => {
                let read = result?;
                tracing::info!("ChatBridge 登录成功（客户端名 {}）", self.inner.name);
                Ok(read)
            }
            Err(_) => Err(format!("ChatBridge 握手超时（{}s）", CONNECT_TIMEOUT)),
        }
    }

    /// Python `_connect_and_login_inner`：建立 TCP → 发登录帧 → 等 `{"message": "ok"}`。
    async fn connect_and_login_inner(&self) -> Result<OwnedReadHalf, String> {
        let stream = TcpStream::connect((self.inner.host.as_str(), self.inner.port))
            .await
            .map_err(|e| format!("连接失败: {}", e))?;
        let (read, write) = stream.into_split();
        *self.inner.writer.lock().await = Some(write);
        // 登录帧走 _send_raw（不检查 connected），失败直接上抛（Python 同）
        self.inner
            .send_raw(&json!({
                "name": self.inner.name,
                "password": self.inner.password,
            }))
            .await?;
        let mut read = read;
        let reply = receive_raw(&mut read, &self.inner.cryptor).await.map_err(recv_error_message)?;
        if reply.get("message").and_then(Value::as_str) == Some("ok") {
            Ok(read)
        } else {
            Err(format!("ChatBridge 登录失败: {}", reply))
        }
    }

    /// 单次连接会话（对应 Python run() try 块主体）：置 connected → on_connected →
    /// keep-alive → 收包循环 → 结束后 finalize（connected=false、关流、on_disconnected）。
    /// 测试可不经外层重连循环直接驱动本方法。
    async fn run_session(&self, read: OwnedReadHalf) {
        self.inner.connected.store(true, Ordering::SeqCst);
        self.fire_state_callback(&self.inner.on_connected).await;
        tracing::info!("ChatBridge 已连接: {}:{}", self.inner.host, self.inner.port);

        let (dead_tx, dead_rx) = watch::channel(false);
        let keep_alive = tokio::spawn(keep_alive_loop(Arc::clone(&self.inner), dead_tx));
        receive_loop(&self.inner, read, dead_rx).await;
        keep_alive.abort(); // Python: keep_alive_task.cancel()

        // Python run() finally：connected=False → _close() → on_disconnected
        self.finalize().await;
    }

    /// Python run() 的 finally + `_close()`：connected=False，关流（错误吞掉），
    /// 触发 on_disconnected（无论本次会话成败都会触发，与 Python finally 一致）。
    async fn finalize(&self) {
        self.inner.connected.store(false, Ordering::SeqCst);
        if let Some(mut writer) = self.inner.writer.lock().await.take() {
            let _ = writer.shutdown().await; // writer.close()：失败吞掉
        }
        self.fire_state_callback(&self.inner.on_disconnected).await;
    }

    /// 触发状态回调（Python `_call_callback`：异常只记日志，不上抛）。
    async fn fire_state_callback(&self, slot: &StdMutex<Option<StateCallback>>) {
        if let Some(callback) = clone_slot(slot) {
            if let Err(err) = AssertUnwindSafe(callback()).catch_unwind().await {
                tracing::error!("ChatBridge 回调异常: {:?}", err);
            }
        }
    }
}

/// Python `_keep_alive_loop`：每 `KEEP_ALIVE_INTERVAL` 秒向 `#SERVER` 发 ping；
/// `KEEP_ALIVE_TIMEOUT` 内没有 pong → 主动断流，令收包循环退出并重连。
async fn keep_alive_loop(inner: Arc<Inner>, dead_tx: watch::Sender<bool>) {
    while inner.running.load(Ordering::SeqCst) && inner.connected.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_secs(KEEP_ALIVE_INTERVAL)).await;
        if !inner.connected.load(Ordering::SeqCst) {
            break;
        }
        // Python: self._pong_event.clear() —— 把当前计数标记为已读，陈旧 pong 不计入
        let mut pong_rx = inner.pong_tx.subscribe();
        let _ = *pong_rx.borrow_and_update();

        inner
            .send_packet(&json!({
                "sender": inner.name,
                "receivers": [SERVER_NAME],
                "broadcast": false,
                "type": PACKET_TYPE_KEEP_ALIVE,
                "payload": {"ping_type": "ping"},
            }))
            .await;

        match tokio::time::timeout(Duration::from_secs(KEEP_ALIVE_TIMEOUT), pong_rx.changed()).await {
            Ok(_) => {}
            Err(_) => {
                tracing::warn!("ChatBridge keep-alive 超时，主动断开重连");
                // Python: await self._close() —— connected=False + 关流
                inner.connected.store(false, Ordering::SeqCst);
                if let Some(mut writer) = inner.writer.lock().await.take() {
                    let _ = writer.shutdown().await;
                }
                let _ = dead_tx.send(true);
                break;
            }
        }
    }
}

/// Python `_receive_loop`：连接类错误立即跳出重连；其余错误连续 5 次后按断线处理。
async fn receive_loop(inner: &Inner, mut read: OwnedReadHalf, mut dead_rx: watch::Receiver<bool>) {
    let mut consecutive_errors: u32 = 0;
    while inner.running.load(Ordering::SeqCst) && inner.connected.load(Ordering::SeqCst) {
        let packet = tokio::select! {
            res = receive_raw(&mut read, &inner.cryptor) => match res {
                Ok(packet) => packet,
                Err(RecvError::Connection(msg)) => {
                    // IncompleteReadError/EOFError 也意味着对端已关闭连接：必须跳出重连，
                    // 否则 readexactly 会立即再次抛错，形成不让出的死循环（把事件循环打满）。
                    tracing::warn!("ChatBridge 连接断开: {}", msg);
                    break;
                }
                Err(RecvError::Decode(msg)) => {
                    consecutive_errors += 1;
                    tracing::error!("ChatBridge 处理消息异常: {}", msg);
                    if consecutive_errors >= 5 {
                        tracing::warn!(
                            "ChatBridge 连续 {} 次处理异常，按连接断开处理并重连",
                            consecutive_errors
                        );
                        break;
                    }
                    continue;
                }
            },
            _ = dead_rx.changed() => {
                // keep-alive 超时已主动断流（Python: _close() 使 readexactly 抛 IncompleteReadError）
                tracing::warn!("ChatBridge 连接断开: 连接已被主动关闭");
                break;
            }
        };

        match dispatch(inner, &packet).await {
            Ok(()) => consecutive_errors = 0,
            Err(msg) => {
                consecutive_errors += 1;
                tracing::error!("ChatBridge 处理消息异常: {}", msg);
                if consecutive_errors >= 5 {
                    tracing::warn!(
                        "ChatBridge 连续 {} 次处理异常，按连接断开处理并重连",
                        consecutive_errors
                    );
                    break;
                }
            }
        }
    }
}

/// Python `_dispatch`。只有"顶层不是 JSON 对象"才算处理错误（Python 里 `packet.get`
/// 会抛 AttributeError）；其余分支与 Python 一致地自带容错，不会向外抛错。
async fn dispatch(inner: &Inner, packet: &Value) -> Result<(), String> {
    let obj = match packet.as_object() {
        Some(obj) => obj,
        None => return Err(format!("包不是 JSON 对象: {}", packet)),
    };
    let ptype = obj.get("type").and_then(Value::as_str).unwrap_or("");
    let sender = obj.get("sender").and_then(Value::as_str).unwrap_or("");
    let payload = match obj.get("payload") {
        Some(p) if p.is_object() => p,
        _ => return Ok(()), // Python: not isinstance(payload, dict) → return
    };

    if ptype == PACKET_TYPE_KEEP_ALIVE {
        let ping_type = payload.get("ping_type").and_then(Value::as_str).unwrap_or("");
        if ping_type == "ping" {
            // 收到 ping → 向 ping 的发送者回 pong
            inner
                .send_packet(&json!({
                    "sender": inner.name,
                    "receivers": [sender],
                    "broadcast": false,
                    "type": PACKET_TYPE_KEEP_ALIVE,
                    "payload": {"ping_type": "pong"},
                }))
                .await;
        } else if ping_type == "pong" {
            // Python: self._pong_event.set()
            let _ = inner.pong_tx.send(1);
        }
    } else if ptype == PACKET_TYPE_CHAT {
        let author = py_str(payload.get("author"));
        let message = py_str(payload.get("message"));
        if !message.is_empty() {
            if let Some(callback) = clone_slot(&inner.on_chat) {
                // 回调内联等待；异常只记日志（Python _call_callback），绝不能打断收包循环
                let fut = callback(sender.to_string(), author, message);
                if let Err(err) = AssertUnwindSafe(fut).catch_unwind().await {
                    tracing::error!("ChatBridge 回调异常: {:?}", err);
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWrite;
    use tokio::net::TcpListener;
    use tokio::sync::{mpsc, oneshot};

    /// 测试服务端辅助：把一包 JSON 按协议（加密 + 长度前缀帧）写出去。
    async fn send_packet_to<W: AsyncWrite + Unpin>(w: &mut W, cryptor: &AesCryptor, packet: &Value) {
        let payload = serde_json::to_vec(packet).unwrap();
        w.write_all(&encode_frame(&cryptor.encrypt(&payload)))
            .await
            .unwrap();
    }

    fn chat_packet(receiver: &str, message: &str) -> Value {
        json!({
            "sender": SERVER_NAME,
            "receivers": [receiver],
            "broadcast": false,
            "type": PACKET_TYPE_CHAT,
            "payload": {"author": "alice", "message": message},
        })
    }

    // ---------- AesCryptor ----------

    #[test]
    fn aes_roundtrip_keyed_produces_lowercase_hex() {
        let cryptor = AesCryptor::new("ThisIstheSecret");
        let plaintext = "你好，ChatBridge！mixed 中英文 0123456789";
        let wire = cryptor.encrypt(plaintext.as_bytes());
        // b2a_hex 输出小写 hex ASCII
        assert!(wire.iter().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));
        assert_eq!(wire.len() % 32, 0, "密文长度必须是 16 字节块的 2 倍 hex");
        assert_eq!(cryptor.decrypt(&wire).as_deref(), Some(plaintext.as_bytes()));
    }

    #[test]
    fn aes_empty_key_passthrough_exact_bytes() {
        let cryptor = AesCryptor::new("");
        // Python: encrypt → text.encode("utf-8") 原样返回（非 UTF-8 字节也原样透传）
        let plaintext: &[u8] = b"\x01\x02 raw {\"json\":true} \xff\xfe";
        assert_eq!(cryptor.encrypt(plaintext), plaintext.to_vec());
        // 空密钥分支不做 rstrip("\0")：尾部 \0 原样保留
        let with_nul: &[u8] = b"abc\0\0";
        assert_eq!(cryptor.decrypt(with_nul), Some(with_nul.to_vec()));
        // 非 UTF-8 → None（对应 Python data.decode("utf-8") 抛 UnicodeDecodeError）
        assert_eq!(cryptor.decrypt(b"\xff\xfe"), None);
    }

    #[test]
    fn aes_known_answers_from_python_reference() {
        // 向量由原 Python 实现（PyCryptodome，src/chatroom_bridge/chatbridge.py）直接生成。
        let cryptor = AesCryptor::new("ThisIstheSecret");
        // 密钥派生：15 字节密码补 1 个 \0 → sha256 = 8e950f2b...674a
        assert_eq!(
            cryptor.encrypt(b"hello world"),
            b"a687744a874514b4c6cf5a819395726e".to_vec()
        );
        // 恰好 16 字节 → 不额外补块（PyCryptodome 无自动填充的直接证据）
        assert_eq!(
            cryptor.encrypt(&[b'A'; 16]),
            b"28d58b11ae093009fc42ee7348b6f81b".to_vec()
        );
        // 43 字节 → 补 5 个 \0 → 3 个块（验证跨块 CBC 链接）
        assert_eq!(
            cryptor.encrypt("The quick brown fox jumps over the lazy dog".as_bytes()),
            b"3721962b807a8fba617dbcc24342a77c47df2622781d6a7191b3e65166cb53b0e69640c01b5068ddbdc68d4907acd658".to_vec()
        );
        // 空明文 → 0 块 → 空密文（与 Python 一致）
        assert_eq!(cryptor.encrypt(b""), Vec::<u8>::new());
        assert_eq!(
            cryptor.decrypt(b"a687744a874514b4c6cf5a819395726e").as_deref(),
            Some(&b"hello world"[..])
        );
        assert_eq!(cryptor.decrypt(b""), Some(Vec::<u8>::new()));
    }

    #[test]
    fn aes_tampered_or_malformed_input_returns_none() {
        let cryptor = AesCryptor::new("ThisIstheSecret");
        assert_eq!(
            cryptor.decrypt(&cryptor.encrypt(b"payload")).as_deref(),
            Some(&b"payload"[..])
        );
        // 非法 hex 字符（Python: binascii.Error）
        let mut bad = cryptor.encrypt(b"payload");
        bad[0] = b'z';
        assert_eq!(cryptor.decrypt(&bad), None);
        // 奇数长度 hex（Python: binascii.Error）
        assert_eq!(cryptor.decrypt(b"abc"), None);
        // hex 合法但密文不是块对齐（Python: ValueError）
        assert_eq!(cryptor.decrypt(b"00"), None);
        // 块对齐但解密结果不是 UTF-8（Python: UnicodeDecodeError）
        assert_eq!(cryptor.decrypt(&[b'f'; 32]), None);
    }

    // ---------- 帧编解码 ----------

    #[test]
    fn frame_encode_exact_bytes_native_endian() {
        let payload = serde_json::to_vec(&json!({"name": "A"})).unwrap();
        assert_eq!(payload, b"{\"name\":\"A\"}".to_vec());
        let frame = encode_frame(&payload);
        // Python: struct.pack("I", 12) → 本机原生字节序（小端）0c 00 00 00
        let mut expected = vec![0x0c, 0x00, 0x00, 0x00];
        expected.extend_from_slice(b"{\"name\":\"A\"}");
        assert_eq!(frame, expected);
        // 跨平台保真：与 u32 原生序编码一致（struct.pack("I") 就是原生序）
        assert_eq!(&frame[..4], &12u32.to_ne_bytes());
    }

    #[tokio::test]
    async fn frame_decode_roundtrip_and_error_classification() {
        let cryptor = AesCryptor::new("");
        let payload = serde_json::to_vec(&json!({"name": "A"})).unwrap();
        let frame = encode_frame(&payload);

        // 正向：从流中解出 JSON
        let mut cursor: &[u8] = &frame;
        let value = receive_raw(&mut cursor, &cryptor).await.unwrap();
        assert_eq!(value, json!({"name": "A"}));

        // 半截头部 → 连接类错误（Python: readexactly 抛 IncompleteReadError）
        let mut partial: &[u8] = &frame[..2];
        assert!(matches!(
            receive_raw(&mut partial, &cryptor).await,
            Err(RecvError::Connection(_))
        ));

        // 头部正常但载荠除非 JSON → 解析类错误（计入连续错误计数）
        let mut garbage: &[u8] = &[0x04, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff];
        assert!(matches!(
            receive_raw(&mut garbage, &cryptor).await,
            Err(RecvError::Decode(_))
        ));

        // 空流（EOF）→ 连接类错误
        let mut empty: &[u8] = &[];
        assert!(matches!(
            receive_raw(&mut empty, &cryptor).await,
            Err(RecvError::Connection(_))
        ));
    }

    // ---------- 回环集成 ----------

    #[tokio::test]
    async fn loopback_login_chat_pong_and_broadcast() {
        let cryptor = AesCryptor::new("testkey");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let (broadcast_tx, broadcast_rx) = oneshot::channel::<Value>();
        let (pong_tx, pong_rx) = oneshot::channel::<Value>();
        let (close_tx, close_rx) = oneshot::channel::<()>();

        let server = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            let (mut r, mut w) = sock.into_split();

            // 1) 登录帧：断言客户端名与密码
            let login = receive_raw(&mut r, &cryptor).await.unwrap();
            assert_eq!(login["name"], "tester");
            assert_eq!(login["password"], "pw");
            // 2) 按登录序列回 {"message": "ok"}
            send_packet_to(&mut w, &cryptor, &json!({"message": "ok"})).await;

            // 3) 服务端下发一包 chat
            send_packet_to(&mut w, &cryptor, &chat_packet("tester", "hello from server")).await;

            // 4) 收客户端的 broadcast_chat
            let got = receive_raw(&mut r, &cryptor).await.unwrap();
            let _ = broadcast_tx.send(got);

            // 5) 服务端 keep-alive ping → 客户端应向 ping 发送者回 pong
            send_packet_to(
                &mut w,
                &cryptor,
                &json!({
                    "sender": SERVER_NAME,
                    "receivers": ["tester"],
                    "broadcast": false,
                    "type": PACKET_TYPE_KEEP_ALIVE,
                    "payload": {"ping_type": "ping"},
                }),
            )
            .await;
            let pong = receive_raw(&mut r, &cryptor).await.unwrap();
            let _ = pong_tx.send(pong);

            // 6) 挂住连接直到测试放行（保证客户端不会因 EOF 提前重连）
            let _ = close_rx.await;
        });

        let client = Arc::new(ChatBridgeClient::new(
            addr.ip().to_string(),
            addr.port(),
            "tester",
            "pw",
            "testkey",
        ));

        let (chat_tx, mut chat_rx) = mpsc::unbounded_channel();
        client.set_on_chat(Arc::new(move |sender, author, message| {
            let tx = chat_tx.clone();
            Box::pin(async move {
                let _ = tx.send((sender, author, message));
            })
        }));
        let (conn_tx, mut conn_rx) = mpsc::unbounded_channel();
        client.set_on_connected(Arc::new(move || {
            let tx = conn_tx.clone();
            Box::pin(async move {
                let _ = tx.send(());
            })
        }));
        let (disc_tx, mut disc_rx) = mpsc::unbounded_channel();
        client.set_on_disconnected(Arc::new(move || {
            let tx = disc_tx.clone();
            Box::pin(async move {
                let _ = tx.send(());
            })
        }));

        let runner = tokio::spawn(Arc::clone(&client).run());

        // 登录完成：on_connected 触发且 is_connected 为真
        tokio::time::timeout(Duration::from_secs(3), conn_rx.recv())
            .await
            .expect("3s 内应完成登录")
            .expect("channel 不应关闭");
        assert!(client.is_connected());

        // on_chat 收到服务端下发的 chat（sender, author, message 逐项正确）
        let (sender, author, message) =
            tokio::time::timeout(Duration::from_secs(3), chat_rx.recv())
                .await
                .expect("3s 内应收到 chat")
                .expect("channel 不应关闭");
        assert_eq!(sender, SERVER_NAME);
        assert_eq!(author, "alice");
        assert_eq!(message, "hello from server");

        // broadcast_chat 到达服务端
        client.broadcast_chat("hi from client", "bob").await;
        let got = tokio::time::timeout(Duration::from_secs(3), broadcast_rx)
            .await
            .expect("3s 内服务端应收到 broadcast")
            .expect("oneshot 不应关闭");
        assert_eq!(got["sender"], "tester");
        assert_eq!(got["broadcast"], true);
        assert_eq!(got["type"], PACKET_TYPE_CHAT);
        assert_eq!(got["payload"]["author"], "bob");
        assert_eq!(got["payload"]["message"], "hi from client");

        // 服务端 ping → 客户端 pong（回给 ping 的发送者）
        let pong = tokio::time::timeout(Duration::from_secs(3), pong_rx)
            .await
            .expect("3s 内服务端应收到 pong")
            .expect("oneshot 不应关闭");
        assert_eq!(pong["sender"], "tester");
        assert_eq!(pong["receivers"][0], SERVER_NAME);
        assert_eq!(pong["type"], PACKET_TYPE_KEEP_ALIVE);
        assert_eq!(pong["payload"]["ping_type"], "pong");

        // 收尾：先 stop 再断流 → run() 不应重连，直接退出
        client.stop();
        let _ = close_tx.send(());
        tokio::time::timeout(Duration::from_secs(3), runner)
            .await
            .expect("客户端应在断开后退出")
            .expect("run 任务不应 panic");
        server.await.expect("服务端任务不应 panic");
        assert!(disc_rx.try_recv().is_ok(), "on_disconnected 应已触发");
    }

    // ---------- 收包循环回归（镜像 tests/test_chatbridge_receive_loop.py） ----------

    #[tokio::test]
    async fn closed_connection_breaks_receive_loop_immediately() {
        // 镜像 test_closed_connection_exits_receive_loop：对端关闭（EOF）后必须立即退出
        // 收包循环 —— 若误吞连接类错误，循环会不停重读死 socket，on_disconnected 永不触发。
        let cryptor = AesCryptor::new("ThisIstheSecret");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            let (mut r, mut w) = sock.into_split();
            let login = receive_raw(&mut r, &cryptor).await.unwrap();
            assert_eq!(login["name"], "web");
            send_packet_to(&mut w, &cryptor, &json!({"message": "ok"})).await;
            // 立即断开：模拟 Python 测试的 EofReader
            drop(r);
            drop(w);
        });

        let client = Arc::new(ChatBridgeClient::new(
            addr.ip().to_string(),
            addr.port(),
            "web",
            "pw",
            "ThisIstheSecret",
        ));
        let (disc_tx, mut disc_rx) = mpsc::unbounded_channel();
        client.set_on_disconnected(Arc::new(move || {
            let tx = disc_tx.clone();
            Box::pin(async move {
                let _ = tx.send(());
            })
        }));

        // 直接驱动单次会话（不走外层重连循环），等价于 Python 测试注入 _running=True
        client.set_running(true);
        let driver = Arc::clone(&client);
        let session = tokio::spawn(async move {
            driver.run_once().await;
        });

        tokio::time::timeout(Duration::from_secs(3), disc_rx.recv())
            .await
            .expect("对端关闭后收包循环必须立即退出（旧实现的死循环会在此超时）")
            .expect("channel 不应关闭");

        client.stop();
        tokio::time::timeout(Duration::from_secs(3), session)
            .await
            .expect("会话应已结束")
            .expect("会话任务不应 panic");
        server.await.expect("服务端任务不应 panic");
    }

    #[tokio::test]
    async fn receive_loop_breaks_after_five_consecutive_errors() {
        // 镜像 test_repeated_errors_break_loop：连续 5 次处理异常后收包循环必须退出，
        // 且成功处理会清零计数；全程不依赖 EOF（服务端始终不主动断开）。
        // 包序：err, ok(msg1), err, ok(msg2), err×5 → 第 5 个连续 err 后断开，
        // 其后的 msg3 不应被处理。
        let cryptor = AesCryptor::new("ThisIstheSecret");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let (release_tx, release_rx) = oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            let (mut r, mut w) = sock.into_split();

            let login = receive_raw(&mut r, &cryptor).await.unwrap();
            assert_eq!(login["name"], "web");
            send_packet_to(&mut w, &cryptor, &json!({"message": "ok"})).await;

            let bad = json!(["not-an-object"]); // 顶层不是 JSON 对象 → dispatch 报错
            send_packet_to(&mut w, &cryptor, &bad).await;
            send_packet_to(&mut w, &cryptor, &chat_packet("web", "msg1")).await;
            send_packet_to(&mut w, &cryptor, &bad).await;
            send_packet_to(&mut w, &cryptor, &chat_packet("web", "msg2")).await;
            for _ in 0..5 {
                send_packet_to(&mut w, &cryptor, &bad).await;
            }
            // 循环已断：这包永远不该被处理
            send_packet_to(&mut w, &cryptor, &chat_packet("web", "msg3-after-break")).await;

            let _ = release_rx.await; // 挂住连接直到测试放行
        });

        let client = Arc::new(ChatBridgeClient::new(
            addr.ip().to_string(),
            addr.port(),
            "web",
            "pw",
            "ThisIstheSecret",
        ));
        let (chat_tx, mut chat_rx) = mpsc::unbounded_channel::<(String, String, String)>();
        client.set_on_chat(Arc::new(move |sender, author, message| {
            let tx = chat_tx.clone();
            Box::pin(async move {
                let _ = tx.send((sender, author, message));
            })
        }));
        let (disc_tx, mut disc_rx) = mpsc::unbounded_channel();
        client.set_on_disconnected(Arc::new(move || {
            let tx = disc_tx.clone();
            Box::pin(async move {
                let _ = tx.send(());
            })
        }));

        client.set_running(true);
        let driver = Arc::clone(&client);
        let session = tokio::spawn(async move {
            driver.run_once().await;
        });

        // 会话结束（循环在第 5 个连续错误后断开 → finalize → on_disconnected）
        tokio::time::timeout(Duration::from_secs(3), disc_rx.recv())
            .await
            .expect("收包循环应在 5 次连续错误后退出")
            .expect("channel 不应关闭");

        // 只有 msg1/msg2 被处理（证明成功会清零计数、5 个连续错误触发断开、之后的包不再处理）
        let mut messages = Vec::new();
        while let Ok(m) = chat_rx.try_recv() {
            messages.push(m.2);
        }
        assert_eq!(messages, vec!["msg1".to_string(), "msg2".to_string()]);

        client.stop();
        let _ = release_tx.send(());
        tokio::time::timeout(Duration::from_secs(3), session)
            .await
            .expect("会话应已结束")
            .expect("会话任务不应 panic");
        server.await.expect("服务端任务不应 panic");
    }

    // ---------- 其他行为 ----------

    #[tokio::test]
    async fn send_while_disconnected_is_silent_noop() {
        // Python _send_packet：未连接时静默 return（无日志、无 panic、无 IO）
        let client = ChatBridgeClient::new("127.0.0.1", 1, "n", "p", "");
        assert!(!client.is_connected());
        client.send_chat("mc", "hello", "tester").await;
        client.broadcast_chat("hello", "tester").await;
        assert!(!client.is_connected());
    }
}
