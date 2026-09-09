use anyhow::{Context, Result};
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

const TERMINALS: &[&str] = &[
    "kitty",
    "alacritty",
    "foot",
    "wezterm",
    "org.wezfurlong.wezterm",
    "gnome-terminal",
    "konsole",
    "st",
    "xterm",
    "urxvt",
    "rio",
    "ghostty",
    "com.mitchellh.ghostty",
];

/// Clipboard + paste keystroke, with a single short settle instead of the
/// that were there for Python's slower clipboard handoff.
pub fn inject(text: &str, preserve_clipboard: bool) -> Result<()> {
    anyhow::ensure!(!text.is_empty(), "nothing to inject");

    let previous = if preserve_clipboard {
        Command::new("wl-paste")
            .arg("--no-newline")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| o.stdout)
    } else {
        None
    };

    write_clipboard(text.as_bytes()).context("wl-copy failed")?;
    std::thread::sleep(Duration::from_millis(60));
    paste()?;

    if let Some(prev) = previous {
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(400));
            let _ = write_clipboard(&prev);
        });
    }
    Ok(())
}

fn write_clipboard(data: &[u8]) -> Result<()> {
    // Plain wl-copy daemonizes itself and exits, so waiting here neither blocks
    // nor leaves a zombie.
    let mut child = Command::new("wl-copy")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    child.stdin.take().context("no stdin")?.write_all(data)?;
    anyhow::ensure!(child.wait()?.success(), "wl-copy exited non-zero");
    Ok(())
}

fn paste() -> Result<()> {
    let term = is_terminal();
    let mods = if term { "CTRL SHIFT" } else { "CTRL" };
    let lua = format!("hl.dispatch(hl.dsp.send_shortcut({{ mods = '{mods}', key = 'v' }}))");
    if run(Command::new("hyprctl").args(["eval", &lua])) {
        return Ok(());
    }
    if run(Command::new("hyprctl").args(["dispatch", "sendshortcut", &format!("{mods}, v, ")])) {
        return Ok(());
    }
    let key = if term { "V" } else { "v" };
    let mut cmd = Command::new("wtype");
    cmd.args(["-M", "ctrl"]);
    if term {
        cmd.args(["-M", "shift"]);
    }
    cmd.args(["-k", key, "-m", "ctrl"]);
    if term {
        cmd.args(["-m", "shift"]);
    }
    anyhow::ensure!(run(&mut cmd), "no paste method worked (hyprctl, wtype)");
    Ok(())
}

fn run(cmd: &mut Command) -> bool {
    cmd.stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn is_terminal() -> bool {
    let out = Command::new("hyprctl")
        .args(["activewindow", "-j"])
        .output()
        .ok()
        .filter(|o| o.status.success());
    let Some(out) = out else { return false };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else {
        return false;
    };
    let class = v["class"].as_str().unwrap_or_default().to_lowercase();
    TERMINALS.contains(&class.as_str())
}
