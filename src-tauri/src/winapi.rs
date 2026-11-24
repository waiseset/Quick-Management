//! Windows 系统调用：ShellExecuteW 打开条目、explorer /select 定位文件、
//! 通过 IShellItemImageFactory 提取 exe/快捷方式/文件夹的系统图标。

use std::ffi::c_void;
use std::path::Path;
use std::process::Command;

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;

use windows::core::{BOOL, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, SetLastError, ERROR_ALREADY_EXISTS, ERROR_SUCCESS, HANDLE, HWND,
    LPARAM, RECT, SIZE,
};
use windows::Win32::Graphics::Gdi::{
    DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, DIB_RGB_COLORS, HBITMAP, HGDIOBJ,
};
use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
    TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Environment::ExpandEnvironmentStringsW;
use windows::Win32::System::Threading::{
    AttachThreadInput, CreateMutexW, GetCurrentProcess, GetCurrentThreadId, OpenProcess,
    OpenProcessToken, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Shell::{
    IShellItemImageFactory, SHCreateItemFromParsingName, ShellExecuteW, SIIGBF_BIGGERSIZEOK,
    SIIGBF_ICONONLY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, EnumWindows, GetForegroundWindow, GetWindow, GetWindowLongW, GetWindowRect,
    GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindowVisible,
    SetForegroundWindow, ShowWindow, SwitchToThisWindow, GWL_EXSTYLE, GW_OWNER, SW_RESTORE,
    SW_SHOW, SW_SHOWNORMAL, WS_EX_TOOLWINDOW,
};

use crate::model::IconData;
use crate::shortcut;

/// 图标最大边长（导出后体积可控）
const ICON_MAX: u32 = 256;

/// 打开目标：应用走 ShellExecuteW("open")，网址同样交给 ShellExecuteW 用默认浏览器打开。
///
/// 有三件事少一件就会出现「打不开 / 打开后不能用」：
/// 1. **展开 `%环境变量%`** —— 快捷方式里常常存着 `%ProgramFiles%\…`，
///    直接丢给 ShellExecuteW 它不会帮你展开（老数据里也可能留着这种路径）；
/// 2. **把起始位置设成目标所在目录** —— 不设的话子进程继承本应用的目录，
///    依赖相对路径找配置 / DLL / 素材的程序会启动失败或行为异常；
/// 3. 打开**快捷方式本身**时不指定目录，让系统按快捷方式里记录的「起始位置」启动。
pub fn shell_open(target: &str) -> Result<(), String> {
    // 用户可能把带引号的路径整段粘进来（"D:\Program Files\App\app.exe"）
    let path = expand_env_vars(target.trim().trim_matches('"'));
    if path.is_empty() {
        return Err("路径或网址为空".into());
    }

    let verb = HSTRING::from("open");
    let file = HSTRING::from(path.as_str());
    // 目录要在调用期间一直存活
    let directory = launch_directory(&path).map(|dir| HSTRING::from(dir.to_string_lossy().as_ref()));

    let ret = unsafe {
        // ShellExecuteW 内部要用到 shell 扩展，先确保这个线程初始化过 COM
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        ShellExecuteW(
            None,
            &verb,
            &file,
            PCWSTR::null(),
            directory
                .as_ref()
                .map_or(PCWSTR::null(), |dir| PCWSTR(dir.as_ptr())),
            SW_SHOWNORMAL,
        )
    };
    let code = ret.0 as isize;
    // ShellExecuteW 返回值 <= 32 表示失败
    if code <= 32 {
        return Err(format!(
            "系统无法打开「{path}」（{}）",
            shell_error_text(code)
        ));
    }
    Ok(())
}

/// 进程级单实例锁：返回 true 表示当前进程是唯一实例，可以继续创建窗口；
/// 返回 false 表示已有实例在运行，应把已有窗口切到前台后直接退出。
///
/// 用命名互斥体而不是窗口标题：窗口标题会随页面变化，且第二实例启动时
/// 第一实例可能还没创建窗口。互斥体句柄故意泄漏到进程结束，提前释放会让
/// 单实例保护失效。
pub fn claim_single_instance() -> bool {
    let name = HSTRING::from("Local\\com.waiseset.quickmanage");
    unsafe {
        SetLastError(ERROR_SUCCESS);
        match CreateMutexW(None, false, PCWSTR(name.as_ptr())) {
            Ok(handle) => {
                if GetLastError() == ERROR_ALREADY_EXISTS {
                    let _ = CloseHandle(handle);
                    false
                } else {
                    // HANDLE 没有 Drop，不调用 CloseHandle 就会一直持有到进程退出
                    let _ = handle;
                    true
                }
            }
            // 互斥体创建失败时不拦住用户：宁可多开一个窗口，也不要打不开应用
            Err(_) => true,
        }
    }
}

/// 把已经在运行的同路径实例窗口切到前台并返回 true；没有可用窗口则返回 false。
///
/// 现在只用于快捷管理自身的单实例：第二个进程启动时调用它把第一个窗口置顶。
/// 仍然保留 .lnk 解析与宿主进程过滤，避免以后复用时误激活 explorer / cmd 等窗口。
pub fn activate_running_app(target: &str) -> bool {
    let Some(exe) = app_exe_path(target) else {
        return false;
    };
    if is_host_process(&exe) {
        return false;
    }
    for pid in find_process_ids(&exe) {
        if let Some(hwnd) = best_window(pid) {
            if focus_window(hwnd) {
                return true;
            }
        }
    }
    false
}

/// 把条目目标还原成可以按进程匹配的 exe 全路径
fn app_exe_path(target: &str) -> Option<String> {
    let target = expand_env_vars(target.trim().trim_matches('"'));
    let path = if target.to_ascii_lowercase().ends_with(".lnk") {
        shortcut::read_lnk(&target)?.target
    } else {
        target
    };
    let path = path.trim().trim_matches('"').to_string();
    // 不要求 Path::is_file()：WindowsApps 等受保护目录普通权限读不到文件，
    // 但进程仍在运行，应该以进程枚举结果为准。
    if !path.to_ascii_lowercase().ends_with(".exe") {
        return None;
    }
    Some(path)
}

/// 这些进程只是「载体」，同名的多个应用会共用它们，不能按 exe 当成应用本身
fn is_host_process(path: &str) -> bool {
    let name = Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    matches!(
        name.as_str(),
        "explorer.exe"
            | "cmd.exe"
            | "powershell.exe"
            | "pwsh.exe"
            | "wscript.exe"
            | "cscript.exe"
            | "mshta.exe"
            | "rundll32.exe"
            | "regsvr32.exe"
            | "steam.exe"
            | "steamwebhelper.exe"
            | "epicgameslauncher.exe"
            | "javaw.exe"
            | "java.exe"
            | "applicationframehost.exe"
            | "runtimebroker.exe"
            | "dllhost.exe"
            | "svchost.exe"
    )
}

/// 枚举所有进程，返回 exe 全路径与目标一致的进程 id
fn find_process_ids(exe: &str) -> Vec<u32> {
    let wanted = Path::new(exe)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    if wanted.is_empty() {
        return Vec::new();
    }

    let mut pids = Vec::new();
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return pids;
        };
        let mut entry = PROCESSENTRY32W::default();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;

        let mut next = Process32FirstW(snapshot, &mut entry).is_ok();
        while next {
            // 先比 exe 文件名（几乎零成本），同名了再查全路径
            let name = wide_array_to_string(&entry.szExeFile);
            if name.eq_ignore_ascii_case(wanted)
                && process_image_path(entry.th32ProcessID)
                    .is_some_and(|path| same_path(&path, exe))
            {
                pids.push(entry.th32ProcessID);
            }
            next = Process32NextW(snapshot, &mut entry).is_ok();
        }
        let _ = CloseHandle(snapshot);
    }
    pids
}

/// 取进程的 exe 全路径（受保护的系统进程可能拿不到，忽略即可）
fn process_image_path(pid: u32) -> Option<String> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buffer = [0u16; 32768];
        let mut len = buffer.len() as u32;
        let ok = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut len,
        )
        .is_ok();
        let _ = CloseHandle(handle);
        if !ok || len == 0 {
            return None;
        }
        Some(String::from_utf16_lossy(&buffer[..len as usize]))
    }
}

/// 路径比较：统一分隔符、去掉 `\\\\?\\` 前缀、忽略大小写
fn same_path(a: &str, b: &str) -> bool {
    normalize_path(a) == normalize_path(b)
}

fn normalize_path(path: &str) -> String {
    let mut path = path.trim().trim_matches('"').replace('/', "\\");
    if let Some(rest) = path.strip_prefix("\\\\?\\UNC\\") {
        path = format!("\\\\{rest}");
    } else if let Some(rest) = path.strip_prefix("\\\\?\\") {
        path = rest.to_string();
    }
    path.to_lowercase()
}

fn wide_array_to_string(buffer: &[u16]) -> String {
    let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..end]).trim().to_string()
}

struct WindowSearch {
    pid: u32,
    best: Option<HWND>,
    best_score: i64,
}

/// 在该进程的顶层窗口里挑一个最像「主窗口」的：
/// 可见、无 owner、不是工具窗、面积大于 0，优先有标题的，再取面积最大的
fn best_window(pid: u32) -> Option<HWND> {
    let mut search = WindowSearch {
        pid,
        best: None,
        best_score: i64::MIN,
    };
    unsafe {
        let _ = EnumWindows(
            Some(enum_window),
            LPARAM(&mut search as *mut WindowSearch as isize),
        );
    }
    search.best
}

unsafe extern "system" fn enum_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let search = &mut *(lparam.0 as *mut WindowSearch);
    let mut window_pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut window_pid as *mut u32));
    if window_pid != search.pid || !IsWindowVisible(hwnd).as_bool() {
        return BOOL(1);
    }
    // 有 owner 的通常是对话框 / 浮动工具窗；带 WS_EX_TOOLWINDOW 的也不进任务栏
    if GetWindow(hwnd, GW_OWNER).is_ok() {
        return BOOL(1);
    }
    let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
    if ex_style & WS_EX_TOOLWINDOW.0 != 0 {
        return BOOL(1);
    }
    let mut rect = RECT::default();
    if GetWindowRect(hwnd, &mut rect).is_err() {
        return BOOL(1);
    }
    // 坐标是 i32，直接相乘可能溢出 i64；屏幕尺寸封顶 10 万足够
    let width = (rect.right - rect.left).clamp(0, 100_000) as i64;
    let height = (rect.bottom - rect.top).clamp(0, 100_000) as i64;
    if width == 0 || height == 0 {
        return BOOL(1);
    }
    let title = window_title(hwnd);
    let score = (if title.is_empty() { 0 } else { 1_000_000_000 }) + width * height;
    if score > search.best_score {
        search.best_score = score;
        search.best = Some(hwnd);
    }
    BOOL(1)
}

fn window_title(hwnd: HWND) -> String {
    unsafe {
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return String::new();
        }
        let mut buffer = vec![0u16; len as usize + 1];
        let written = GetWindowTextW(hwnd, &mut buffer);
        if written <= 0 {
            return String::new();
        }
        String::from_utf16_lossy(&buffer[..written as usize])
    }
}

/// 还原（如果最小化了）+ 置前。Windows 默认禁止后台进程抢前台，
/// 所以失败时把当前线程的输入队列临时挂到前台 / 目标线程上再抢一次。
fn focus_window(hwnd: HWND) -> bool {
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        } else {
            let _ = ShowWindow(hwnd, SW_SHOW);
        }
        if SetForegroundWindow(hwnd).as_bool() {
            return true;
        }

        let current = GetCurrentThreadId();
        let target_thread = GetWindowThreadProcessId(hwnd, None);
        let foreground = GetForegroundWindow();
        let foreground_thread = if foreground.is_invalid() {
            0
        } else {
            GetWindowThreadProcessId(foreground, None)
        };

        let attach_foreground = foreground_thread != 0
            && foreground_thread != current
            && AttachThreadInput(current, foreground_thread, true).as_bool();
        let attach_target = target_thread != 0
            && target_thread != current
            && AttachThreadInput(current, target_thread, true).as_bool();

        let _ = BringWindowToTop(hwnd);
        let foreground_ok = SetForegroundWindow(hwnd).as_bool();

        if attach_target {
            let _ = AttachThreadInput(current, target_thread, false);
        }
        if attach_foreground {
            let _ = AttachThreadInput(current, foreground_thread, false);
        }
        if foreground_ok {
            return true;
        }
        // 前台锁定时这个老 API 通常也能把窗口带到最前
        SwitchToThisWindow(hwnd, true);
        GetForegroundWindow() == hwnd
    }
}

/// 起始位置：只给本地普通文件设「所在目录」。
///
/// 网址、`shell:` 之类的伪协议、以及快捷方式都返回 None ——
/// 快捷方式自带的「起始位置」是作者精心设好的，不能被我们覆盖掉。
fn launch_directory(target: &str) -> Option<std::path::PathBuf> {
    if target.contains("://") {
        return None;
    }
    let path = Path::new(target);
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if matches!(ext.as_str(), "lnk" | "url") {
        return None;
    }
    if !path.is_file() {
        return None;
    }
    path.parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.to_path_buf())
}

/// 把 `%ProgramFiles%\Foo` 这类路径展开成真实路径；没有变量或解析失败时原样返回
pub fn expand_env_vars(text: &str) -> String {
    if !text.contains('%') {
        return text.to_string();
    }
    let source: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let needed = ExpandEnvironmentStringsW(PCWSTR(source.as_ptr()), None);
        if needed == 0 {
            return text.to_string();
        }
        let mut buffer = vec![0u16; needed as usize];
        let written = ExpandEnvironmentStringsW(PCWSTR(source.as_ptr()), Some(&mut buffer));
        if written == 0 {
            return text.to_string();
        }
        let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
        String::from_utf16_lossy(&buffer[..end])
    }
}

/// 当前进程是否以管理员权限（提权）运行。
///
/// 提权后 Windows 的 UIPI 会**禁止**把文件从资源管理器（普通权限）拖进本窗口 ——
/// 拖动时只显示一个禁止图标，松手也不会产生任何事件，应用侧无法拦截或补救。
/// 所以启动时把这件事告诉用户（见 App.tsx）。
pub fn is_elevated() -> bool {
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut size = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut c_void),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut size,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok && elevation.TokenIsElevated != 0
    }
}

/// ShellExecuteW 的失败码翻译成能看懂的提示（返回码 <= 32）
fn shell_error_text(code: isize) -> String {
    let reason = match code {
        0 | 8 => "内存不足",
        2 => "找不到该文件，可能已被移动或删除",
        3 => "找不到该路径",
        5 => "访问被拒绝，可能需要以管理员身份运行",
        26 => "文件正在被占用",
        27 => "文件关联不完整",
        28 => "DDE 操作超时",
        29 => "DDE 操作失败",
        30 => "DDE 忙",
        31 => "没有与之关联的程序",
        32 => "找不到所需的 DLL",
        _ => "未知错误",
    };
    format!("{reason}，错误码 {code}")
}

/// 在资源管理器中定位文件。
/// 路径不存在时逐级向上，打开最近一个真实存在的目录
/// （例如 D:\app\1.exe 没了就打开 D:\app，D:\app 也没了就打开 D:\）。
pub fn reveal_in_explorer(path: &str) -> Result<(), String> {
    if path.trim().is_empty() {
        return Err("路径为空".into());
    }
    let open = |arg: String| -> Result<(), String> {
        Command::new("explorer")
            .arg(arg)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("调用资源管理器失败: {e}"))
    };

    let target = Path::new(path);
    if target.is_dir() {
        return open(path.to_string());
    }
    if target.is_file() {
        return open(format!("/select,{path}"));
    }

    let mut dir = target.parent();
    while let Some(current) = dir {
        if current.is_dir() {
            return open(current.to_string_lossy().to_string());
        }
        dir = current.parent();
    }
    Err("该路径及其上级目录都不存在".into())
}

/// 按扩展名决定取图标的方式：图片文件直接读，其它用系统 shell 图标
pub fn icon_from_path(path: &str) -> Result<IconData, String> {
    if !Path::new(path).exists() {
        return Err(format!("文件不存在: {path}"));
    }
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "ico" => image_file_to_icon(path),
        _ => shell_icon(path, ICON_MAX),
    }
}

/// 本地图片 -> PNG base64
fn image_file_to_icon(path: &str) -> Result<IconData, String> {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let format = match ext.as_str() {
        "png" => Some(image::ImageFormat::Png),
        "jpg" | "jpeg" => Some(image::ImageFormat::Jpeg),
        "gif" => Some(image::ImageFormat::Gif),
        "bmp" => Some(image::ImageFormat::Bmp),
        "webp" => Some(image::ImageFormat::WebP),
        "ico" => Some(image::ImageFormat::Ico),
        _ => None,
    };
    let mut reader = image::ImageReader::open(path).map_err(|e| format!("读取图片失败: {e}"))?;
    if let Some(f) = format {
        reader.set_format(f);
    }
    let img = reader.decode().map_err(|e| format!("解析图片失败: {e}"))?;
    rgba_to_png_icon(img.to_rgba8(), ICON_MAX)
}

/// 提取 exe / 快捷方式 / 文件夹 / 任意文件的系统图标
pub fn shell_icon(path: &str, size: u32) -> Result<IconData, String> {
    unsafe {
        // 该线程可能还没初始化 COM（Tauri 的线程池线程），失败也无妨
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        let factory: IShellItemImageFactory =
            SHCreateItemFromParsingName(&HSTRING::from(path), None)
                .map_err(|e| format!("无法读取系统图标: {e}"))?;

        let hbmp: HBITMAP = factory
            .GetImage(
                SIZE {
                    cx: size as i32,
                    cy: size as i32,
                },
                SIIGBF_ICONONLY | SIIGBF_BIGGERSIZEOK,
            )
            .map_err(|e| format!("无法读取系统图标: {e}"))?;

        let result = hbitmap_to_rgba(hbmp);
        let _ = DeleteObject(HGDIOBJ(hbmp.0));
        let (rgba, w, h) = result?;
        let img = image::RgbaImage::from_raw(w, h, rgba).ok_or_else(|| "图标尺寸异常".to_string())?;
        // 有些 exe 的图标只占画布中间一小块，先裁掉多余留白再缩放，显示时才不会显得很小
        rgba_to_png_icon(trim_transparent(img), ICON_MAX)
    }
}

unsafe fn hbitmap_to_rgba(hbmp: HBITMAP) -> Result<(Vec<u8>, u32, u32), String> {
    let mut bm = BITMAP::default();
    let got = GetObjectW(
        HGDIOBJ(hbmp.0),
        std::mem::size_of::<BITMAP>() as i32,
        Some(&mut bm as *mut _ as *mut c_void),
    );
    if got == 0 {
        return Err("读取位图信息失败".into());
    }
    let w = bm.bmWidth;
    let h = bm.bmHeight;
    if w <= 0 || h <= 0 {
        return Err("位图尺寸无效".into());
    }

    let mut info = BITMAPINFO::default();
    info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    info.bmiHeader.biWidth = w;
    info.bmiHeader.biHeight = -h; // 负数 = 自上而下
    info.bmiHeader.biPlanes = 1;
    info.bmiHeader.biBitCount = 32;
    info.bmiHeader.biCompression = BI_RGB.0;

    let mut buf = vec![0u8; (w as usize) * (h as usize) * 4];
    let hdc = GetDC(None);
    if hdc.is_invalid() {
        return Err("获取绘图上下文失败".into());
    }
    let lines = GetDIBits(
        hdc,
        hbmp,
        0,
        h as u32,
        Some(buf.as_mut_ptr() as *mut c_void),
        &mut info,
        DIB_RGB_COLORS,
    );
    ReleaseDC(None, hdc);
    if lines == 0 {
        return Err("读取位图像素失败".into());
    }

    // BGRA -> RGBA；GetImage 返回的位图通常是预乘 alpha
    let has_alpha = buf.chunks_exact(4).any(|p| p[3] != 0);
    let premultiplied = has_alpha
        && buf
            .chunks_exact(4)
            .all(|p| p[0] <= p[3] && p[1] <= p[3] && p[2] <= p[3]);

    let mut out = vec![0u8; buf.len()];
    for (i, px) in buf.chunks_exact(4).enumerate() {
        let (b, g, r, mut a) = (px[0], px[1], px[2], px[3]);
        if !has_alpha {
            a = 255;
        }
        let (r, g, b) = if premultiplied && a > 0 && a < 255 {
            let scale = |c: u8| ((c as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8;
            (scale(r), scale(g), scale(b))
        } else {
            (r, g, b)
        };
        out[i * 4] = r;
        out[i * 4 + 1] = g;
        out[i * 4 + 2] = b;
        out[i * 4 + 3] = a;
    }
    Ok((out, w as u32, h as u32))
}

/// 裁掉四周的透明留白：部分 exe / 快捷方式的图标在小画布里只占中间一块，
/// 直接显示会显得很小。
/// 判定要点：只把「实心」像素（alpha > 128）算作内容 —— 有些快捷方式图标带一层
/// alpha 很低的半透明底板，若按 alpha > 8 统计，底板会把边界框撑满，主体就裁不出来；
/// 行 / 列再要求达到峰值 15%（至少 3 个像素）才算主体，忽略零星离群点。
fn trim_transparent(img: image::RgbaImage) -> image::RgbaImage {
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        return img;
    }

    let mut rows = vec![0u32; h as usize];
    let mut cols = vec![0u32; w as usize];
    for (x, y, pixel) in img.enumerate_pixels() {
        if pixel[3] > 128 {
            rows[y as usize] += 1;
            cols[x as usize] += 1;
        }
    }

    let threshold = |counts: &[u32]| (counts.iter().copied().max().unwrap_or(0) * 15 / 100).max(3);
    let row_min = threshold(&rows);
    let col_min = threshold(&cols);

    let top = rows.iter().position(|&c| c >= row_min).unwrap_or(0);
    let bottom = rows.iter().rposition(|&c| c >= row_min).unwrap_or(h as usize - 1);
    let left = cols.iter().position(|&c| c >= col_min).unwrap_or(0);
    let right = cols.iter().rposition(|&c| c >= col_min).unwrap_or(w as usize - 1);
    if bottom < top || right < left {
        return img;
    }

    let (top, left, bottom, right) = (top as u32, left as u32, bottom as u32, right as u32);
    let content_w = right - left + 1;
    let content_h = bottom - top + 1;
    // 主体本来就铺满时保持原样，不破坏有意留白的设计
    if content_w * 100 / w >= 85 && content_h * 100 / h >= 85 {
        return img;
    }

    // 留 1px 边距，避免裁得过紧
    let x = left.saturating_sub(1);
    let y = top.saturating_sub(1);
    let cw = (content_w + 2).min(w - x);
    let ch = (content_h + 2).min(h - y);
    image::imageops::crop_imm(&img, x, y, cw, ch).to_image()
}

#[cfg(test)]
mod trim_tests {
    use super::{expand_env_vars, launch_directory, trim_transparent};
    use image::{Rgba, RgbaImage};
    use std::path::PathBuf;

    /// 造一张 w×h 的画布，可指定其中一块不透明区域
    fn canvas(w: u32, h: u32, inner: Option<(u32, u32, u32, u32)>) -> RgbaImage {
        let mut img = RgbaImage::from_pixel(w, h, Rgba([0, 0, 0, 0]));
        if let Some((x, y, cw, ch)) = inner {
            for px in x..x + cw {
                for py in y..y + ch {
                    img.put_pixel(px, py, Rgba([255, 0, 0, 255]));
                }
            }
        }
        img
    }

    #[test]
    fn trims_content_with_large_padding() {
        // 64×64 画布里只有中间 16×16 有内容 -> 裁到 18×18（含 1px 边距）
        let out = trim_transparent(canvas(64, 64, Some((24, 24, 16, 16))));
        assert_eq!(out.dimensions(), (18, 18));
    }

    #[test]
    fn keeps_content_that_fills_canvas() {
        let out = trim_transparent(canvas(64, 64, Some((0, 0, 64, 64))));
        assert_eq!(out.dimensions(), (64, 64));
    }

    #[test]
    fn keeps_slightly_inset_content() {
        // 只留 2px 边距（96% 占比）时不应裁剪
        let out = trim_transparent(canvas(64, 64, Some((2, 2, 60, 60))));
        assert_eq!(out.dimensions(), (64, 64));
    }

    #[test]
    fn keeps_fully_transparent_image() {
        let out = trim_transparent(canvas(64, 64, None));
        assert_eq!(out.dimensions(), (64, 64));
    }

    #[test]
    fn ignores_corner_noise_when_trimming() {
        // 中心 16×16 是主体，但四角有零星像素（快捷方式图标常见）——
        // 旧实现会被这些杂点骗过而放弃裁剪，这里要求仍裁到主体附近。
        let mut img = canvas(64, 64, Some((24, 24, 16, 16)));
        for (x, y) in [(0u32, 0u32), (63, 0), (0, 63), (63, 63)] {
            img.put_pixel(x, y, Rgba([255, 255, 255, 255]));
        }
        let out = trim_transparent(img);
        let (w, h) = out.dimensions();
        assert!(w <= 20 && h <= 20, "应裁到主体附近，实际为 {w}×{h}");
    }

    #[test]
    fn exe_gets_its_own_folder_as_working_directory() {
        // 不设起始位置时子进程会继承本应用的目录，依赖相对路径资源的程序会异常
        let dir = launch_directory(r"C:\Windows\System32\notepad.exe");
        assert_eq!(dir, Some(PathBuf::from(r"C:\Windows\System32")));
    }

    #[test]
    fn shortcuts_and_urls_keep_their_own_startup_settings() {
        // .lnk 里的「起始位置」是作者设好的，不能被我们覆盖
        assert_eq!(launch_directory(r"C:\Users\Tian\Desktop\Steam.lnk"), None);
        assert_eq!(launch_directory(r"C:\Users\Tian\Desktop\GitHub.url"), None);
        assert_eq!(launch_directory("https://example.com"), None);
        // 不存在的路径不硬塞目录，免得 ShellExecuteW 报「找不到该路径」
        assert_eq!(launch_directory(r"C:\No\Such\App\tool.exe"), None);
    }

    #[test]
    fn environnment_variables_are_expanded_before_opening() {
        let system_root = std::env::var("SystemRoot").expect("Windows 上一定有 SystemRoot");
        assert_eq!(
            expand_env_vars(r"%SystemRoot%\System32\notepad.exe"),
            format!(r"{system_root}\System32\notepad.exe")
        );
        // 没有变量的普通路径原样返回
        assert_eq!(expand_env_vars(r"D:\App\app.exe"), r"D:\App\app.exe");
    }

    #[test]
    fn paths_compare_after_normalizing_separators_and_prefixes() {
        assert!(super::same_path(
            r"C:\Program Files\App\app.exe",
            r"c:/program files/app/app.exe"
        ));
        assert!(super::same_path(
            r"\\?\C:\Program Files\App\app.exe",
            r"C:\Program Files\App\app.exe"
        ));
        assert!(!super::same_path(
            r"C:\Program Files\App\app.exe",
            r"C:\Program Files\App\other.exe"
        ));
    }

    #[test]
    fn host_processes_are_not_treated_as_the_app() {
        assert!(super::is_host_process(r"C:\Windows\explorer.exe"));
        assert!(super::is_host_process(r"C:\Windows\System32\cmd.exe"));
        assert!(!super::is_host_process(
            r"C:\Program Files\Google\Chrome\Application\chrome.exe"
        ));
    }

    #[test]
    #[ignore = "需要交互式桌面，手动运行：cargo test --lib activates_running_charmap -- --ignored"]
    fn activates_running_charmap() {
        let exe = r"C:\Windows\System32\charmap.exe";
        if !std::path::Path::new(exe).is_file() {
            return;
        }
        let mut child = std::process::Command::new(exe)
            .spawn()
            .expect("启动字符映射表");
        // 给窗口一点创建时间；找不到窗口时 activate_running_app 会返回 false
        std::thread::sleep(std::time::Duration::from_millis(1500));
        let activated = super::activate_running_app(exe);
        let _ = child.kill();
        let _ = child.wait();
        assert!(activated, "应识别到已运行的 charmap 并置顶窗口");
    }
}

/// RGBA 图像 -> PNG base64（超过 max 边长时等比缩小）
pub fn rgba_to_png_icon(img: image::RgbaImage, max: u32) -> Result<IconData, String> {
    let (w, h) = (img.width(), img.height());
    if w == 0 || h == 0 {
        return Err("图像尺寸无效".into());
    }
    let img = if w.max(h) > max {
        let scale = max as f32 / w.max(h) as f32;
        let nw = ((w as f32 * scale).round() as u32).max(1);
        let nh = ((h as f32 * scale).round() as u32).max(1);
        image::imageops::resize(&img, nw, nh, image::imageops::FilterType::Lanczos3)
    } else {
        img
    };

    let mut bytes: Vec<u8> = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
        .map_err(|e| format!("编码 PNG 失败: {e}"))?;

    Ok(IconData {
        mime: "image/png".into(),
        data: STANDARD.encode(&bytes),
    })
}
