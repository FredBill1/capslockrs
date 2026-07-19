# CapsLockRs

CapsLockRs 是一个仅面向 Windows 10/11 x64 的原生 Rust 键盘重映射程序，用来替代仓库中的 `reference.ahk`。所有按键映射和时间参数都编译在程序中，不读取配置文件。

## 构建和运行

```powershell
cargo run --release
```

首次运行会请求 UAC 权限，然后执行以下操作：

1. 安装运行副本到 `%LOCALAPPDATA%\CapsLockRs\capslockrs.exe`。
2. 注册名为 `CapsLockRs` 的当前用户登录计划任务，并以最高权限运行。
3. 停止旧实例并立即启动新实例。

常驻进程不使用 `target` 目录中的构建产物，因此可以在旧版本运行时继续修改代码并再次执行 `cargo run --release`。更新程序会先请求旧实例正常退出，超时后才会校验 PID 和完整路径并强制结束它。

运行前请退出 `reference.ahk`，避免两个程序同时处理相同按键。程序不会终止其他 AutoHotkey 脚本。

## 托盘菜单

右键单击系统托盘中的 CapsLockRs 图标：

- **暂停**：暂停全部重映射和 F13–F15 连点；暂停期间 CapsLock 恢复系统原生行为。
- **开机启动**：启用或取消当前用户登录时的管理员计划任务。取消后，后续代码更新不会擅自重新启用它。
- **退出**：正常卸载钩子并结束当前实例。若仍勾选“开机启动”，下次登录时会再次运行。

## 固定功能

- CapsLock 在 300ms 内单击发送 `Esc`；长按作为功能层。
- CapsLock 组合键完整复现 `reference.ahk` 中的导航、选择、删除和三个自定义快捷键。
- 按住 F13、F14、F15 时，每 10ms 分别连点鼠标左键、右键、中键。
- 键盘钩子在专用高优先级线程运行，注入、连点和日志不会阻塞钩子回调；看门狗通过无实际按键效果的健康探针检测钩子，只在探针超时时重装，不会周期性清空正在按住的 CapsLock 状态。

日志位于 `%LOCALAPPDATA%\CapsLockRs\logs`，达到 1 MiB 后轮转一次。

## 验证

```powershell
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
```

若要彻底移除，先在托盘中取消“开机启动”并退出，然后删除 `%LOCALAPPDATA%\CapsLockRs`。
