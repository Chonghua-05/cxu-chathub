//! 本地渲染状态图（开发 / 视觉对比用）：
//!   cargo run --release --example render_status -- out.png
//! 使用与线上 /server 相同的样例数据（含 3 条线路 + 8 台服务器）。
use chatroom_bridge::services::status_render::{render, render_bulletin_png, Block};
use serde_json::json;

fn main() {
    let mut args = std::env::args().skip(1);
    let out = args.next().unwrap_or_else(|| "status.png".to_string());
    let mode = args.next().unwrap_or_default();
    if mode == "bulletin" {
        render_bulletin(&out);
        return;
    }
    let data = json!({
        "timestamp": "2026-09-30T09:26:31",
        "network_routes": [
            {"route_name": "RMS主线路", "online": true, "latency": 41.13, "packet_loss": 0.0},
            {"route_name": "RMS海外加速线路", "online": true, "latency": 27.59, "packet_loss": 0.0},
            {"route_name": "RMS 备用线路", "online": true, "latency": 79.45, "packet_loss": 0.0}
        ],
        "servers": [
            {"server_name": "cra-creative", "online": true, "online_players": []},
            {"server_name": "cra-mirror1", "online": true, "online_players": []},
            {"server_name": "cra-survival", "online": true, "online_players": []},
            {"server_name": "creative1", "online": true, "online_players": []},
            {"server_name": "creative2", "online": false, "online_players": []},
            {"server_name": "mirror1", "online": true, "online_players": []},
            {"server_name": "mirror2", "online": false, "online_players": []},
            {"server_name": "survival", "online": true, "online_players": ["bot_sleep", "bot_ms_end"]}
        ]
    });
    let addresses = vec![
        ("主IP".to_string(), "game.cxu.org.cn".to_string()),
        ("海外加速IP（中国香港）".to_string(), "hk-game.cxu.org.cn".to_string()),
        ("备用地址".to_string(), "backup.cxu.org.cn:5555".to_string()),
    ];
    match render(&data, Some(&addresses)) {
        Ok(png) => {
            std::fs::write(&out, &png).expect("写文件失败");
            eprintln!("已写出 {out}（{} 字节）", png.len());
        }
        Err(err) => {
            eprintln!("渲染失败: {err}");
            std::process::exit(1);
        }
    }
}

/// 渲染一张 v0.5 播报示例长图（黑底白字）。
fn render_bulletin(out: &str) {
    let title = "Minecraft 1.21.4";
    let blocks = vec![
        Block::Paragraph("本次快照更新了若干内容，以下为变更摘要。".into()),
        Block::Title("新特性".into()),
        Block::Item("加入了新的生物群系与方块变种。".into()),
        Block::Item("改进了性能与内存占用。".into()),
        Block::Title("修复".into()),
        Block::Item("修复了多人游戏中偶发的物品同步错误。".into()),
    ];
    match render_bulletin_png(title, &blocks) {
        Ok(png) => {
            std::fs::write(out, &png).expect("写文件失败");
            eprintln!("已写出 {out}（{} 字节）", png.len());
        }
        Err(err) => {
            eprintln!("渲染失败: {err}");
            std::process::exit(1);
        }
    }
}
