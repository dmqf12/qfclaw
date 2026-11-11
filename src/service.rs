//! 自动配置用户级 systemd 服务。
//!
//! 启动时若 `~/.config/systemd/user/qfclaw.service` 不存在，就按当前运行目录
//! 生成一个直接运行本程序的服务单元（`Type=simple`，不再依赖 tmux），
//! 然后执行 `daemon-reload` 与 `enable`。已存在时完全不动，避免覆盖用户的定制。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::log;

/// 服务名（与 unit 文件名一致，`/restart` 也用它）。
const SERVICE_NAME: &str = "qfclaw.service";

/// 确保用户级服务存在；已存在时什么都不做。
pub fn ensure() {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        log::warn("service", "读不到 HOME，跳过 systemd 服务配置");
        return;
    };
    let unit = home.join(".config/systemd/user").join(SERVICE_NAME);
    if unit.exists() {
        log::info(
            "service",
            format!("已存在系统服务 {}，保持原样", unit.display()),
        );
        return;
    }

    let Ok(dir) = std::env::current_dir() else {
        log::warn("service", "读不到当前目录，跳过 systemd 服务配置");
        return;
    };
    let exe = match std::env::current_exe() {
        Ok(path) => path,
        Err(e) => {
            log::warn("service", format!("读不到可执行文件路径，跳过配置：{e}"));
            return;
        }
    };

    if let Some(parent) = unit.parent()
        && let Err(e) = fs::create_dir_all(parent)
    {
        log::warn(
            "service",
            format!("创建目录 {} 失败：{e}", parent.display()),
        );
        return;
    }
    if let Err(e) = fs::write(&unit, unit_text(&dir, &exe)) {
        log::warn(
            "service",
            format!("写入服务文件 {} 失败：{e}", unit.display()),
        );
        return;
    }
    log::info(
        "service",
        format!(
            "已生成系统服务 {}（目录 {}，程序 {}）",
            unit.display(),
            dir.display(),
            exe.display()
        ),
    );
    reload_and_enable();
}

/// 生成 unit 内容。
fn unit_text(dir: &Path, exe: &Path) -> String {
    format!(
        "\
[Unit]
Description=qfclaw AI Agent
After=network-online.target

[Service]
Type=simple
WorkingDirectory={}
ExecStart={}
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
",
        dir.display(),
        exe.display()
    )
}

/// 让 systemd 重新加载并设置为开机自启。
fn reload_and_enable() {
    run_systemctl(&["--user", "daemon-reload"], "daemon-reload");
    run_systemctl(&["--user", "enable", SERVICE_NAME], "enable");
}

/// 执行一条 systemctl 命令，把退出码与错误输出写进日志。
fn run_systemctl(args: &[&str], what: &str) {
    match Command::new("systemctl").args(args).output() {
        Ok(out) if out.status.success() => {
            log::info("service", format!("systemctl {what} 成功"));
        }
        Ok(out) => log::warn(
            "service",
            format!(
                "systemctl {what} 失败（退出码 {}）：{}",
                out.status.code().unwrap_or(-1),
                log::preview(&String::from_utf8_lossy(&out.stderr), 200)
            ),
        ),
        Err(e) => log::warn("service", format!("无法执行 systemctl {what}：{e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_text_contains_paths() {
        let text = unit_text(Path::new("/opt/qfclaw"), Path::new("/opt/qfclaw/qfclaw"));
        assert!(text.contains("Type=simple"));
        assert!(!text.contains("tmux"));
        assert!(text.contains("WorkingDirectory=/opt/qfclaw\n"));
        assert!(text.contains("ExecStart=/opt/qfclaw/qfclaw\n"));
        assert!(text.contains("WantedBy=default.target"));
    }
}
