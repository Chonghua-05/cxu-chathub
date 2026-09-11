"""配置加载。token 只从文件读取，绝不写日志。"""

from __future__ import annotations

import json
import os
from dataclasses import dataclass, field
from typing import Any


class ConfigError(RuntimeError):
    pass


@dataclass
class OneBotConfig:
    listen_host: str = "0.0.0.0"
    listen_port: int = 6200
    path: str = "/ws"
    access_token: str = ""
    self_id: int = 0


@dataclass
class ChatroomConfig:
    base_url: str = "https://chatroom.example.com"  # 示例值，按实际部署地址覆盖
    channel_id: int = 1
    forward_token: str = ""
    refresh_token: str = ""
    group_ids: list[int] = field(default_factory=list)
    poll_interval: int = 10
    debounce_count: int = 2
    qq_sync_enabled: bool = True
    qq_forward_enabled: bool = True
    qq_to_game_enabled: bool = True
    player_tracking_enabled: bool = False
    snapshot_sender: str = "snapshot"
    snapshot_prefix: str = "!snap"
    # 以下三项是部署方自己的服务地址，示例值必须在 config.json 里覆盖
    voice_api: str = "https://chatroom.example.com/api/voice/qqbot/get_voice_channel_people"
    status_api: str = "https://status.example.com/api/qqbot/status"
    server_addresses: list[list[str]] = field(
        default_factory=lambda: [["主IP", "game.example.com"]]
    )


@dataclass
class ChatBridgeConfig:
    enabled: bool = True
    host: str = ""
    port: int = 21027
    name: str = "web"
    password: str = ""
    aes_key: str = ""


@dataclass
class CommandsConfig:
    group_allow_all: bool = True
    allow_from: list[int] = field(default_factory=list)
    status_image: bool = False


@dataclass
class AppConfig:
    onebot: OneBotConfig = field(default_factory=OneBotConfig)
    chatroom: ChatroomConfig = field(default_factory=ChatroomConfig)
    chatbridge: ChatBridgeConfig = field(default_factory=ChatBridgeConfig)
    commands: CommandsConfig = field(default_factory=CommandsConfig)
    state_path: str = "/data/state.json"
    log_level: str = "INFO"
    raw: dict[str, Any] = field(default_factory=dict)

    @property
    def group_ids(self) -> set[int]:
        return {int(g) for g in self.chatroom.group_ids}


def _build(cls, data: Any):
    if not isinstance(data, dict):
        return cls()
    return cls(**{k: v for k, v in data.items() if k in cls.__dataclass_fields__})


def load_config(path: str | os.PathLike[str]) -> AppConfig:
    try:
        with open(path, "r", encoding="utf-8-sig") as fh:
            data = json.load(fh)
    except FileNotFoundError as exc:
        raise ConfigError(f"配置文件不存在: {path}") from exc
    except json.JSONDecodeError as exc:
        raise ConfigError(f"配置文件不是合法 JSON: {exc}") from exc

    if not isinstance(data, dict):
        raise ConfigError("配置根节点必须是对象")

    cfg = AppConfig(
        onebot=_build(OneBotConfig, data.get("onebot", {})),
        chatroom=_build(ChatroomConfig, data.get("chatroom", {})),
        chatbridge=_build(ChatBridgeConfig, data.get("chatbridge", {})),
        commands=_build(CommandsConfig, data.get("commands", {})),
        state_path=str(data.get("state_path", "/data/state.json")),
        log_level=str(data.get("log_level", "INFO")).upper(),
        raw=data,
    )
    cfg.chatroom.base_url = cfg.chatroom.base_url.rstrip("/")
    return cfg


def describe(cfg: AppConfig) -> dict[str, Any]:
    """可安全打印的配置摘要（token 一律替换为占位符）。"""
    return {
        "onebot": {
            "listen": f"{cfg.onebot.listen_host}:{cfg.onebot.listen_port}{cfg.onebot.path}",
            "access_token": "***" if cfg.onebot.access_token else "(空)",
            "self_id": cfg.onebot.self_id,
        },
        "chatroom": {
            "base_url": cfg.chatroom.base_url,
            "channel_id": cfg.chatroom.channel_id,
            "forward_token": "***" if cfg.chatroom.forward_token else "(空)",
            "refresh_token": "***" if cfg.chatroom.refresh_token else "(空)",
            "group_ids": sorted(cfg.group_ids),
        },
        "chatbridge": {
            "enabled": cfg.chatbridge.enabled,
            "endpoint": (
                f"{cfg.chatbridge.host}:{cfg.chatbridge.port}" if cfg.chatbridge.host else "(未配置)"
            ),
        },
        "state_path": cfg.state_path,
        "log_level": cfg.log_level,
    }
