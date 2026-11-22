//! Windows 系统调用：ShellExecuteW 打开条目、explorer /select 定位文件、
//! 通过 IShellItemImageFactory 提取 exe/快捷方式/文件夹的系统图标。

use std::ffi::c_void;
use std::path::Path;
use std::process::Command;

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;

use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, SIZE};
use windows::Win32::Graphics::Gdi::{
    DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, DIB_RGB_COLORS, HBITMAP, HGDIOBJ,
};
use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::Environment::ExpandEnvironmentStringsW;
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::Win32::UI::Shell::{
    IShellItemImageFactory, SHCreateItemFromParsingName, ShellExecuteW, SIIGBF_BIGGERSIZEOK,
    SIIGBF_ICONONLY,
};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

use crate::model::IconData;

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
