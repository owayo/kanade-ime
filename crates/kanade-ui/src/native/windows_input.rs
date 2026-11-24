use super::*;
use std::{
    os::windows::process::CommandExt,
    path::PathBuf,
    process::{Command, Stdio},
};

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum DefaultInput {
    #[default]
    Unknown,
    Reading,
    Other,
    Kanade,
    Saving,
    Failed,
}

// Use the Windows PowerShell International module shipped with the OS. Only an
// explicit button press sets the per-user override. Both paths read it back.
fn windows_default_input(apply: bool) -> Option<bool> {
    let executable = PathBuf::from(std::env::var_os("SystemRoot")?)
        .join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
    let script = include_str!("default_input.ps1");
    let command = format!("& {{ {script}\n}} ${apply}");
    let status = Command::new(executable)
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &command,
        ])
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;
    match status.code() {
        Some(0) => Some(true),
        Some(3) => Some(false),
        _ => None,
    }
}

impl Settings {
    pub(super) fn update_default_input(&mut self, apply: bool, cx: &mut Context<Self>) {
        use DefaultInput::*;
        if matches!(self.default_input, Reading | Saving) {
            return;
        }
        self.default_input = if apply { Saving } else { Reading };
        let task = cx.background_spawn(async move { windows_default_input(apply) });
        cx.spawn(async move |view, cx| {
            let result = task.await;
            let _ = view.update(cx, |this, cx| {
                this.default_input = match result {
                    Some(true) => Kanade,
                    Some(false) if !apply => Other,
                    _ if apply => Failed,
                    _ => Unknown,
                };
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn default_input_section(&self, cx: &mut Context<Self>) -> gpui::Div {
        use DefaultInput::*;
        let detail = match self.default_input {
            Unknown => "現在の既定の入力方式を確認できませんでした。",
            Reading => "Windows の設定を確認しています。",
            Other => "奏を既定にすると、次回のサインインから奏で入力できます。",
            Kanade => "奏が既定の入力方式に設定されています。",
            Saving => "奏を既定の入力方式に設定しています。",
            Failed => {
                "変更を確認できませんでした。Windows の「既定の入力方式の上書き」で奏を選んでください。"
            }
        };
        section("Windows の既定の入力方式").child(
            card().p_3().gap_2().child(detail).child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .when(self.default_input != Kanade, |row| {
                        row.child(
                            button(
                                "default-input-kanade",
                                "奏を既定にする",
                                "奏を既定の入力方式にする",
                            )
                            .inactive(matches!(self.default_input, Reading | Saving))
                            .on_click(
                                cx.listener(|this, _, _, cx| this.update_default_input(true, cx)),
                            ),
                        )
                    })
                    .child(
                        button(
                            "windows-keyboard-settings",
                            "Windows の設定を開く",
                            "Windows のキーボードの詳細設定を開く",
                        )
                        .on_click(|_, _, cx| cx.open_url("ms-settings:keyboard-advanced")),
                    ),
            ),
        )
    }
}
