//! 快捷方式解析：把 .lnk / .url 还原成它们真正指向的目标。
//!
//! ⚠️ 解析的原则是「能直开才直开」：很多快捷方式把**启动参数**、**起始位置**、
//! **以管理员身份运行**记录在自己身上（Steam 游戏、应用商店 / UWP 应用、
//! `cmd /c xxx.bat`、`javaw -jar xxx.jar`、带 `--profile` 的浏览器…）。
//! 只把 .lnk 换成它指向的 exe，这些信息会一起丢掉 —— 表现就是
//! 「点了没反应」「打开的是别的程序」「程序起来了但用不了」。所以只有确认
//! 不依赖这些信息时，才解析成真实路径；否则保留 .lnk 本身，交给系统按
//! 快捷方式启动（等价于在资源管理器里双击它）。

use std::path::Path;

use windows::core::{Interface, HSTRING};
use windows::Win32::Foundation::MAX_PATH;
use windows::Win32::Storage::FileSystem::WIN32_FIND_DATAW;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, IPersistFile, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
    STGM_READ,
};
use windows::Win32::UI::Shell::{IShellLinkDataList, IShellLinkW, ShellLink, SLDF_RUNAS_USER};

/// 拖入的条目：原始路径 + 解析后的目标 + 类型 + 建议名称
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DroppedEntry {
    pub path: String,
    pub target: String,
    /// "app" 或 "url"
    pub kind: String,
    pub name: String,
}

/// 快捷方式里记录的启动信息（只有 IShellLink 能拿到，光看目标路径看不出来）
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LinkInfo {
    /// 目标路径：已展开环境变量、已补全相对路径
    pub target: String,
    /// 启动参数
    pub arguments: String,
    /// 「起始位置」（很多程序靠它找同级资源）
    pub working_dir: String,
    /// 快捷方式勾了「以管理员身份运行」
    pub run_as_admin: bool,
}

/// .lnk -> 它指向的路径；.url -> 里面的网址；其它文件返回 None
///
/// 返回 None 有两种含义：根本不是快捷方式，或者**这个快捷方式不能只靠目标路径启动**
/// （带参数 / 依赖起始位置 / 要提权），这时调用方应该原样使用传进来的路径。
pub fn resolve_launch_target(path: &str) -> Option<String> {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".lnk") {
        resolve_lnk(path)
    } else if lower.ends_with(".url") {
        resolve_url_file(path)
    } else {
        None
    }
}

/// 读取 Internet 快捷方式（.url 是 INI 文本，可能有 BOM 或 ANSI 编码）
fn resolve_url_file(path: &str) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    for line in text.lines() {
        let line = line.trim().trim_start_matches('\u{feff}');
        if line.len() >= 4 && line[..4].eq_ignore_ascii_case("url=") {
            let url = line[4..].trim();
            if !url.is_empty() {
                return Some(url.to_string());
            }
        }
    }
    None
}

/// 用 COM 的 IShellLink 读出 .lnk 的启动信息
pub fn read_lnk(path: &str) -> Option<LinkInfo> {
    unsafe {
        // 线程可能尚未初始化 COM，重复初始化会返回 S_FALSE，忽略即可
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
        let persist: IPersistFile = link.cast().ok()?;
        persist.Load(&HSTRING::from(path), STGM_READ).ok()?;

        let target = link_target(&link)?;
        let arguments = link_text(|buf| link.GetArguments(buf));
        let working_dir = link_text(|buf| link.GetWorkingDirectory(buf));
        // 「以管理员身份运行」只写在 .lnk 的 data block 里，看目标路径看不出来
        let run_as_admin = link
            .cast::<IShellLinkDataList>()
            .and_then(|list| list.GetFlags())
            .map(|flags| flags & SLDF_RUNAS_USER.0 as u32 != 0)
            .unwrap_or(false);

        Some(LinkInfo {
            target,
            arguments,
            working_dir,
            run_as_admin,
        })
    }
}

/// 读 .lnk 的目标路径。
///
/// flags 必须传 0（默认）：这样 shell 会把 `%ProgramFiles%\…` 这类环境变量**展开**、
/// 把相对路径补全。传 SLGP_SHORTPATH(1) 会得到 `C:\PROGRA~1\…` 这种 8.3 短名，
/// 传 SLGP_RAWPATH(4) 会留下没展开的 `%变量%` —— 两种都可能导致「点了打不开」。
unsafe fn link_target(link: &IShellLinkW) -> Option<String> {
    let mut buffer = [0u16; MAX_PATH as usize];
    let mut find_data = WIN32_FIND_DATAW::default();
    link.GetPath(&mut buffer, &mut find_data, 0).ok()?;
    let path = from_wide(&buffer);
    if path.is_empty() {
        None
    } else {
        Some(path)
    }
}

/// 读 .lnk 里的一段文本（参数 / 起始位置）。参数可能比 MAX_PATH 长，留足空间。
unsafe fn link_text(read: impl FnOnce(&mut [u16]) -> windows::core::Result<()>) -> String {
    let mut buffer = [0u16; 2048];
    if read(&mut buffer).is_err() {
        return String::new();
    }
    from_wide(&buffer)
}

fn from_wide(buffer: &[u16]) -> String {
    let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..end]).trim().to_string()
}

/// 这个快捷方式能不能「只打开它指向的目标」而不改变行为？
///
/// 返回 false 时必须原样打开 .lnk，否则会丢掉启动信息。
pub fn can_open_target_directly(info: &LinkInfo) -> bool {
    let target = info.target.trim();
    if target.is_empty() {
        return false;
    }
    // 带参数：宿主要靠自己（explorer / cmd / java / steam …），exe 本身不接受这些参数
    if !info.arguments.trim().is_empty() {
        return false;
    }
    // 要提权：ShellExecuteW 打开 exe 时不会带上 .lnk 的 RunAs 标记，应用会因权限不足出问题
    if info.run_as_admin {
        return false;
    }
    // 目标必须真实存在：应用商店（UWP）快捷方式的目标常常是 AppExecutionAlias，
    // 直接执行很容易被拒
    if !Path::new(target).is_file() {
        return false;
    }
    // 包内的 exe（C:\Program Files\WindowsApps\…）权限受限，只认 shell 启动
    if target.to_ascii_lowercase().contains("\\windowsapps\\") {
        return false;
    }
    // 起始位置与目标所在目录不同：程序可能依赖相对路径的资源 / 配置
    let work_dir = info.working_dir.trim();
    if !work_dir.is_empty() {
        let target_dir = Path::new(target).parent();
        let same = target_dir
            .map(|dir| same_path(dir, Path::new(work_dir)))
            .unwrap_or(false);
        if !same {
            return false;
        }
    }
    true
}

/// 路径比较：忽略大小写与结尾的分隔符（`C:\App\` 与 `c:\app` 视为同一个）
fn same_path(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| {
        p.to_string_lossy()
            .trim_end_matches(['\\', '/'])
            .to_ascii_lowercase()
    };
    norm(a) == norm(b)
}

/// 用 COM 的 IShellLink 读出 .lnk 的目标路径（仅当可以安全直开时）
fn resolve_lnk(path: &str) -> Option<String> {
    let info = read_lnk(path)?;
    if can_open_target_directly(&info) {
        Some(info.target.trim().to_string())
    } else {
        None
    }
}

/// 把一个待添加的文件整理成条目信息：快捷方式解析成目标，.url 变成网址
pub fn describe_dropped(path: &str) -> DroppedEntry {
    const APP_EXTS: [&str; 4] = [".exe", ".lnk", ".bat", ".cmd"];

    let original = path.to_string();
    let resolved = resolve_launch_target(path);
    let lower = path.to_ascii_lowercase();
    let is_url_file = lower.ends_with(".url");

    let target = resolved.clone().unwrap_or_else(|| path.to_string());
    let kind = if is_url_file {
        "url"
    } else if resolved.is_some() || APP_EXTS.iter().any(|ext| lower.ends_with(ext)) {
        "app"
    } else {
        // 其它文件按「文件」处理，双击时用系统默认程序打开
        "file"
    }
    .to_string();
    let name = if is_url_file {
        // 网址条目用域名作名字更直观
        host_of(&target).unwrap_or_else(|| file_stem(path))
    } else {
        // 拖的是快捷方式时，用快捷方式自己的名字（而不是它指向的源文件名）
        file_stem(path)
    };

    DroppedEntry {
        path: original,
        target,
        kind,
        name,
    }
}

fn file_stem(path: &str) -> String {
    Path::new(path)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| path.to_string())
}

fn host_of(url: &str) -> Option<String> {
    let rest = url.split("://").nth(1)?;
    let host = rest.split(['/', '?', '#']).next()?;
    let host = host.trim_start_matches("www.");
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::{can_open_target_directly, describe_dropped, resolve_launch_target, LinkInfo};

    fn temp_url_file(name: &str, content: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(name);
        std::fs::write(&path, content).unwrap();
        path
    }

    fn info(target: &str, arguments: &str, working_dir: &str, run_as_admin: bool) -> LinkInfo {
        LinkInfo {
            target: target.into(),
            arguments: arguments.into(),
            working_dir: working_dir.into(),
            run_as_admin,
        }
    }

    /// 一个真实存在、仅用于测试的文件
    fn real_file() -> String {
        std::env::current_exe()
            .expect("取测试可执行文件路径失败")
            .to_string_lossy()
            .to_string()
    }

    #[test]
    fn resolves_url_file_to_its_website() {
        let path = temp_url_file(
            "quick-manage-test-a.url",
            "[InternetShortcut]\r\nURL=https://example.com/a\r\nIconIndex=0\r\n",
        );
        let target = resolve_launch_target(path.to_str().unwrap());
        assert_eq!(target.as_deref(), Some("https://example.com/a"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn dropped_url_becomes_a_website_entry() {
        let path = temp_url_file(
            "quick-manage-test-b.url",
            "[InternetShortcut]\nURL=https://www.github.com/waiseset\n",
        );
        let entry = describe_dropped(path.to_str().unwrap());
        assert_eq!(entry.kind, "url", "Internet 快捷方式应识别为网址");
        assert_eq!(entry.target, "https://www.github.com/waiseset");
        assert_eq!(entry.name, "github.com", "名称应取域名（去掉 www.）");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn plain_executable_keeps_its_path() {
        let entry = describe_dropped(r"C:\Windows\System32\notepad.exe");
        assert_eq!(entry.kind, "app");
        assert_eq!(entry.target, r"C:\Windows\System32\notepad.exe");
        assert_eq!(entry.name, "notepad");
    }

    #[test]
    fn plain_target_can_be_opened_directly() {
        assert!(can_open_target_directly(&info(&real_file(), "", "", false)));
    }

    #[test]
    fn target_with_arguments_must_go_through_the_shortcut() {
        // 典型：explorer.exe shell:AppsFolder\…（应用商店应用）、cmd /c run.bat
        let exe = real_file();
        assert!(!can_open_target_directly(&info(
            &exe,
            "shell:AppsFolder\\Microsoft.WindowsCalculator_8wekyb3d8bbwe!App",
            "",
            false
        )));
    }

    #[test]
    fn target_with_its_own_working_dir_can_still_be_direct() {
        let exe = real_file();
        let dir = std::path::Path::new(&exe).parent().unwrap().to_string_lossy().to_string();
        assert!(can_open_target_directly(&info(&exe, "", &dir, false)));
        // 大小写 / 结尾分隔符不同也算同一个目录
        assert!(can_open_target_directly(&info(&exe, "", &format!("{dir}\\"), false)));
    }

    #[test]
    fn target_relying_on_another_working_dir_keeps_the_shortcut() {
        let exe = real_file();
        assert!(!can_open_target_directly(&info(
            &exe,
            "",
            r"C:\Some\Other\Folder",
            false
        )));
    }

    #[test]
    fn elevated_shortcut_keeps_the_shortcut() {
        let exe = real_file();
        assert!(!can_open_target_directly(&info(&exe, "", "", true)));
    }

    #[test]
    fn missing_target_keeps_the_shortcut() {
        // 目标不存在（应用商店别名、已被移动的软件）时不要凭空造一个坏路径
        assert!(!can_open_target_directly(&info(
            r"C:\No\Such\App\tool.exe",
            "",
            "",
            false
        )));
    }

    /// 真实 .lnk 的读取：用 WScript.Shell 造快捷方式，验证 COM 侧字段都读对了。
    /// 默认跳过（要起 PowerShell），手动验：
    /// `cargo test --lib -- --ignored real_shortcut`
    #[test]
    #[ignore = "需要真实创建 .lnk（调用 WScript.Shell）"]
    fn reads_real_shortcuts_from_disk() {
        let dir = std::env::temp_dir().join("quick-manage-lnk-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let with_args = dir.join("with-args.lnk");
        let plain = dir.join("plain.lnk");
        let notepad = std::env::var("SystemRoot").unwrap() + r"\System32\notepad.exe";
        let notepad_dir = std::path::Path::new(&notepad)
            .parent()
            .unwrap()
            .to_string_lossy()
            .to_string();

        let script = format!(
            "$s = New-Object -ComObject WScript.Shell; \
             $a = $s.CreateShortcut('{args}'); $a.TargetPath = '{exe}'; \
             $a.Arguments = '--flag 1'; $a.WorkingDirectory = '{dir}'; $a.Save(); \
             $b = $s.CreateShortcut('{plain}'); $b.TargetPath = '{exe}'; \
             $b.WorkingDirectory = '{dir}'; $b.Save()",
            args = with_args.to_string_lossy(),
            plain = plain.to_string_lossy(),
            exe = notepad,
            dir = notepad_dir,
        );
        let status = std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .status()
            .expect("调用 PowerShell 失败");
        assert!(status.success(), "创建测试快捷方式失败");

        // 带参数：四个字段都要读出来，且必须保留 .lnk 启动
        let info = super::read_lnk(with_args.to_str().unwrap()).expect("读取带参数快捷方式失败");
        assert_eq!(info.target.to_ascii_lowercase(), notepad.to_ascii_lowercase());
        assert_eq!(info.arguments, "--flag 1");
        assert_eq!(info.working_dir.to_ascii_lowercase(), notepad_dir.to_ascii_lowercase());
        assert!(!info.run_as_admin, "没勾提权时不应误判");
        assert!(!super::can_open_target_directly(&info));
        assert_eq!(resolve_launch_target(with_args.to_str().unwrap()), None);

        // 不带参数、起始位置就是 exe 所在目录：可以直开成 exe
        let info = super::read_lnk(plain.to_str().unwrap()).expect("读取普通快捷方式失败");
        assert!(super::can_open_target_directly(&info));
        assert_eq!(
            resolve_launch_target(plain.to_str().unwrap()).map(|p| p.to_ascii_lowercase()),
            Some(notepad.to_ascii_lowercase())
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
