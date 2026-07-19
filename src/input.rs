use std::cell::UnsafeCell;
use std::mem::size_of;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, OnceLock, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use windows::Win32::Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::System::Threading::{
    GetCurrentThread, GetCurrentThreadId, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
    THREAD_PRIORITY_HIGHEST, WaitForSingleObject,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, MOUSE_EVENT_FLAGS, MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEINPUT, SendInput, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT, MSG, PM_NOREMOVE, PeekMessageW,
    PostThreadMessageW, SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_APP, WM_KEYDOWN,
    WM_KEYUP, WM_QUIT, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

use crate::engine::{
    Effect, Engine, KeyAction, MOD_ALT, MOD_CTRL, MOD_SHIFT, VK_CAPITAL, VK_DELETE, VK_DOWN,
    VK_END, VK_F13, VK_F14, VK_F15, VK_HOME, VK_LEFT, VK_RIGHT, VK_UP,
};
use crate::install::WinHandle;
use crate::logging;

const EXTRA_INFO_COOKIE: usize = 0x4350_534C_5253; // "CPSLRS"
const HEALTH_PROBE_COOKIE: usize = 0x4350_534C_4850; // "CPSLHP"
const VK_NONAME: u32 = 0xFC;
const MSG_INJECT: u32 = WM_APP + 1;
const MSG_REHOOK: u32 = WM_APP + 2;
const MSG_RESET: u32 = WM_APP + 3;

const VK_LSHIFT: u32 = 0xA0;
const VK_RSHIFT: u32 = 0xA1;
const VK_LCONTROL: u32 = 0xA2;
const VK_RCONTROL: u32 = 0xA3;
const VK_LMENU: u32 = 0xA4;
const VK_RMENU: u32 = 0xA5;
const VK_LWIN: u32 = 0x5B;
const VK_RWIN: u32 = 0x5C;

#[derive(Default)]
struct HookLocalState {
    engine: Engine,
    modifiers: u8,
}

struct HookShared {
    local: UnsafeCell<HookLocalState>,
    paused: AtomicBool,
    injector_thread: AtomicU32,
    probe_pending: AtomicU32,
    probe_acknowledged: AtomicU32,
    click_active: [AtomicBool; 3],
    click_event: windows::Win32::Foundation::HANDLE,
}

// HookLocalState is touched only by the dedicated hook thread. Other fields are synchronized.
unsafe impl Sync for HookShared {}
unsafe impl Send for HookShared {}

impl HookShared {
    fn clear_clicks(&self) {
        for active in &self.click_active {
            active.store(false, Ordering::Release);
        }
        // SAFETY: click_event is valid until all input threads are joined.
        unsafe {
            let _ = windows::Win32::System::Threading::SetEvent(self.click_event);
        }
    }
}

static HOOK_SHARED: OnceLock<Arc<HookShared>> = OnceLock::new();

pub struct InputRuntime {
    shared: Arc<HookShared>,
    hook_thread_id: u32,
    injector_thread_id: u32,
    shutdown: Arc<AtomicBool>,
    click_event: WinHandle,
    threads: Vec<JoinHandle<()>>,
}

impl InputRuntime {
    pub fn start() -> Result<Self> {
        // SAFETY: creates an unnamed auto-reset event owned by this runtime.
        let click_event = WinHandle::new(unsafe {
            windows::Win32::System::Threading::CreateEventW(None, false, false, None)
                .context("创建连点器事件失败")?
        });
        let shutdown = Arc::new(AtomicBool::new(false));
        let shared = Arc::new(HookShared {
            local: UnsafeCell::new(HookLocalState::default()),
            paused: AtomicBool::new(false),
            injector_thread: AtomicU32::new(0),
            probe_pending: AtomicU32::new(0),
            probe_acknowledged: AtomicU32::new(0),
            click_active: std::array::from_fn(|_| AtomicBool::new(false)),
            click_event: click_event.raw(),
        });

        HOOK_SHARED
            .set(Arc::clone(&shared))
            .map_err(|_| anyhow!("键盘钩子已在本进程中初始化"))?;

        let (injector_tx, injector_rx) = mpsc::sync_channel(1);
        let injector_shutdown = Arc::clone(&shutdown);
        let injector = thread::Builder::new()
            .name("capslockrs-injector".into())
            .spawn(move || injector_thread(injector_tx, injector_shutdown))
            .context("启动输入注入线程失败")?;
        let injector_thread_id = injector_rx.recv().context("输入注入线程初始化失败")??;
        shared
            .injector_thread
            .store(injector_thread_id, Ordering::Release);

        let click_shared = Arc::clone(&shared);
        let click_shutdown = Arc::clone(&shutdown);
        let clicker = thread::Builder::new()
            .name("capslockrs-clicker".into())
            .spawn(move || clicker_thread(click_shared, click_shutdown))
            .context("启动鼠标连点线程失败")?;

        let (hook_tx, hook_rx) = mpsc::sync_channel(1);
        let hook_shutdown = Arc::clone(&shutdown);
        let hook = thread::Builder::new()
            .name("capslockrs-hook".into())
            .spawn(move || hook_thread(hook_tx, hook_shutdown))
            .context("启动键盘钩子线程失败")?;
        let hook_thread_id = hook_rx.recv().context("键盘钩子线程初始化失败")??;

        let watchdog_shutdown = Arc::clone(&shutdown);
        let watchdog_shared = Arc::clone(&shared);
        let watchdog = thread::Builder::new()
            .name("capslockrs-watchdog".into())
            .spawn(move || watchdog_thread(watchdog_shared, watchdog_shutdown, hook_thread_id))
            .context("启动钩子看门狗失败")?;

        Ok(Self {
            shared,
            hook_thread_id,
            injector_thread_id,
            shutdown,
            click_event,
            threads: vec![injector, clicker, hook, watchdog],
        })
    }

    pub fn is_paused(&self) -> bool {
        self.shared.paused.load(Ordering::Acquire)
    }

    pub fn set_paused(&self, paused: bool) {
        self.shared.paused.store(paused, Ordering::Release);
        self.shared.clear_clicks();
        // SAFETY: both target threads have live message queues while InputRuntime is alive.
        unsafe {
            let _ = PostThreadMessageW(self.hook_thread_id, MSG_RESET, WPARAM(0), LPARAM(0));
        }
        logging::log(if paused {
            "键盘映射已暂停"
        } else {
            "键盘映射已恢复"
        });
    }

    pub fn shutdown(mut self) {
        self.shutdown.store(true, Ordering::Release);
        self.shared.clear_clicks();
        // SAFETY: message queues and event remain live until joins complete.
        unsafe {
            let _ = PostThreadMessageW(self.hook_thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
            let _ = PostThreadMessageW(self.injector_thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
            let _ = windows::Win32::System::Threading::SetEvent(self.click_event.raw());
        }
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

fn injector_thread(ready: mpsc::SyncSender<Result<u32>>, shutdown: Arc<AtomicBool>) {
    // SAFETY: retrieving and setting the current thread priority affects only this worker.
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL);
    }
    let mut message = MSG::default();
    // SAFETY: PeekMessage creates this thread's message queue.
    unsafe {
        let _ = PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE);
    }
    // SAFETY: returns this thread's stable system identifier.
    let id = unsafe { GetCurrentThreadId() };
    if ready.send(Ok(id)).is_err() {
        return;
    }

    while !shutdown.load(Ordering::Acquire) {
        // SAFETY: message points to writable storage and this is the thread's message loop.
        let result = unsafe { GetMessageW(&mut message, None, 0, 0) };
        if result.0 <= 0 {
            break;
        }
        if message.message == MSG_INJECT {
            let action = KeyAction {
                key: message.wParam.0 as u32,
                modifiers: (message.lParam.0 as u32 & 0xFF) as u8,
                repeat: ((message.lParam.0 as u32 >> 8) & 0xFF) as u8,
            };
            inject_key_action(action);
        }
    }
}

fn hook_thread(ready: mpsc::SyncSender<Result<u32>>, shutdown: Arc<AtomicBool>) {
    // SAFETY: retrieving and setting the current thread priority affects only this worker.
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
    }
    let mut message = MSG::default();
    // SAFETY: PeekMessage creates the message queue before publishing the thread id.
    unsafe {
        let _ = PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE);
    }
    // SAFETY: stable identifier for the current thread.
    let id = unsafe { GetCurrentThreadId() };
    reset_hook_state();
    let mut hook = match install_hook() {
        Ok(hook) => hook,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    if ready.send(Ok(id)).is_err() {
        // SAFETY: hook was installed by this thread.
        unsafe {
            let _ = UnhookWindowsHookEx(hook);
        }
        return;
    }

    while !shutdown.load(Ordering::Acquire) {
        // SAFETY: standard message loop for the hook thread.
        let result = unsafe { GetMessageW(&mut message, None, 0, 0) };
        if result.0 <= 0 {
            break;
        }
        match message.message {
            MSG_REHOOK => {
                // SAFETY: hook is owned by this thread.
                unsafe {
                    let _ = UnhookWindowsHookEx(hook);
                }
                let mut replacement = None;
                for attempt in 1..=20 {
                    match install_hook() {
                        Ok(new_hook) => {
                            replacement = Some(new_hook);
                            break;
                        }
                        Err(error) => {
                            logging::log(format!(
                                "重装键盘钩子失败（第 {attempt}/20 次）: {error:#}"
                            ));
                            thread::sleep(Duration::from_millis(250));
                        }
                    }
                }
                if let Some(new_hook) = replacement {
                    hook = new_hook;
                } else {
                    logging::log("键盘钩子连续重装失败，退出以交由计划任务恢复");
                    std::process::exit(2);
                }
                reset_hook_state();
            }
            MSG_RESET => reset_hook_state(),
            _ => {}
        }
    }
    // SAFETY: hook is owned by this thread and no callback runs after unhook returns.
    unsafe {
        let _ = UnhookWindowsHookEx(hook);
    }
}

fn install_hook() -> Result<HHOOK> {
    // SAFETY: retrieves the module containing this callback.
    let module = unsafe { GetModuleHandleW(None) }.context("获取程序模块句柄失败")?;
    // SAFETY: low_level_keyboard_proc has the required lifetime and ABI.
    unsafe {
        SetWindowsHookExW(
            WH_KEYBOARD_LL,
            Some(low_level_keyboard_proc),
            Some(HINSTANCE(module.0)),
            0,
        )
    }
    .context("安装低级键盘钩子失败")
}

fn reset_hook_state() {
    let Some(shared) = HOOK_SHARED.get() else {
        return;
    };
    // SAFETY: invoked only on the hook thread while no hook callback is executing.
    let local = unsafe { &mut *shared.local.get() };
    let enabled = !shared.paused.load(Ordering::Acquire);
    let clicks = std::array::from_fn(|index| enabled && physical_click_key_down(index));
    // SAFETY: GetTickCount64 is side-effect free and matches KBDLLHOOKSTRUCT's tick epoch.
    let timestamp = unsafe { GetTickCount64() } as u32;
    local
        .engine
        .synchronize_physical(enabled && key_down(VK_CAPITAL), clicks, timestamp);
    local.modifiers = current_modifier_mask();
    for (active, value) in shared.click_active.iter().zip(clicks) {
        active.store(value, Ordering::Release);
    }
    // SAFETY: wakes the clicker so reconciled state takes effect immediately.
    unsafe {
        let _ = windows::Win32::System::Threading::SetEvent(shared.click_event);
    }
}

unsafe extern "system" fn low_level_keyboard_proc(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if code != HC_ACTION as i32 {
        // SAFETY: required forwarding for hook codes we do not handle.
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let Some(shared) = HOOK_SHARED.get() else {
            return false;
        };
        // SAFETY: Windows supplies a valid KBDLLHOOKSTRUCT for HC_ACTION.
        let event = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        if event.dwExtraInfo == HEALTH_PROBE_COOKIE {
            let generation = shared.probe_pending.load(Ordering::Acquire);
            shared
                .probe_acknowledged
                .store(generation, Ordering::Release);
            return true;
        }
        if event.dwExtraInfo == EXTRA_INFO_COOKIE {
            return false;
        }
        let is_down = wparam.0 == WM_KEYDOWN as usize || wparam.0 == WM_SYSKEYDOWN as usize;
        let is_up = wparam.0 == WM_KEYUP as usize || wparam.0 == WM_SYSKEYUP as usize;
        if !is_down && !is_up {
            return false;
        }

        // SAFETY: the low-level callback is serialized onto the installing hook thread.
        let local = unsafe { &mut *shared.local.get() };
        update_modifier_mask(&mut local.modifiers, event.vkCode, is_down);
        if shared.paused.load(Ordering::Acquire) {
            return false;
        }

        let outcome = local
            .engine
            .process(event.vkCode, is_down, event.time, local.modifiers != 0);
        if let Some(effect) = outcome.effect {
            match effect {
                Effect::Key(action) => {
                    let packed = action.modifiers as isize | ((action.repeat as isize) << 8);
                    let target = shared.injector_thread.load(Ordering::Acquire);
                    if target != 0 {
                        // SAFETY: the injector owns a live message queue.
                        unsafe {
                            let _ = PostThreadMessageW(
                                target,
                                MSG_INJECT,
                                WPARAM(action.key as usize),
                                LPARAM(packed),
                            );
                        }
                    }
                }
                Effect::Click { index, active } => {
                    shared.click_active[index].store(active, Ordering::Release);
                    // SAFETY: the click event remains valid for the runtime lifetime.
                    unsafe {
                        let _ = windows::Win32::System::Threading::SetEvent(shared.click_event);
                    }
                }
            }
        }
        outcome.suppress
    }))
    .unwrap_or(false);

    if result {
        LRESULT(1)
    } else {
        // SAFETY: unhandled events must continue through the hook chain.
        unsafe { CallNextHookEx(None, code, wparam, lparam) }
    }
}

fn clicker_thread(shared: Arc<HookShared>, shutdown: Arc<AtomicBool>) {
    // SAFETY: affects only this low-duty input worker.
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL);
    }
    while !shutdown.load(Ordering::Acquire) {
        // SAFETY: click_event is a valid auto-reset event.
        unsafe { WaitForSingleObject(shared.click_event, u32::MAX) };
        if shutdown.load(Ordering::Acquire) {
            break;
        }
        loop {
            let mut any = false;
            for (index, active) in shared.click_active.iter().enumerate() {
                if active.load(Ordering::Acquire) {
                    any = true;
                    inject_mouse_click(index);
                }
            }
            if !any || shutdown.load(Ordering::Acquire) {
                break;
            }
            // A state transition wakes this wait early; otherwise it supplies the 10 ms cadence.
            // SAFETY: click_event remains valid.
            unsafe { WaitForSingleObject(shared.click_event, 10) };
        }
    }
}

fn watchdog_thread(shared: Arc<HookShared>, shutdown: Arc<AtomicBool>, hook_thread_id: u32) {
    let mut generation = 0_u32;
    let mut confirmed_once = false;
    while !shutdown.load(Ordering::Acquire) {
        for _ in 0..5 {
            if shutdown.load(Ordering::Acquire) {
                return;
            }
            thread::sleep(Duration::from_secs(1));
        }

        generation = generation.wrapping_add(1).max(1);
        shared.probe_pending.store(generation, Ordering::Release);
        if !inject_health_probe() {
            logging::log("键盘钩子健康探针注入失败，本轮不执行重装");
            continue;
        }

        let mut acknowledged = false;
        for _ in 0..20 {
            if shutdown.load(Ordering::Acquire) {
                return;
            }
            if shared.probe_acknowledged.load(Ordering::Acquire) == generation {
                acknowledged = true;
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }

        if acknowledged {
            if !confirmed_once {
                logging::log("键盘钩子健康探针已确认");
                confirmed_once = true;
            }
            continue;
        }

        confirmed_once = false;
        logging::log("键盘钩子健康探针超时，正在重装钩子");
        // SAFETY: the hook thread owns a live Win32 message queue.
        unsafe {
            let _ = PostThreadMessageW(hook_thread_id, MSG_REHOOK, WPARAM(0), LPARAM(0));
        }
    }
}

fn inject_health_probe() -> bool {
    let inputs = [
        keyboard_input_with_cookie(VK_NONAME, false, HEALTH_PROBE_COOKIE),
        keyboard_input_with_cookie(VK_NONAME, true, HEALTH_PROBE_COOKIE),
    ];
    // SAFETY: both INPUT records are fully initialized keyboard events.
    (unsafe { SendInput(&inputs, size_of::<INPUT>() as i32) }) == inputs.len() as u32
}

fn inject_key_action(action: KeyAction) {
    let groups = [
        ([VK_LCONTROL, VK_RCONTROL], MOD_CTRL),
        ([VK_LSHIFT, VK_RSHIFT], MOD_SHIFT),
        ([VK_LMENU, VK_RMENU], MOD_ALT),
        ([VK_LWIN, VK_RWIN], 0),
    ];
    let mut prefix = Vec::with_capacity(10);
    let mut suffix = Vec::with_capacity(10);

    for (keys, modifier) in groups {
        let down = [key_down(keys[0]), key_down(keys[1])];
        let wanted = modifier != 0 && action.modifiers & modifier != 0;
        if wanted {
            if !down[0] && !down[1] {
                prefix.push(keyboard_input(keys[0], false));
                suffix.insert(0, keyboard_input(keys[0], true));
            }
        } else {
            for (key, was_down) in keys.into_iter().zip(down) {
                if was_down {
                    prefix.push(keyboard_input(key, true));
                    suffix.insert(0, keyboard_input(key, false));
                }
            }
        }
    }

    let mut inputs = prefix;
    for _ in 0..action.repeat.max(1) {
        inputs.push(keyboard_input(action.key, false));
        inputs.push(keyboard_input(action.key, true));
    }
    inputs.extend(suffix);
    // SAFETY: INPUT slice contains fully initialized keyboard events.
    let sent = unsafe { SendInput(&inputs, size_of::<INPUT>() as i32) };
    if sent != inputs.len() as u32 {
        logging::log(format!("SendInput 键盘注入不完整: {sent}/{}", inputs.len()));
    }
}

fn inject_mouse_click(index: usize) {
    let (down, up) = match index {
        0 => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
        1 => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
        _ => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
    };
    let inputs = [mouse_input(down), mouse_input(up)];
    // SAFETY: both INPUT records are initialized mouse events.
    let sent = unsafe { SendInput(&inputs, size_of::<INPUT>() as i32) };
    if sent != inputs.len() as u32 {
        logging::log(format!("SendInput 鼠标注入不完整: {sent}/2"));
    }
}

fn keyboard_input(vk: u32, key_up: bool) -> INPUT {
    keyboard_input_with_cookie(vk, key_up, EXTRA_INFO_COOKIE)
}

fn keyboard_input_with_cookie(vk: u32, key_up: bool, cookie: usize) -> INPUT {
    let mut flags = if key_up {
        KEYEVENTF_KEYUP
    } else {
        Default::default()
    };
    if is_extended(vk) {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk as u16),
                dwFlags: flags,
                dwExtraInfo: cookie,
                ..Default::default()
            },
        },
    }
}

fn mouse_input(flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dwFlags: flags,
                dwExtraInfo: EXTRA_INFO_COOKIE,
                ..Default::default()
            },
        },
    }
}

fn is_extended(vk: u32) -> bool {
    matches!(
        vk,
        VK_LEFT
            | VK_RIGHT
            | VK_UP
            | VK_DOWN
            | VK_HOME
            | VK_END
            | VK_DELETE
            | VK_RCONTROL
            | VK_RMENU
    )
}

fn key_down(vk: u32) -> bool {
    // SAFETY: GetAsyncKeyState accepts every virtual-key code in the u8 range used here.
    unsafe { GetAsyncKeyState(vk as i32) < 0 }
}

fn physical_click_key_down(index: usize) -> bool {
    key_down([VK_F13, VK_F14, VK_F15][index])
}

fn update_modifier_mask(mask: &mut u8, vk: u32, is_down: bool) {
    let bit = match vk {
        VK_LSHIFT => 1,
        VK_RSHIFT => 2,
        VK_LCONTROL => 4,
        VK_RCONTROL => 8,
        VK_LMENU => 16,
        VK_RMENU => 32,
        VK_LWIN => 64,
        VK_RWIN => 128,
        _ => return,
    };
    if is_down {
        *mask |= bit;
    } else {
        *mask &= !bit;
    }
}

fn current_modifier_mask() -> u8 {
    let mut mask = 0;
    for vk in [
        VK_LSHIFT,
        VK_RSHIFT,
        VK_LCONTROL,
        VK_RCONTROL,
        VK_LMENU,
        VK_RMENU,
        VK_LWIN,
        VK_RWIN,
    ] {
        update_modifier_mask(&mut mask, vk, key_down(vk));
    }
    mask
}
