use std::cell::Cell;
use std::ffi::OsStr;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Context, Result, anyhow};
use windows::Win32::Foundation::{
    HINSTANCE, HWND, LPARAM, LRESULT, POINT, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT, WPARAM,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION,
    NOTIFYICON_VERSION_4, NOTIFYICONDATAW, NOTIFYICONDATAW_0, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, GetCursorPos, IDI_APPLICATION, LoadIconW, MF_CHECKED, MF_SEPARATOR,
    MF_STRING, MF_UNCHECKED, MSG, MessageBoxW, MsgWaitForMultipleObjects, PM_REMOVE, PeekMessageW,
    PostMessageW, PostQuitMessage, QS_ALLINPUT, RegisterClassW, RegisterWindowMessageW,
    SetForegroundWindow, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage,
    WINDOW_EX_STYLE, WM_APP, WM_CONTEXTMENU, WM_DESTROY, WM_NULL, WM_QUIT, WM_RBUTTONUP, WNDCLASSW,
    WS_OVERLAPPED,
};
use windows::core::{PCWSTR, w};

use crate::input::InputRuntime;
use crate::{install, logging};

const TRAY_CALLBACK: u32 = WM_APP + 100;
const TRAY_ICON_ID: u32 = 1;
const CMD_PAUSE: usize = 1001;
const CMD_AUTOSTART: usize = 1002;
const CMD_EXIT: usize = 1003;
const TRAY_RETRY_INTERVAL_MS: u32 = 1_000;

static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

thread_local! {
    static MENU_REQUESTED: Cell<bool> = const { Cell::new(false) };
    static TASKBAR_RECREATED: Cell<bool> = const { Cell::new(false) };
}

struct TrayGuard(HWND);

impl Drop for TrayGuard {
    fn drop(&mut self) {
        MENU_REQUESTED.with(|flag| flag.set(false));
        TASKBAR_RECREATED.with(|flag| flag.set(false));
        delete_icon(self.0);
        // SAFETY: this guard is created immediately after the window. Destroying an
        // already-destroyed window only fails and is safe to ignore.
        unsafe {
            let _ = DestroyWindow(self.0);
        }
    }
}

enum MenuResult {
    None,
    IconMissing,
    Exit,
}

pub fn run(runtime: &InputRuntime, stop_event: windows::Win32::Foundation::HANDLE) -> Result<()> {
    // SAFETY: retrieves this executable's module.
    let module = unsafe { GetModuleHandleW(None) }.context("获取托盘模块句柄失败")?;
    let instance = HINSTANCE(module.0);
    // SAFETY: registers a process-unique hidden window class.
    let atom = unsafe {
        RegisterClassW(&WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: w!("CapsLockRs.TrayWindow"),
            ..Default::default()
        })
    };
    if atom == 0 {
        return Err(anyhow!("注册托盘窗口类失败"));
    }
    // SAFETY: class and instance are valid; the window remains hidden.
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("CapsLockRs.TrayWindow"),
            w!("CapsLockRs"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance),
            None,
        )
    }
    .context("创建托盘消息窗口失败")?;
    let _tray_guard = TrayGuard(hwnd);

    // SAFETY: the string is static and valid.
    let taskbar_created = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
    if taskbar_created == 0 {
        return Err(anyhow!("注册 TaskbarCreated 消息失败"));
    }
    TASKBAR_CREATED.store(taskbar_created, Ordering::Release);
    let mut icon_added = match add_icon(hwnd, runtime.is_paused()) {
        Ok(()) => true,
        Err(error) => {
            logging::log(format!("系统托盘尚未就绪，将每秒重试: {error:#}"));
            false
        }
    };

    let handles = [stop_event];
    let mut message = MSG::default();
    'outer: loop {
        let timeout = if icon_added {
            u32::MAX
        } else {
            TRAY_RETRY_INTERVAL_MS
        };
        // SAFETY: stop_event is valid and this thread owns the window message queue.
        let wait =
            unsafe { MsgWaitForMultipleObjects(Some(&handles), false, timeout, QS_ALLINPUT) };
        if wait == WAIT_OBJECT_0 {
            break;
        }
        if wait == WAIT_TIMEOUT {
            if add_icon(hwnd, runtime.is_paused()).is_ok() {
                icon_added = true;
                logging::log("系统托盘已就绪，图标添加成功");
            }
            continue;
        }
        if wait == WAIT_FAILED {
            return Err(windows::core::Error::from_thread()).context("等待托盘消息或停止事件失败");
        }
        if wait.0 == WAIT_OBJECT_0.0 + handles.len() as u32 {
            // SAFETY: standard non-blocking message drain for this thread.
            while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
                if message.message == WM_QUIT {
                    break 'outer;
                }
                // SAFETY: message came from this thread's queue.
                unsafe {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }

            if MENU_REQUESTED.with(|flag| flag.replace(false)) {
                match show_menu(hwnd, runtime) {
                    MenuResult::None => {}
                    MenuResult::IconMissing => {
                        delete_icon(hwnd);
                        icon_added = false;
                    }
                    MenuResult::Exit => break 'outer,
                }
            }
            if TASKBAR_RECREATED.with(|flag| flag.replace(false)) {
                icon_added = false;
                logging::log("检测到任务栏已重新创建，正在恢复托盘图标");
            }
            if !icon_added && add_icon(hwnd, runtime.is_paused()).is_ok() {
                icon_added = true;
                logging::log("系统托盘图标添加成功");
            }
        } else {
            return Err(anyhow!("托盘消息循环返回了未知等待状态"));
        }
    }
    Ok(())
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        if message == TRAY_CALLBACK {
            let event = (lparam.0 as u32) & 0xFFFF;
            if event == WM_RBUTTONUP || event == WM_CONTEXTMENU {
                MENU_REQUESTED.with(|flag| flag.set(true));
            }
            return Some(LRESULT(0));
        }
        if message == TASKBAR_CREATED.load(Ordering::Acquire) && message != 0 {
            TASKBAR_RECREATED.with(|flag| flag.set(true));
            return Some(LRESULT(0));
        }
        if message == WM_DESTROY {
            // SAFETY: ends this thread's tray message loop.
            unsafe { PostQuitMessage(0) };
            return Some(LRESULT(0));
        }
        None
    }))
    .ok()
    .flatten();

    result.unwrap_or_else(|| {
        // SAFETY: default processing for messages we did not handle.
        unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
    })
}

fn show_menu(hwnd: HWND, runtime: &InputRuntime) -> MenuResult {
    // SAFETY: creates a menu owned and destroyed within this function.
    let Ok(menu) = (unsafe { CreatePopupMenu() }) else {
        return MenuResult::None;
    };
    let pause_flags = MF_STRING
        | if runtime.is_paused() {
            MF_CHECKED
        } else {
            MF_UNCHECKED
        };
    let autostart_flags = MF_STRING
        | if install::autostart_enabled() {
            MF_CHECKED
        } else {
            MF_UNCHECKED
        };
    // SAFETY: menu is valid and labels are static null-terminated strings.
    unsafe {
        let _ = AppendMenuW(menu, pause_flags, CMD_PAUSE, w!("暂停"));
        let _ = AppendMenuW(menu, autostart_flags, CMD_AUTOSTART, w!("开机启动"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, CMD_EXIT, w!("退出"));
    }
    let mut point = POINT::default();
    // SAFETY: point is a valid output and hwnd is live.
    unsafe {
        let _ = GetCursorPos(&mut point);
        let _ = SetForegroundWindow(hwnd);
    }
    // SAFETY: menu and window remain valid during the modal tracking operation.
    let selected = unsafe {
        TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON,
            point.x,
            point.y,
            None,
            hwnd,
            None,
        )
    }
    .0 as usize;
    // SAFETY: releases the temporary menu and lets the shell dismiss it correctly.
    unsafe {
        let _ = DestroyMenu(menu);
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
    }

    match selected {
        CMD_PAUSE => {
            let paused = !runtime.is_paused();
            runtime.set_paused(paused);
            if update_tooltip(hwnd, paused).is_err() {
                return MenuResult::IconMissing;
            }
        }
        CMD_AUTOSTART => {
            let enable = !install::autostart_enabled();
            if let Err(error) = install::set_autostart(enable) {
                logging::log(format!("切换开机启动失败: {error:#}"));
                show_error(&format!("切换开机启动失败：\n{error:#}"));
            }
        }
        CMD_EXIT => {
            logging::log("用户从托盘退出");
            return MenuResult::Exit;
        }
        _ => {}
    }
    MenuResult::None
}

fn add_icon(hwnd: HWND, paused: bool) -> Result<()> {
    // SAFETY: loads a shared stock application icon; it must not be destroyed.
    let icon = unsafe { LoadIconW(None, IDI_APPLICATION) }.context("加载托盘图标失败")?;
    let mut data = tray_data(hwnd, paused);
    data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    data.hIcon = icon;
    // SAFETY: data is fully initialized and hwnd is live.
    if !unsafe { Shell_NotifyIconW(NIM_ADD, &data) }.as_bool() {
        return Err(anyhow!("添加系统托盘图标失败"));
    }
    data.Anonymous = NOTIFYICONDATAW_0 {
        uVersion: NOTIFYICON_VERSION_4,
    };
    // SAFETY: switches callback semantics to NOTIFYICON_VERSION_4.
    unsafe {
        let _ = Shell_NotifyIconW(NIM_SETVERSION, &data);
    }
    Ok(())
}

fn update_tooltip(hwnd: HWND, paused: bool) -> Result<()> {
    let mut data = tray_data(hwnd, paused);
    data.uFlags = NIF_TIP;
    // SAFETY: modifies only this application's icon identifier.
    if unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) }.as_bool() {
        Ok(())
    } else {
        Err(anyhow!("更新托盘提示失败"))
    }
}

fn delete_icon(hwnd: HWND) {
    let data = tray_data(hwnd, false);
    // SAFETY: deleting a nonexistent icon is harmless.
    unsafe {
        let _ = Shell_NotifyIconW(NIM_DELETE, &data);
    }
}

fn tray_data(hwnd: HWND, paused: bool) -> NOTIFYICONDATAW {
    let mut data = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ICON_ID,
        uCallbackMessage: TRAY_CALLBACK,
        ..Default::default()
    };
    let text = if paused {
        "CapsLockRs（已暂停）"
    } else {
        "CapsLockRs"
    };
    copy_wide(&mut data.szTip, text);
    data
}

fn copy_wide(destination: &mut [u16], text: &str) {
    let source: Vec<u16> = OsStr::new(text).encode_wide().collect();
    let count = source.len().min(destination.len().saturating_sub(1));
    destination[..count].copy_from_slice(&source[..count]);
    destination[count] = 0;
}

fn show_error(message: &str) {
    let message: Vec<u16> = OsStr::new(message).encode_wide().chain(Some(0)).collect();
    // SAFETY: both strings are valid and null terminated.
    unsafe {
        let _ = MessageBoxW(
            None,
            PCWSTR(message.as_ptr()),
            w!("CapsLockRs"),
            windows::Win32::UI::WindowsAndMessaging::MB_OK
                | windows::Win32::UI::WindowsAndMessaging::MB_ICONERROR,
        );
    }
}
