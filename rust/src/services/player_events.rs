//! 玩家上下线事件检测（ChatBridge 事件驱动）。
//!
//! MC 服务端插件把玩家的加入/离开以**系统广播**形式经 ChatBridge 发来
//! （author 为空、content 形如「Steve 加入了游戏」）。本模块用可配置的正则
//! 从广播文本里识别上下线事件并提取玩家名，供 QQ 群推送。
//!
//! 为什么不走状态网站轮询：轮询做快照差分天然可能丢掉单次事件（两次轮询
//! 之间上线又下线），而上下线推送恰恰要求每一条都不丢——事件源就在
//! ChatBridge，直接用它。`/status` 命令的数据仍来自状态网站，那边只关心
//! 当前状态，不关心单次事件。

use regex::Regex;

/// 事件类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventType {
    Online,
    Offline,
}

/// 玩家上下线事件。
#[derive(Debug, Clone)]
pub struct PlayerEvent {
    pub event_type: EventType,
    /// 来源 ChatBridge 客户端名（服务端插件标识，记录用）。
    pub server: String,
    pub player: String,
}

impl PlayerEvent {
    /// QQ 群推送文案：`🎮 {player} 上线` / `🚪 {player} 下线`。
    pub fn push_text(&self) -> String {
        let (icon, action) = match self.event_type {
            EventType::Online => ("🎮", "上线"),
            EventType::Offline => ("🚪", "下线"),
        };
        format!("{icon} {} {action}", self.player)
    }
}

/// 上下线事件检测器：两个正则（各含一个玩家名捕获组），均未配置 = 禁用。
pub struct PlayerEventDetector {
    join: Option<Regex>,
    quit: Option<Regex>,
}

impl PlayerEventDetector {
    /// 模式为空 = 对应事件不启用。正则非法时返回 `Err`（装配层 warn 后禁用）。
    pub fn new(join_pattern: &str, quit_pattern: &str) -> Result<Self, regex::Error> {
        let join = (!join_pattern.is_empty()).then(|| Regex::new(join_pattern)) .transpose()?;
        let quit = (!quit_pattern.is_empty()).then(|| Regex::new(quit_pattern)).transpose()?;
        Ok(Self { join, quit })
    }

    /// 全禁用的检测器（正则配置非法时的兜底）。
    pub fn disabled() -> Self {
        Self {
            join: None,
            quit: None,
        }
    }

    pub fn enabled(&self) -> bool {
        self.join.is_some() || self.quit.is_some()
    }

    /// 从系统广播文本识别事件。玩家名 = 正则的第一个捕获组；
    /// 先查上线再查下线，两个正则都未配置或都不命中 → `None`。
    pub fn detect(&self, server: &str, message: &str) -> Option<PlayerEvent> {
        if let Some(regex) = &self.join {
            if let Some(player) = regex
                .captures(message)
                .and_then(|caps| caps.get(1))
                .map(|m| m.as_str().to_string())
            {
                return Some(PlayerEvent {
                    event_type: EventType::Online,
                    server: server.to_string(),
                    player,
                });
            }
        }
        if let Some(regex) = &self.quit {
            if let Some(player) = regex
                .captures(message)
                .and_then(|caps| caps.get(1))
                .map(|m| m.as_str().to_string())
            {
                return Some(PlayerEvent {
                    event_type: EventType::Offline,
                    server: server.to_string(),
                    player,
                });
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_join_and_quit_with_capture_group() {
        let detector = PlayerEventDetector::new("^(.+?) 加入了游戏$", "^(.+?) 离开了游戏$").unwrap();

        let join = detector.detect("snapshot", "Steve 加入了游戏").unwrap();
        assert_eq!(join.event_type, EventType::Online);
        assert_eq!(join.player, "Steve");
        assert_eq!(join.server, "snapshot");
        assert_eq!(join.push_text(), "🎮 Steve 上线");

        let quit = detector.detect("snapshot", "Alex 离开了游戏").unwrap();
        assert_eq!(quit.event_type, EventType::Offline);
        assert_eq!(quit.push_text(), "🚪 Alex 下线");

        // 普通聊天不误报
        assert!(detector.detect("snapshot", "Steve: 大家好").is_none());
        // 上线优先于下线（同一文本同时命中两个模式时取上线）
        let both = PlayerEventDetector::new("(.+?) 加入了游戏", "^(.+?) 加入了游戏$").unwrap();
        assert_eq!(
            both.detect("s", "Steve 加入了游戏").unwrap().event_type,
            EventType::Online
        );
        // 无捕获组的模式：拿不到玩家名，安全跳过（不 panic）
        let no_group = PlayerEventDetector::new("加入", "").unwrap();
        assert!(no_group.detect("s", "Steve 加入了游戏").is_none());
    }

    #[test]
    fn empty_patterns_disable_detection() {
        let detector = PlayerEventDetector::new("", "").unwrap();
        assert!(!detector.enabled());
        assert!(detector.detect("s", "Steve 加入了游戏").is_none());
    }

    #[test]
    fn invalid_regex_reports_error() {
        assert!(PlayerEventDetector::new("^(.+?) 加入了游戏$", "([").is_err());
        let disabled = PlayerEventDetector::disabled();
        assert!(!disabled.enabled());
    }

    #[test]
    fn partial_patterns_only_detect_configured_side() {
        let detector = PlayerEventDetector::new("^(.+?) 加入了游戏$", "").unwrap();
        assert!(detector.detect("s", "Steve 加入了游戏").is_some());
        assert!(detector.detect("s", "Steve 离开了游戏").is_none());
    }
}
