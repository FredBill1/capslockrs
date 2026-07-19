use std::ffi::OsStr;
use std::fs;
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{self, Command};

use anyhow::{Context, Result, anyhow, bail};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_CANCELLED, HANDLE, HWND, VARIANT_FALSE, VARIANT_TRUE, WAIT_ABANDONED,
    WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows::Win32::Storage::FileSystem::{
    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    CoUninitialize,
};
use windows::Win32::System::TaskScheduler::{
    IExecAction, ILogonTrigger, ITaskFolder, ITaskService, TASK_ACTION_EXEC, TASK_CREATE_OR_UPDATE,
    TASK_INSTANCES_STOP_EXISTING, TASK_LOGON_INTERACTIVE_TOKEN, TASK_RUNLEVEL_HIGHEST,
    TASK_TRIGGER_LOGON, TaskScheduler,
};
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CreateEventW, CreateMutexW, EVENT_MODIFY_STATE, GetCurrentProcess,
    GetExitCodeProcess, OpenEventW, OpenProcess, OpenProcessToken, PROCESS_ACCESS_RIGHTS,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE, QueryFullProcessImageNameW, ReleaseMutex,
    SetEvent, TerminateProcess, WaitForSingleObject,
};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
use windows::core::{BSTR, Interface, PCWSTR, PWSTR};

use crate::logging;

const TASK_NAME: &str = "CapsLockRs";
const WORKER_MUTEX: &str = "Local\\CapsLockRs.Worker";
const UPDATE_MUTEX: &str = "Local\\CapsLockRs.Update";
const STOP_EVENT: &str = "Local\\CapsLockRs.Stop";

#[derive(Debug)]
pub struct WinHandle(HANDLE);

impl WinHandle {
    pub(crate) fn new(handle: HANDLE) -> Self {
        Self(handle)
    }

    pub fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for WinHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: this wrapper uniquely owns the handle.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

unsafe impl Send for WinHandle {}
unsafe impl Sync for WinHandle {}

pub struct WorkerInstance {
    mutex: WinHandle,
    stop_event: WinHandle,
    pid_path: PathBuf,
}

impl WorkerInstance {
    pub fn stop_event(&self) -> HANDLE {
        self.stop_event.raw()
    }
}

impl Drop for WorkerInstance {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.pid_path);
        // SAFETY: this process acquired the mutex before constructing WorkerInstance.
        unsafe {
            let _ = ReleaseMutex(self.mutex.raw());
        }
    }
}

struct ComApartment;

impl ComApartment {
    fn initialize() -> Result<Self> {
        // SAFETY: balanced by Drop on the same thread.
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
            .ok()
            .context("初始化 COM 失败")?;
        Ok(Self)
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        // SAFETY: paired with a successful CoInitializeEx on this thread.
        unsafe { CoUninitialize() };
    }
}

struct TaskManager {
    root: ITaskFolder,
    service: ITaskService,
    _com: ComApartment,
}

impl TaskManager {
    fn connect() -> Result<Self> {
        let com = ComApartment::initialize()?;
        // SAFETY: COM is initialized and TaskScheduler is an in-process COM class.
        let service: ITaskService = unsafe {
            CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER)
                .context("创建 Task Scheduler COM 服务失败")?
        };
        let empty = VARIANT::default();
        // SAFETY: all VARIANT arguments are valid empty variants for a local connection.
        unsafe { service.Connect(&empty, &empty, &empty, &empty) }
            .context("连接 Task Scheduler 失败")?;
        // SAFETY: the root task folder always exists.
        let root =
            unsafe { service.GetFolder(&BSTR::from("\\")) }.context("打开计划任务根目录失败")?;
        Ok(Self {
            root,
            service,
            _com: com,
        })
    }

    fn account_name(&self) -> Result<BSTR> {
        // SAFETY: the service is connected.
        let user = unsafe { self.service.ConnectedUser() }.context("读取当前任务用户失败")?;
        // SAFETY: the service is connected.
        let domain = unsafe { self.service.ConnectedDomain() }.unwrap_or_default();
        let user = user.to_string();
        let domain = domain.to_string();
        if domain.is_empty() {
            Ok(BSTR::from(user))
        } else {
            Ok(BSTR::from(format!("{domain}\\{user}")))
        }
    }

    fn register(&self, executable: &Path) -> Result<()> {
        let account = self.account_name()?;
        // SAFETY: the service is connected.
        let definition = unsafe { self.service.NewTask(0) }.context("创建计划任务定义失败")?;

        // SAFETY: interfaces belong to the live task definition.
        unsafe {
            definition
                .RegistrationInfo()?
                .SetDescription(&BSTR::from("CapsLockRs 管理员键盘重映射程序"))?;

            let principal = definition.Principal()?;
            principal.SetUserId(&account)?;
            principal.SetLogonType(TASK_LOGON_INTERACTIVE_TOKEN)?;
            principal.SetRunLevel(TASK_RUNLEVEL_HIGHEST)?;

            let trigger: ILogonTrigger =
                definition.Triggers()?.Create(TASK_TRIGGER_LOGON)?.cast()?;
            trigger.SetUserId(&account)?;

            let action: IExecAction = definition.Actions()?.Create(TASK_ACTION_EXEC)?.cast()?;
            action.SetPath(&BSTR::from(
                executable.as_os_str().to_string_lossy().as_ref(),
            ))?;
            action.SetArguments(&BSTR::from("--worker"))?;
            if let Some(parent) = executable.parent() {
                action.SetWorkingDirectory(&BSTR::from(
                    parent.as_os_str().to_string_lossy().as_ref(),
                ))?;
            }

            let settings = definition.Settings()?;
            settings.SetEnabled(VARIANT_TRUE)?;
            settings.SetHidden(VARIANT_TRUE)?;
            settings.SetAllowDemandStart(VARIANT_TRUE)?;
            settings.SetStartWhenAvailable(VARIANT_TRUE)?;
            settings.SetDisallowStartIfOnBatteries(VARIANT_FALSE)?;
            settings.SetStopIfGoingOnBatteries(VARIANT_FALSE)?;
            settings.SetAllowHardTerminate(VARIANT_TRUE)?;
            settings.SetExecutionTimeLimit(&BSTR::from("PT0S"))?;
            settings.SetRestartInterval(&BSTR::from("PT1M"))?;
            settings.SetRestartCount(10)?;
            settings.SetMultipleInstances(TASK_INSTANCES_STOP_EXISTING)?;

            let empty = VARIANT::default();
            self.root.RegisterTaskDefinition(
                &BSTR::from(TASK_NAME),
                &definition,
                TASK_CREATE_OR_UPDATE.0,
                &empty,
                &empty,
                TASK_LOGON_INTERACTIVE_TOKEN,
                &empty,
            )?;
        }
        Ok(())
    }

    fn is_enabled(&self) -> bool {
        // SAFETY: the task object, when present, is valid for this COM apartment.
        unsafe {
            self.root
                .GetTask(&BSTR::from(TASK_NAME))
                .and_then(|task| task.Enabled())
                .is_ok_and(|enabled| enabled.0 != 0)
        }
    }

    fn delete(&self) -> Result<()> {
        if self.is_enabled() || unsafe { self.root.GetTask(&BSTR::from(TASK_NAME)) }.is_ok() {
            // SAFETY: deletion is scoped to the fixed application task name.
            unsafe { self.root.DeleteTask(&BSTR::from(TASK_NAME), 0) }
                .context("删除开机启动计划任务失败")?;
        }
        Ok(())
    }

    fn run(&self) -> Result<()> {
        // SAFETY: Run receives an empty parameter variant, as required by an exec action.
        unsafe {
            self.root
                .GetTask(&BSTR::from(TASK_NAME))?
                .Run(&VARIANT::default())?;
        }
        Ok(())
    }

    fn stop(&self) {
        // SAFETY: stops only the fixed task when it exists.
        unsafe {
            if let Ok(task) = self.root.GetTask(&BSTR::from(TASK_NAME)) {
                let _ = task.Stop(0);
            }
        }
    }
}

pub fn app_dir() -> Result<PathBuf> {
    let base =
        std::env::var_os("LOCALAPPDATA").ok_or_else(|| anyhow!("未找到 LOCALAPPDATA 环境变量"))?;
    Ok(PathBuf::from(base).join("CapsLockRs"))
}

pub fn installed_exe() -> Result<PathBuf> {
    Ok(app_dir()?.join("capslockrs.exe"))
}

pub fn is_elevated() -> Result<bool> {
    let mut token = HANDLE::default();
    // SAFETY: token receives a newly opened process token handle.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }
        .context("打开当前进程令牌失败")?;
    let token = WinHandle::new(token);
    let mut elevation = TOKEN_ELEVATION::default();
    let mut returned = 0;
    // SAFETY: buffer and size exactly match TOKEN_ELEVATION.
    unsafe {
        GetTokenInformation(
            token.raw(),
            TokenElevation,
            Some((&mut elevation as *mut TOKEN_ELEVATION).cast()),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        )
    }
    .context("读取管理员令牌状态失败")?;
    Ok(elevation.TokenIsElevated != 0)
}

pub fn relaunch_elevated() -> Result<i32> {
    let executable = std::env::current_exe().context("无法确定当前程序路径")?;
    let executable_w = wide(&executable);
    let verb = wide(OsStr::new("runas"));
    let parameters = wide(OsStr::new("--elevated-install"));
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        hwnd: HWND::default(),
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(executable_w.as_ptr()),
        lpParameters: PCWSTR(parameters.as_ptr()),
        nShow: SW_HIDE.0,
        ..unsafe { zeroed() }
    };
    // SAFETY: all pointers remain valid for the duration of ShellExecuteExW.
    if let Err(error) = unsafe { ShellExecuteExW(&mut info) } {
        if error.code() == ERROR_CANCELLED.into() {
            bail!("用户取消了管理员权限请求");
        }
        return Err(error).context("请求管理员权限失败");
    }
    if info.hProcess.is_invalid() {
        bail!("管理员进程未能启动");
    }
    let process_handle = WinHandle::new(info.hProcess);
    // SAFETY: hProcess is a valid process handle returned by ShellExecuteExW.
    unsafe { WaitForSingleObject(process_handle.raw(), u32::MAX) };
    let mut exit_code = 1;
    // SAFETY: the process has terminated and the output pointer is valid.
    unsafe { GetExitCodeProcess(process_handle.raw(), &mut exit_code) }
        .context("读取管理员进程退出码失败")?;
    Ok(exit_code as i32)
}

pub fn install_and_start() -> Result<()> {
    if !is_elevated()? {
        bail!("安装更新必须在管理员权限下运行");
    }
    let app_dir = app_dir()?;
    fs::create_dir_all(&app_dir).context("创建安装目录失败")?;
    logging::init(&app_dir);

    let update_mutex_name = wide(OsStr::new(UPDATE_MUTEX));
    // SAFETY: creates or opens a process-local named kernel mutex.
    let update_mutex = WinHandle::new(unsafe {
        CreateMutexW(None, false, PCWSTR(update_mutex_name.as_ptr()))
            .context("创建更新互斥量失败")?
    });
    // SAFETY: waiting on an owned valid mutex handle.
    let wait = unsafe { WaitForSingleObject(update_mutex.raw(), 30_000) };
    if wait != WAIT_OBJECT_0 && wait != WAIT_ABANDONED {
        bail!("另一个 CapsLockRs 更新操作仍在运行");
    }

    let installed = installed_exe()?;
    let was_installed = installed.exists();
    let tasks = TaskManager::connect()?;
    let keep_autostart = !was_installed || tasks.is_enabled();
    stop_old_worker(&installed)?;

    let current = std::env::current_exe().context("无法确定当前程序路径")?;
    if !same_path(&current, &installed) {
        let temporary = app_dir.join("capslockrs.new.exe");
        fs::copy(&current, &temporary).context("复制新的运行副本失败")?;
        let temporary_w = wide(&temporary);
        let installed_w = wide(&installed);
        // SAFETY: both paths are valid, null-terminated UTF-16 buffers.
        unsafe {
            MoveFileExW(
                PCWSTR(temporary_w.as_ptr()),
                PCWSTR(installed_w.as_ptr()),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        }
        .context("原子更新运行副本失败")?;
    }

    if keep_autostart {
        tasks.register(&installed)?;
        tasks.run().context("启动管理员计划任务失败")?;
    } else {
        Command::new(&installed)
            .arg("--worker")
            .creation_flags(CREATE_NO_WINDOW.0)
            .spawn()
            .context("启动管理员 worker 失败")?;
    }
    logging::log("安装更新完成，已启动新的 worker");

    // SAFETY: this process acquired the mutex above.
    unsafe { ReleaseMutex(update_mutex.raw()) }.ok();
    Ok(())
}

pub fn acquire_worker_instance() -> Result<Option<WorkerInstance>> {
    let mutex_name = wide(OsStr::new(WORKER_MUTEX));
    // SAFETY: creates or opens the fixed worker mutex.
    let mutex = WinHandle::new(unsafe {
        CreateMutexW(None, false, PCWSTR(mutex_name.as_ptr())).context("创建 worker 互斥量失败")?
    });
    // SAFETY: non-blocking wait on a valid mutex.
    let wait = unsafe { WaitForSingleObject(mutex.raw(), 0) };
    if wait != WAIT_OBJECT_0 && wait != WAIT_ABANDONED {
        return Ok(None);
    }

    let event_name = wide(OsStr::new(STOP_EVENT));
    // SAFETY: creates a manual-reset, initially nonsignaled stop event.
    let stop_event = WinHandle::new(unsafe {
        CreateEventW(None, true, false, PCWSTR(event_name.as_ptr())).context("创建停止事件失败")?
    });
    let pid_path = app_dir()?.join("worker.pid");
    fs::write(&pid_path, process::id().to_string()).context("写入 worker 状态失败")?;
    Ok(Some(WorkerInstance {
        mutex,
        stop_event,
        pid_path,
    }))
}

pub fn autostart_enabled() -> bool {
    TaskManager::connect().is_ok_and(|tasks| tasks.is_enabled())
}

pub fn set_autostart(enabled: bool) -> Result<()> {
    if !is_elevated()? {
        bail!("修改开机启动需要管理员权限");
    }
    let tasks = TaskManager::connect()?;
    if enabled {
        tasks.register(&installed_exe()?)?;
        logging::log("已启用登录自启动");
    } else {
        tasks.delete()?;
        logging::log("已禁用登录自启动");
    }
    Ok(())
}

fn stop_old_worker(installed: &Path) -> Result<()> {
    let pid_path = app_dir()?.join("worker.pid");
    let process_handle = fs::read_to_string(&pid_path)
        .ok()
        .and_then(|text| text.trim().parse::<u32>().ok())
        .and_then(|pid| {
            // SAFETY: access is limited to querying, waiting, and termination.
            unsafe {
                OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION
                        | PROCESS_TERMINATE
                        | PROCESS_ACCESS_RIGHTS(0x0010_0000),
                    false,
                    pid,
                )
                .ok()
            }
        })
        .map(WinHandle::new)
        .filter(|handle| process_path(handle.raw()).is_ok_and(|path| same_path(&path, installed)));

    let event_name = wide(OsStr::new(STOP_EVENT));
    // SAFETY: opens only the fixed application stop event when it exists.
    if let Ok(event) = unsafe { OpenEventW(EVENT_MODIFY_STATE, false, PCWSTR(event_name.as_ptr())) }
    {
        let event = WinHandle::new(event);
        // SAFETY: event is a valid event handle.
        let _ = unsafe { SetEvent(event.raw()) };
    }

    if let Some(handle) = process_handle {
        // SAFETY: process handle includes SYNCHRONIZE.
        let wait = unsafe { WaitForSingleObject(handle.raw(), 3000) };
        if wait == WAIT_TIMEOUT {
            logging::log("旧 worker 未及时退出，正在强制终止");
            // SAFETY: executable path was validated and handle has PROCESS_TERMINATE.
            unsafe { TerminateProcess(handle.raw(), 0) }.context("终止旧 worker 失败")?;
            // SAFETY: process handle remains valid.
            unsafe { WaitForSingleObject(handle.raw(), 3000) };
        }
    }

    let mutex_name = wide(OsStr::new(WORKER_MUTEX));
    // SAFETY: creates or opens only the fixed worker mutex.
    let worker_mutex = WinHandle::new(unsafe {
        CreateMutexW(None, false, PCWSTR(mutex_name.as_ptr())).context("打开 worker 互斥量失败")?
    });
    // SAFETY: waits for graceful teardown to release the worker mutex.
    let mut mutex_wait = unsafe { WaitForSingleObject(worker_mutex.raw(), 3000) };
    if mutex_wait != WAIT_OBJECT_0 && mutex_wait != WAIT_ABANDONED {
        if let Ok(tasks) = TaskManager::connect() {
            tasks.stop();
        }
        // SAFETY: the task has been asked to stop; wait once more for teardown.
        mutex_wait = unsafe { WaitForSingleObject(worker_mutex.raw(), 3000) };
    }
    if mutex_wait == WAIT_OBJECT_0 || mutex_wait == WAIT_ABANDONED {
        // SAFETY: this updater acquired the worker mutex and immediately releases it.
        unsafe { ReleaseMutex(worker_mutex.raw()) }.ok();
    } else {
        bail!("旧 worker 未能释放单实例互斥量");
    }
    let _ = fs::remove_file(pid_path);
    Ok(())
}

fn process_path(process: HANDLE) -> Result<PathBuf> {
    let mut buffer = vec![0_u16; 32768];
    let mut length = buffer.len() as u32;
    // SAFETY: buffer and its current capacity are correctly supplied.
    unsafe {
        QueryFullProcessImageNameW(
            process,
            Default::default(),
            PWSTR(buffer.as_mut_ptr()),
            &mut length,
        )
    }
    .context("查询旧 worker 路径失败")?;
    buffer.truncate(length as usize);
    Ok(PathBuf::from(String::from_utf16_lossy(&buffer)))
}

fn same_path(left: &Path, right: &Path) -> bool {
    left.as_os_str()
        .to_string_lossy()
        .eq_ignore_ascii_case(&right.as_os_str().to_string_lossy())
}

fn wide(value: impl AsRef<OsStr>) -> Vec<u16> {
    value.as_ref().encode_wide().chain(Some(0)).collect()
}
