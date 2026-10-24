//! 内置终端（Friday 的 term.*，Friday 仓库的 qt/docs/TERMINAL.md）：大脑开伪终端跑 shell，客户端断了照常跑，连上来的接着看
//! （最近 512 KB 的原始输出重放）。macOS / Linux 用 posix_openpt，Windows 用 ConPTY。
//! 每个终端记着谁接着（连接名）：输出推 `terminal.output` 给它们；开了、关了、退出了推 `terminals`（全部终端）给全部全权的连接。

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use serde_json::{Value, json};

/// 重放最多留多少输出
const KEEP: usize = 512 * 1024;

/// 伪终端的那一头：写进去、改大小、结束
trait Pty: Send + Sync {
    fn write(&self, data: &[u8]) -> std::io::Result<()>;
    fn resize(&self, cols: u16, rows: u16);
    fn kill(&self);
}

pub struct Terminal {
    id: String,
    session: String,
    title: String,
    command: Option<String>,
    size: Mutex<(u16, u16)>,
    output: Mutex<VecDeque<u8>>,
    running: AtomicBool,
    exit_code: Mutex<Option<i32>>,
    /// 接着它的连接（能力名）
    watchers: Mutex<HashSet<String>>,
    pty: Box<dyn Pty>,
}

static TERMINALS: LazyLock<Mutex<HashMap<String, Arc<Terminal>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn base64(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

impl Terminal {
    fn info(&self) -> Value {
        let (cols, rows) = *self.size.lock().unwrap();
        json!({
            "id": self.id, "conversationID": self.session, "title": self.title, "shell": self.title, "command": self.command,
            "running": self.running.load(Ordering::Relaxed), "exitCode": *self.exit_code.lock().unwrap(), "cols": cols, "rows": rows,
        })
    }

    /// 读到输出：记下（重放用）、推给接着的
    fn output(&self, data: &[u8]) {
        {
            let mut kept = self.output.lock().unwrap();
            kept.extend(data);
            let extra = kept.len().saturating_sub(KEEP);
            kept.drain(..extra);
        }
        let watchers: Vec<String> = self.watchers.lock().unwrap().iter().cloned().collect();
        if !watchers.is_empty() {
            crate::capabilities::notify_some(&watchers, "terminal.output", json!({ "id": self.id, "data": base64(data) }));
        }
    }

    fn exited(&self, code: Option<i32>) {
        self.running.store(false, Ordering::Relaxed);
        *self.exit_code.lock().unwrap() = code;
        changed();
    }
}

/// 全部终端（TerminalInfo）
pub fn list() -> Vec<Value> {
    let mut all: Vec<Arc<Terminal>> = TERMINALS.lock().unwrap().values().cloned().collect();
    all.sort_by(|a, b| a.id.cmp(&b.id));
    all.iter().map(|terminal| terminal.info()).collect()
}

fn changed() {
    crate::capabilities::notify_full("terminals", json!({ "terminals": list() }));
}

fn find(id: &str) -> Result<Arc<Terminal>, String> {
    TERMINALS.lock().unwrap().get(id).cloned().ok_or_else(|| format!("没有终端 {id}"))
}

/// 用的 shell：Unix 的 $SHELL（没有用 /bin/sh），Windows 的 PowerShell
fn shell() -> (String, Vec<String>) {
    if cfg!(windows) {
        return ("powershell.exe".into(), vec!["-NoLogo".into()]);
    }
    let shell = std::env::var("SHELL").ok().filter(|shell| Path::new(shell).is_file()).unwrap_or_else(|| "/bin/sh".into());
    (shell, vec!["-l".into()])
}

/// 开一个（id 是客户端给的；watcher：开它的连接，开好就接着它）。command 起好后打进去回车
pub fn open(id: &str, session: &str, cwd: &Path, cols: u16, rows: u16, command: Option<&str>, watcher: &str) -> Result<Value, String> {
    if id.is_empty() || TERMINALS.lock().unwrap().contains_key(id) {
        return Err(format!("终端 id 不对或已经有了：{id}"));
    }
    let (cols, rows) = (cols.max(2), rows.max(1));
    let (program, args) = shell();
    let title = Path::new(&program).file_stem().map(|name| name.to_string_lossy().into_owned()).unwrap_or_else(|| program.clone());
    let cwd = if cwd.is_dir() { cwd.to_path_buf() } else { crate::home::user_home() };
    let (pty, reader, waiter) = spawn(&program, &args, &cwd, cols, rows)?;
    let terminal = Arc::new(Terminal {
        id: id.to_string(),
        session: session.to_string(),
        title,
        command: command.map(str::to_string),
        size: Mutex::new((cols, rows)),
        output: Mutex::new(VecDeque::new()),
        running: AtomicBool::new(true),
        exit_code: Mutex::new(None),
        watchers: Mutex::new(HashSet::from([watcher.to_string()])),
        pty,
    });
    TERMINALS.lock().unwrap().insert(id.to_string(), terminal.clone());
    {
        let terminal = terminal.clone();
        let mut reader = reader;
        std::thread::spawn(move || {
            let mut buffer = [0u8; 16 * 1024];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => terminal.output(&buffer[..count]),
                }
            }
        });
    }
    {
        let terminal = terminal.clone();
        std::thread::spawn(move || terminal.exited(waiter()));
    }
    if let Some(command) = command.filter(|command| !command.trim().is_empty()) {
        let line = if cfg!(windows) { format!("{command}\r") } else { format!("{command}\n") };
        let _ = terminal.pty.write(line.as_bytes());
    }
    changed();
    Ok(terminal.info())
}

/// 接上：之后的输出推给它；回最近的输出和现在的大小
pub fn attach(id: &str, watcher: &str) -> Result<Value, String> {
    let terminal = find(id)?;
    terminal.watchers.lock().unwrap().insert(watcher.to_string());
    let data: Vec<u8> = terminal.output.lock().unwrap().iter().copied().collect();
    let (cols, rows) = *terminal.size.lock().unwrap();
    Ok(json!({ "data": base64(&data), "cols": cols, "rows": rows }))
}

pub fn detach(id: &str, watcher: &str) -> Result<Value, String> {
    find(id)?.watchers.lock().unwrap().remove(watcher);
    Ok(json!({}))
}

/// 连接断了：不再推给它
pub fn forget(watcher: &str) {
    for terminal in TERMINALS.lock().unwrap().values() {
        terminal.watchers.lock().unwrap().remove(watcher);
    }
}

pub fn write(id: &str, data: &[u8]) -> Result<Value, String> {
    find(id)?.pty.write(data).map_err(|error| format!("写不进终端：{error}"))?;
    Ok(json!({}))
}

pub fn resize(id: &str, cols: u16, rows: u16) -> Result<Value, String> {
    let terminal = find(id)?;
    let (cols, rows) = (cols.max(2), rows.max(1));
    *terminal.size.lock().unwrap() = (cols, rows);
    terminal.pty.resize(cols, rows);
    changed();
    Ok(json!({}))
}

/// 结束 shell 和它起的进程，从列表去掉
pub fn close(id: &str) -> Result<Value, String> {
    let terminal = TERMINALS.lock().unwrap().remove(id).ok_or_else(|| format!("没有终端 {id}"))?;
    terminal.pty.kill();
    changed();
    Ok(json!({}))
}

/// 大脑退出时：都结束
pub fn close_all() {
    let all: Vec<Arc<Terminal>> = TERMINALS.lock().unwrap().drain().map(|(_, terminal)| terminal).collect();
    for terminal in all {
        terminal.pty.kill();
    }
}

type Spawned = (Box<dyn Pty>, Box<dyn Read + Send>, Box<dyn FnOnce() -> Option<i32> + Send>);

#[cfg(unix)]
fn spawn(program: &str, args: &[String], cwd: &Path, cols: u16, rows: u16) -> Result<Spawned, String> {
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;

    static PTSNAME: Mutex<()> = Mutex::new(());
    let failed = |what: &str| format!("开不了伪终端（{what}）：{}", std::io::Error::last_os_error());
    let master = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    if master < 0 {
        return Err(failed("posix_openpt"));
    }
    let master = unsafe { OwnedFd::from_raw_fd(master) };
    use std::os::fd::AsRawFd;
    let fd = master.as_raw_fd();
    if unsafe { libc::grantpt(fd) } != 0 || unsafe { libc::unlockpt(fd) } != 0 {
        return Err(failed("grantpt"));
    }
    let name = {
        let _guard = PTSNAME.lock().unwrap();
        let name = unsafe { libc::ptsname(fd) };
        if name.is_null() {
            return Err(failed("ptsname"));
        }
        unsafe { std::ffi::CStr::from_ptr(name) }.to_owned()
    };
    let slave = unsafe { libc::open(name.as_ptr(), libc::O_RDWR | libc::O_NOCTTY) };
    if slave < 0 {
        return Err(failed("open"));
    }
    let slave = unsafe { OwnedFd::from_raw_fd(slave) };
    set_size(fd, cols, rows);
    let mut command = std::process::Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .env("TERM", "xterm-256color")
        .env("COLORTERM", "truecolor")
        .stdin(slave.try_clone().map_err(|error| error.to_string())?)
        .stdout(slave.try_clone().map_err(|error| error.to_string())?)
        .stderr(slave.try_clone().map_err(|error| error.to_string())?);
    unsafe {
        command.pre_exec(|| {
            // 自己一个会话、这个伪终端当控制终端（Ctrl+C、作业控制才对）
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().map_err(|error| format!("起不了 {program}：{error}"))?;
    drop(slave);
    let pid = child.id() as libc::pid_t;
    let reader = std::fs::File::from(master.try_clone().map_err(|error| error.to_string())?);
    struct Unix {
        master: Mutex<std::fs::File>,
        pid: libc::pid_t,
    }
    impl Pty for Unix {
        fn write(&self, data: &[u8]) -> std::io::Result<()> {
            let mut master = self.master.lock().unwrap();
            master.write_all(data).and_then(|_| master.flush())
        }
        fn resize(&self, cols: u16, rows: u16) {
            use std::os::fd::AsRawFd;
            set_size(self.master.lock().unwrap().as_raw_fd(), cols, rows);
        }
        fn kill(&self) {
            // 整个进程组：shell 和它起的
            unsafe {
                libc::kill(-self.pid, libc::SIGHUP);
                libc::kill(-self.pid, libc::SIGTERM);
            }
            let pid = self.pid;
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(2));
                unsafe { libc::kill(-pid, libc::SIGKILL) };
            });
        }
    }
    let pty = Unix { master: Mutex::new(std::fs::File::from(master)), pid };
    let waiter = Box::new(move || child.wait().ok().and_then(|status| status.code()));
    Ok((Box::new(pty), Box::new(reader), waiter))
}

#[cfg(unix)]
fn set_size(fd: std::os::fd::RawFd, cols: u16, rows: u16) {
    let size = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
    unsafe { libc::ioctl(fd, libc::TIOCSWINSZ as _, &size) };
}

#[cfg(windows)]
#[allow(non_snake_case, clippy::upper_case_acronyms)]
mod conpty {
    use std::ffi::c_void;

    pub type HANDLE = *mut c_void;
    pub type HPCON = *mut c_void;

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct COORD {
        pub X: i16,
        pub Y: i16,
    }

    #[repr(C)]
    pub struct STARTUPINFOW {
        pub cb: u32,
        pub lpReserved: *mut u16,
        pub lpDesktop: *mut u16,
        pub lpTitle: *mut u16,
        pub dwX: u32,
        pub dwY: u32,
        pub dwXSize: u32,
        pub dwYSize: u32,
        pub dwXCountChars: u32,
        pub dwYCountChars: u32,
        pub dwFillAttribute: u32,
        pub dwFlags: u32,
        pub wShowWindow: u16,
        pub cbReserved2: u16,
        pub lpReserved2: *mut u8,
        pub hStdInput: HANDLE,
        pub hStdOutput: HANDLE,
        pub hStdError: HANDLE,
    }

    #[repr(C)]
    pub struct STARTUPINFOEXW {
        pub StartupInfo: STARTUPINFOW,
        pub lpAttributeList: *mut c_void,
    }

    #[repr(C)]
    pub struct PROCESS_INFORMATION {
        pub hProcess: HANDLE,
        pub hThread: HANDLE,
        pub dwProcessId: u32,
        pub dwThreadId: u32,
    }

    pub const PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE: usize = 0x0002_0016;
    pub const EXTENDED_STARTUPINFO_PRESENT: u32 = 0x0008_0000;
    pub const CREATE_UNICODE_ENVIRONMENT: u32 = 0x0000_0400;
    pub const STARTF_USESTDHANDLES: u32 = 0x0000_0100;
    pub const INFINITE: u32 = 0xFFFF_FFFF;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        pub fn CreatePipe(read: *mut HANDLE, write: *mut HANDLE, attributes: *const c_void, size: u32) -> i32;
        pub fn CreatePseudoConsole(size: COORD, input: HANDLE, output: HANDLE, flags: u32, console: *mut HPCON) -> i32;
        pub fn ResizePseudoConsole(console: HPCON, size: COORD) -> i32;
        pub fn ClosePseudoConsole(console: HPCON);
        pub fn InitializeProcThreadAttributeList(list: *mut c_void, count: u32, flags: u32, size: *mut usize) -> i32;
        pub fn UpdateProcThreadAttribute(
            list: *mut c_void,
            flags: u32,
            attribute: usize,
            value: *const c_void,
            size: usize,
            previous: *mut c_void,
            returned: *mut usize,
        ) -> i32;
        pub fn DeleteProcThreadAttributeList(list: *mut c_void);
        pub fn CreateProcessW(
            application: *const u16,
            command_line: *mut u16,
            process_attributes: *const c_void,
            thread_attributes: *const c_void,
            inherit_handles: i32,
            flags: u32,
            environment: *const c_void,
            directory: *const u16,
            startup: *mut STARTUPINFOEXW,
            information: *mut PROCESS_INFORMATION,
        ) -> i32;
        pub fn CloseHandle(handle: HANDLE) -> i32;
        pub fn WaitForSingleObject(handle: HANDLE, milliseconds: u32) -> u32;
        pub fn GetExitCodeProcess(handle: HANDLE, code: *mut u32) -> i32;
        pub fn TerminateProcess(handle: HANDLE, code: u32) -> i32;
    }
}

#[cfg(windows)]
fn spawn(program: &str, args: &[String], cwd: &Path, cols: u16, rows: u16) -> Result<Spawned, String> {
    use conpty::*;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use std::ptr::{null, null_mut};

    let failed = |what: &str| format!("开不了伪终端（{what}）：{}", std::io::Error::last_os_error());
    let wide = |text: &std::ffi::OsStr| text.encode_wide().chain(std::iter::once(0)).collect::<Vec<u16>>();
    let (mut input_read, mut input_write, mut output_read, mut output_write): (HANDLE, HANDLE, HANDLE, HANDLE) =
        (null_mut(), null_mut(), null_mut(), null_mut());
    unsafe {
        if CreatePipe(&mut input_read, &mut input_write, null(), 0) == 0 || CreatePipe(&mut output_read, &mut output_write, null(), 0) == 0
        {
            return Err(failed("CreatePipe"));
        }
        let mut console: HPCON = null_mut();
        let size = COORD { X: cols as i16, Y: rows as i16 };
        let result = CreatePseudoConsole(size, input_read, output_write, 0, &mut console);
        if result < 0 {
            return Err(format!("开不了伪终端（CreatePseudoConsole 0x{result:08x}，要 Windows 10 1809 以上）"));
        }
        // 伪终端那头的句柄交给它了，这边关掉（不然读不到结束）
        CloseHandle(input_read);
        CloseHandle(output_write);
        let mut list_size = 0usize;
        InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut list_size);
        let mut list = vec![0u8; list_size];
        let list_pointer = list.as_mut_ptr() as *mut std::ffi::c_void;
        if InitializeProcThreadAttributeList(list_pointer, 1, 0, &mut list_size) == 0
            || UpdateProcThreadAttribute(
                list_pointer,
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
                console as *const std::ffi::c_void,
                std::mem::size_of::<HPCON>(),
                null_mut(),
                null_mut(),
            ) == 0
        {
            ClosePseudoConsole(console);
            return Err(failed("UpdateProcThreadAttribute"));
        }
        let mut startup: STARTUPINFOEXW = std::mem::zeroed();
        startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        // 标准输入输出不继承这个进程的（大脑没有控制台），全走伪终端
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.lpAttributeList = list_pointer;
        let mut line = std::ffi::OsString::from(format!("\"{program}\""));
        for arg in args {
            line.push(" ");
            line.push(arg);
        }
        let mut line = wide(&line);
        let directory = wide(cwd.as_os_str());
        let mut information: PROCESS_INFORMATION = std::mem::zeroed();
        let created = CreateProcessW(
            null(),
            line.as_mut_ptr(),
            null(),
            null(),
            0,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
            null(),
            directory.as_ptr(),
            &mut startup,
            &mut information,
        );
        DeleteProcThreadAttributeList(list_pointer);
        if created == 0 {
            ClosePseudoConsole(console);
            return Err(failed("CreateProcessW"));
        }
        CloseHandle(information.hThread);
        struct Windows {
            input: Mutex<std::fs::File>,
            console: usize,
            process: usize,
        }
        // 句柄只在这个结构里用，几个线程经锁、按值传
        unsafe impl Send for Windows {}
        unsafe impl Sync for Windows {}
        impl Pty for Windows {
            fn write(&self, data: &[u8]) -> std::io::Result<()> {
                let mut input = self.input.lock().unwrap();
                input.write_all(data).and_then(|_| input.flush())
            }
            fn resize(&self, cols: u16, rows: u16) {
                unsafe { ResizePseudoConsole(self.console as HPCON, COORD { X: cols as i16, Y: rows as i16 }) };
            }
            fn kill(&self) {
                unsafe {
                    TerminateProcess(self.process as HANDLE, 1);
                    ClosePseudoConsole(self.console as HPCON);
                }
            }
        }
        let process = information.hProcess as usize;
        let pty = Windows { input: Mutex::new(std::fs::File::from_raw_handle(input_write)), console: console as usize, process };
        let reader = std::fs::File::from_raw_handle(output_read);
        let waiter = Box::new(move || {
            let handle = process as HANDLE;
            WaitForSingleObject(handle, INFINITE);
            let mut code = 0u32;
            let known = GetExitCodeProcess(handle, &mut code) != 0;
            CloseHandle(handle);
            known.then_some(code as i32)
        });
        Ok((Box::new(pty), Box::new(reader), waiter))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn runs_a_shell_and_replays() {
        let id = format!("test-{}", std::process::id());
        let dir = std::env::temp_dir();
        open(&id, "s1", &dir, 80, 24, Some("echo edith-$((40+2))"), "nobody").unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let replay = loop {
            let replay = attach(&id, "nobody").unwrap();
            let data = {
                use base64::Engine;
                base64::engine::general_purpose::STANDARD.decode(replay["data"].as_str().unwrap()).unwrap()
            };
            if String::from_utf8_lossy(&data).contains("edith-42") || std::time::Instant::now() > deadline {
                break String::from_utf8_lossy(&data).into_owned();
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        };
        assert!(replay.contains("edith-42"), "{replay}");
        resize(&id, 100, 30).unwrap();
        assert_eq!(list().iter().find(|item| item["id"] == json!(id)).unwrap()["cols"], 100);
        close(&id).unwrap();
        assert!(list().iter().all(|item| item["id"] != json!(id)));
    }
}
