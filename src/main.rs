#![windows_subsystem = "windows"]

mod engine;
mod input;
mod install;
mod logging;
mod tray;

use std::ffi::OsStr;
use std::fs;
use std::os::windows::ffi::OsStrExt;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
use windows::core::{PCWSTR, w};

fn main() -> ExitCode {
    let worker_mode = std::env::args().nth(1).as_deref() == Some("--worker");
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            logging::log(format!("致命错误: {error:#}"));
            if !worker_mode {
                show_error(&format!("CapsLockRs 启动失败：\n\n{error:#}"));
            }
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<u8> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    match arguments.as_slice() {
        [] => {
            if install::is_elevated()? {
                install::install_and_start()?;
                Ok(0)
            } else {
                let code = install::relaunch_elevated()?;
                Ok(code.clamp(0, 255) as u8)
            }
        }
        [mode] if mode == "--elevated-install" => {
            install::install_and_start()?;
            Ok(0)
        }
        [mode] if mode == "--worker" => {
            worker_main()?;
            Ok(0)
        }
        _ => bail!("不支持的启动参数"),
    }
}

fn worker_main() -> Result<()> {
    if !install::is_elevated()? {
        bail!("worker 必须由最高权限计划任务启动");
    }
    let app_dir = install::app_dir()?;
    fs::create_dir_all(&app_dir).context("创建应用数据目录失败")?;
    logging::init(&app_dir);

    let Some(instance) = install::acquire_worker_instance()? else {
        logging::log("检测到现有 worker，本次计划任务实例退出");
        return Ok(());
    };
    logging::log(format!("worker 启动，PID={}", std::process::id()));

    let input = input::InputRuntime::start()?;
    let tray_result = tray::run(&input, instance.stop_event());
    input.shutdown();
    drop(instance);
    match tray_result {
        Ok(()) => {
            logging::log("worker 已正常退出");
            Ok(())
        }
        Err(error) => {
            logging::log(format!("worker 因托盘错误退出: {error:#}"));
            Err(error)
        }
    }
}

fn show_error(message: &str) {
    let message: Vec<u16> = OsStr::new(message).encode_wide().chain(Some(0)).collect();
    // SAFETY: strings are valid, null-terminated UTF-16 buffers.
    unsafe {
        let _ = MessageBoxW(
            None,
            PCWSTR(message.as_ptr()),
            w!("CapsLockRs"),
            MB_OK | MB_ICONERROR,
        );
    }
}
