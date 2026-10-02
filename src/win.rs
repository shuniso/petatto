//! Win32 の UI 本体。常駐する非表示ウィンドウがホットキーを受け、検索ポップアップを出す。

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::mem::{size_of, transmute, zeroed};
use std::path::PathBuf;
use std::ptr::{copy_nonoverlapping, null, null_mut};
use std::time::Duration;

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Dwm::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::DataExchange::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::*;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;
use windows_sys::w;

use crate::config::{self, Snippet};

const CF_UNICODETEXT: u32 = 13;
const SS_ETCHEDHORZ: u32 = 0x10;
const HOTKEY_ID: i32 = 1;
const WM_TRAY: u32 = WM_APP + 1;
const TIMER_PASTE: usize = 1;
const TIMER_RESTORE: usize = 2;
const ID_OPEN: usize = 1;
const ID_RELOAD: usize = 2;
const ID_EXIT: usize = 3;

// 96dpi 基準の寸法
const WIDTH: i32 = 640;
const HEIGHT: i32 = 400;
const FONT_PX: i32 = 16;
const PAD: i32 = 12;
const EDIT_HEIGHT: i32 = 24;
/// 名前と本文プレビューの間のタブ位置(ダイアログ単位)
const TAB_STOP: i32 = 110;

/// フォーカスを貼り付け先へ戻してから Ctrl+V を送るまでの待ち
const PASTE_DELAY_MS: u32 = 80;
/// 貼り付け後、元のクリップボードを書き戻すまでの待ち
const RESTORE_DELAY_MS: u32 = 500;

struct State {
    main: Cell<HWND>,
    edit: Cell<HWND>,
    sep: Cell<HWND>,
    list: Cell<HWND>,
    edit_proc: Cell<WNDPROC>,
    font: Cell<HFONT>,
    dpi: Cell<u32>,
    taskbar_created: Cell<u32>,
    /// ポップアップを出す直前に前面だったウィンドウ(貼り付け先)
    target: Cell<HWND>,
    /// 現在登録されているホットキーの設定文字列(未登録なら空)
    hotkey: RefCell<String>,
    snippets: RefCell<Vec<Snippet>>,
    /// 一覧の各行に対応する snippets の添字
    hits: RefCell<Vec<usize>>,
    error: RefCell<Option<String>>,
    saved_clip: RefCell<Option<Vec<u16>>>,
    restore_pending: Cell<bool>,
}

struct Global(State);
// SAFETY: State に触れるのはメッセージループを回す UI スレッドだけ。
unsafe impl Sync for Global {}

static STATE: Global = Global(State {
    main: Cell::new(null_mut()),
    edit: Cell::new(null_mut()),
    sep: Cell::new(null_mut()),
    list: Cell::new(null_mut()),
    edit_proc: Cell::new(None),
    font: Cell::new(null_mut()),
    dpi: Cell::new(0),
    taskbar_created: Cell::new(0),
    target: Cell::new(null_mut()),
    hotkey: RefCell::new(String::new()),
    snippets: RefCell::new(Vec::new()),
    hits: RefCell::new(Vec::new()),
    error: RefCell::new(None),
    saved_clip: RefCell::new(None),
    restore_pending: Cell::new(false),
});

fn st() -> &'static State {
    &STATE.0
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn config_path() -> PathBuf {
    let dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(PathBuf::from))
        .unwrap_or_default();
    dir.join(config::FILE_NAME)
}

fn message_box(text: &str) {
    unsafe {
        MessageBoxW(
            null_mut(),
            wide(text).as_ptr(),
            w!("ペタッと"),
            MB_OK | MB_ICONINFORMATION | MB_SETFOREGROUND,
        );
    }
}

pub fn run() {
    let s = st();
    unsafe {
        // 多重起動防止。ハンドルはプロセス終了まで保持する。
        let _mutex = CreateMutexW(null(), 0, w!("petatto-single-instance"));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return;
        }
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        s.taskbar_created
            .set(RegisterWindowMessageW(w!("TaskbarCreated")));

        let hinst = GetModuleHandleW(null());
        let class = w!("petatto");
        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: null_mut(),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: (COLOR_WINDOW + 1) as usize as HBRUSH,
            lpszMenuName: null(),
            lpszClassName: class,
        };
        RegisterClassW(&wc);
        let main = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            class,
            class,
            WS_POPUP | WS_CLIPCHILDREN,
            0,
            0,
            0,
            0,
            null_mut(),
            null_mut(),
            hinst,
            null(),
        );
        let child = |class: *const u16, style: u32| {
            CreateWindowExW(
                0,
                class,
                null(),
                WS_CHILD | WS_VISIBLE | style,
                0,
                0,
                0,
                0,
                main,
                null_mut(),
                hinst,
                null(),
            )
        };
        s.main.set(main);
        s.edit.set(child(w!("EDIT"), ES_AUTOHSCROLL as u32));
        s.sep.set(child(w!("STATIC"), SS_ETCHEDHORZ));
        s.list.set(child(
            w!("LISTBOX"),
            (LBS_NOTIFY | LBS_NOINTEGRALHEIGHT | LBS_USETABSTOPS) as u32 | WS_VSCROLL,
        ));
        let old = SetWindowLongPtrW(s.edit.get(), GWLP_WNDPROC, edit_proc as *const () as isize);
        s.edit_proc.set(transmute::<isize, WNDPROC>(old));

        // Windows 11 の角丸と影
        let corner = DWMWCP_ROUND;
        DwmSetWindowAttribute(
            main,
            DWMWA_WINDOW_CORNER_PREFERENCE as u32,
            &corner as *const _ as *const c_void,
            size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
        );

        tray(NIM_ADD);
        let first_run = !config_path().exists();
        match reload() {
            Err(e) => message_box(&e),
            Ok(()) if first_run => message_box(&format!(
                "ペタッとを起動しました。\n{} で呼び出せます。\n\nスニペットは {} に書きます(タスクトレイのアイコンから開けます)。",
                s.hotkey.borrow(),
                config_path().display()
            )),
            Ok(()) => {}
        }

        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// 設定ファイルを読み直し、スニペットとホットキーを更新する。
fn reload() -> Result<(), String> {
    let s = st();
    let path = config_path();
    if !path.exists() {
        let _ = std::fs::write(&path, config::SAMPLE);
    }
    let src = std::fs::read_to_string(&path).map_err(|e| {
        format!(
            "{} を読めません(UTF-8 で保存してください)\n{e}",
            path.display()
        )
    })?;
    let cfg =
        config::parse(&src).map_err(|e| format!("{} の書式エラー\n{e}", config::FILE_NAME))?;
    *s.snippets.borrow_mut() = cfg.snippets;
    apply_hotkey(&cfg.hotkey)
}

fn apply_hotkey(spec: &str) -> Result<(), String> {
    let s = st();
    if *s.hotkey.borrow() == spec {
        return Ok(());
    }
    let (mods, vk) =
        config::parse_hotkey(spec).ok_or_else(|| format!("hotkey \"{spec}\" を解釈できません"))?;
    let registered = unsafe {
        UnregisterHotKey(s.main.get(), HOTKEY_ID);
        RegisterHotKey(s.main.get(), HOTKEY_ID, mods | MOD_NOREPEAT, vk) != 0
    };
    if !registered {
        s.hotkey.borrow_mut().clear();
        return Err(format!(
            "ホットキー {spec} を登録できません(他のアプリが使用中の可能性があります)\n{} の hotkey を変更し、トレイの「再読み込み」を実行してください。",
            config::FILE_NAME
        ));
    }
    *s.hotkey.borrow_mut() = spec.to_string();
    Ok(())
}

fn tray(action: NOTIFY_ICON_MESSAGE) {
    unsafe {
        let mut nid: NOTIFYICONDATAW = zeroed();
        nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = st().main.get();
        nid.uID = 1;
        nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        nid.uCallbackMessage = WM_TRAY;
        nid.hIcon = LoadIconW(null_mut(), IDI_APPLICATION);
        let tip = wide("ペタッと");
        nid.szTip[..tip.len()].copy_from_slice(&tip);
        Shell_NotifyIconW(action, &nid);
    }
}

fn tray_menu() {
    let main = st().main.get();
    unsafe {
        let menu = CreatePopupMenu();
        AppendMenuW(menu, MF_STRING, ID_OPEN, w!("snippets.toml を開く"));
        AppendMenuW(menu, MF_STRING, ID_RELOAD, w!("再読み込み"));
        AppendMenuW(menu, MF_SEPARATOR, 0, null());
        AppendMenuW(menu, MF_STRING, ID_EXIT, w!("終了"));
        let mut pt = POINT { x: 0, y: 0 };
        GetCursorPos(&mut pt);
        // メニュー外クリックで閉じるようにするための定石
        SetForegroundWindow(main);
        TrackPopupMenu(menu, TPM_RIGHTBUTTON, pt.x, pt.y, 0, main, null());
        PostMessageW(main, WM_NULL, 0, 0);
        DestroyMenu(menu);
    }
}

fn open_config() {
    let path = config_path();
    let file = wide(&path.to_string_lossy());
    unsafe {
        let r = ShellExecuteW(
            null_mut(),
            w!("open"),
            file.as_ptr(),
            null(),
            null(),
            SW_SHOWNORMAL,
        );
        // .toml に関連付けがなければメモ帳で開く
        if r as usize <= 32 {
            let arg = wide(&format!("\"{}\"", path.display()));
            ShellExecuteW(
                null_mut(),
                w!("open"),
                w!("notepad.exe"),
                arg.as_ptr(),
                null(),
                SW_SHOWNORMAL,
            );
        }
    }
}

fn show_popup() {
    let s = st();
    unsafe {
        if IsWindowVisible(s.main.get()) != 0 {
            dismiss();
            return;
        }
        s.target.set(GetForegroundWindow());
        *s.error.borrow_mut() = reload().err();

        // 貼り付け先ウィンドウがあるモニターの、やや上寄り中央に出す
        let monitor = MonitorFromWindow(s.target.get(), MONITOR_DEFAULTTOPRIMARY);
        let mut mi: MONITORINFO = zeroed();
        mi.cbSize = size_of::<MONITORINFO>() as u32;
        GetMonitorInfoW(monitor, &mut mi);
        let (mut dpi, mut dpi_y) = (96, 96);
        GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi, &mut dpi_y);
        let (w, h) = (scale(WIDTH, dpi), scale(HEIGHT, dpi));
        let work = mi.rcWork;
        let x = work.left + (work.right - work.left - w) / 2;
        let y = work.top + (work.bottom - work.top - h) / 3;

        layout(dpi, w, h);
        SetWindowTextW(s.edit.get(), w!(""));
        refilter();
        SetWindowPos(s.main.get(), HWND_TOPMOST, x, y, w, h, SWP_SHOWWINDOW);
        SetForegroundWindow(s.main.get());
        SetFocus(s.edit.get());
    }
}

fn scale(v: i32, dpi: u32) -> i32 {
    v * dpi as i32 / 96
}

fn layout(dpi: u32, w: i32, h: i32) {
    let s = st();
    let (edit, sep, list) = (s.edit.get(), s.sep.get(), s.list.get());
    unsafe {
        if s.dpi.replace(dpi) != dpi {
            let mut ncm: NONCLIENTMETRICSW = zeroed();
            ncm.cbSize = size_of::<NONCLIENTMETRICSW>() as u32;
            SystemParametersInfoW(
                SPI_GETNONCLIENTMETRICS,
                ncm.cbSize,
                &mut ncm as *mut _ as *mut c_void,
                0,
            );
            let mut lf = ncm.lfMessageFont;
            lf.lfHeight = -scale(FONT_PX, dpi);
            let font = CreateFontIndirectW(&lf);
            SendMessageW(edit, WM_SETFONT, font as WPARAM, 1);
            SendMessageW(list, WM_SETFONT, font as WPARAM, 1);
            let old = s.font.replace(font);
            if !old.is_null() {
                DeleteObject(old);
            }
            let tab = TAB_STOP;
            SendMessageW(list, LB_SETTABSTOPS, 1, &tab as *const i32 as LPARAM);
        }
        let pad = scale(PAD, dpi);
        let edit_h = scale(EDIT_HEIGHT, dpi);
        let sep_y = pad * 2 + edit_h;
        let list_y = sep_y + 2 + pad / 2;
        MoveWindow(edit, pad, pad, w - pad * 2, edit_h, 1);
        MoveWindow(sep, 0, sep_y, w, 2, 1);
        MoveWindow(list, pad / 2, list_y, w - pad, h - list_y - pad / 2, 1);
    }
}

fn window_text(hwnd: HWND) -> String {
    unsafe {
        let mut buf = vec![0u16; GetWindowTextLengthW(hwnd) as usize + 1];
        let len = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
        String::from_utf16_lossy(&buf[..len as usize])
    }
}

/// 検索ボックスの内容で一覧を作り直す。
fn refilter() {
    let s = st();
    let list = s.list.get();
    let query = window_text(s.edit.get());
    let snippets = s.snippets.borrow();
    let error = s.error.borrow();
    let hits = match *error {
        Some(_) => Vec::new(),
        None => config::search(&snippets, &query),
    };
    let add = |text: &str| unsafe {
        SendMessageW(list, LB_ADDSTRING, 0, wide(text).as_ptr() as LPARAM);
    };
    unsafe {
        SendMessageW(list, WM_SETREDRAW, 0, 0);
        SendMessageW(list, LB_RESETCONTENT, 0, 0);
    }
    match error.as_deref() {
        Some(e) => e.lines().for_each(add),
        None => hits.iter().for_each(|&i| add(&snippets[i].label)),
    }
    unsafe {
        if !hits.is_empty() {
            SendMessageW(list, LB_SETCURSEL, 0, 0);
        }
        SendMessageW(list, WM_SETREDRAW, 1, 0);
        InvalidateRect(list, null(), 1);
    }
    *s.hits.borrow_mut() = hits;
}

fn move_selection(delta: isize) {
    let list = st().list.get();
    unsafe {
        let count = SendMessageW(list, LB_GETCOUNT, 0, 0);
        if count > 0 {
            let cur = SendMessageW(list, LB_GETCURSEL, 0, 0);
            let next = (cur + delta).clamp(0, count - 1);
            SendMessageW(list, LB_SETCURSEL, next as WPARAM, 0);
        }
    }
}

fn hide() {
    unsafe {
        ShowWindow(st().main.get(), SW_HIDE);
    }
}

/// 何も貼らずに閉じ、フォーカスを元のウィンドウへ戻す。
fn dismiss() {
    unsafe {
        SetForegroundWindow(st().target.get());
    }
    hide();
}

/// 選択中のスニペットを貼り付け先へ送る。
fn accept() {
    let s = st();
    let row = unsafe { SendMessageW(s.list.get(), LB_GETCURSEL, 0, 0) };
    let index = usize::try_from(row)
        .ok()
        .and_then(|row| s.hits.borrow().get(row).copied());
    let Some(index) = index else {
        unsafe {
            SetFocus(s.edit.get());
        }
        return;
    };
    let text = wide(&s.snippets.borrow()[index].text.replace('\n', "\r\n"));

    // 連続使用で書き戻し待ちの間は、退避済みの内容を上書きしない
    if !s.restore_pending.get() {
        *s.saved_clip.borrow_mut() = clipboard_text();
    }
    let copied = set_clipboard_text(&text);
    dismiss();
    if copied {
        s.restore_pending.set(true);
        unsafe {
            KillTimer(s.main.get(), TIMER_RESTORE);
            SetTimer(s.main.get(), TIMER_PASTE, PASTE_DELAY_MS, None);
        }
    }
}

fn send_ctrl_v() {
    let key = |vk: VIRTUAL_KEY, flags: KEYBD_EVENT_FLAGS| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let inputs = [
        key(VK_CONTROL, 0),
        key(VK_V, 0),
        key(VK_V, KEYEVENTF_KEYUP),
        key(VK_CONTROL, KEYEVENTF_KEYUP),
    ];
    unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            size_of::<INPUT>() as i32,
        );
    }
}

fn open_clipboard() -> bool {
    // 他のアプリが開いている間は失敗するので少しだけ粘る
    for _ in 0..10 {
        if unsafe { OpenClipboard(st().main.get()) } != 0 {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

/// クリップボードのテキストを NUL 終端付きで取り出す。テキストでなければ None。
fn clipboard_text() -> Option<Vec<u16>> {
    if !open_clipboard() {
        return None;
    }
    unsafe {
        let mut text = None;
        let handle = GetClipboardData(CF_UNICODETEXT);
        if !handle.is_null() {
            let p = GlobalLock(handle) as *const u16;
            if !p.is_null() {
                let max = GlobalSize(handle) / 2;
                let len = (0..max).take_while(|&i| *p.add(i) != 0).count();
                let mut buf = std::slice::from_raw_parts(p, len).to_vec();
                buf.push(0);
                text = Some(buf);
                GlobalUnlock(handle);
            }
        }
        CloseClipboard();
        text
    }
}

/// NUL 終端付きの UTF-16 文字列をクリップボードへ置く。
fn set_clipboard_text(text: &[u16]) -> bool {
    if !open_clipboard() {
        return false;
    }
    unsafe {
        EmptyClipboard();
        let mut ok = false;
        let handle = GlobalAlloc(GMEM_MOVEABLE, text.len() * 2);
        if !handle.is_null() {
            let p = GlobalLock(handle) as *mut u16;
            if !p.is_null() {
                copy_nonoverlapping(text.as_ptr(), p, text.len());
                GlobalUnlock(handle);
                ok = !SetClipboardData(CF_UNICODETEXT, handle).is_null();
            }
            // 成功時はクリップボードが所有権を持つ
            if !ok {
                GlobalFree(handle);
            }
        }
        CloseClipboard();
        ok
    }
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let s = st();
    match msg {
        WM_HOTKEY => show_popup(),
        // 他のウィンドウへフォーカスが移ったら閉じる
        WM_ACTIVATE if (wp & 0xffff) as u32 == WA_INACTIVE => hide(),
        // Alt+F4 では終了させず、閉じるだけにする(終了はトレイメニューから)
        WM_CLOSE => dismiss(),
        WM_COMMAND => {
            let (id, code) = (wp & 0xffff, (wp >> 16 & 0xffff) as u32);
            let from = lp as HWND;
            if from == s.edit.get() && code == EN_CHANGE {
                refilter();
            } else if from == s.list.get() && code == LBN_SELCHANGE {
                // キー操作は edit 側で処理するので、ここに来るのはマウスクリックだけ
                accept();
            } else if from.is_null() {
                match id {
                    ID_OPEN => open_config(),
                    ID_RELOAD => {
                        if let Err(e) = reload() {
                            message_box(&e);
                        }
                    }
                    ID_EXIT => unsafe {
                        DestroyWindow(hwnd);
                    },
                    _ => {}
                }
            }
        }
        WM_TIMER => unsafe {
            KillTimer(hwnd, wp);
            match wp {
                TIMER_PASTE => {
                    send_ctrl_v();
                    SetTimer(hwnd, TIMER_RESTORE, RESTORE_DELAY_MS, None);
                }
                TIMER_RESTORE => {
                    if let Some(old) = s.saved_clip.take() {
                        set_clipboard_text(&old);
                    }
                    s.restore_pending.set(false);
                }
                _ => {}
            }
        },
        WM_TRAY => {
            if matches!(lp as u32, WM_LBUTTONUP | WM_RBUTTONUP) {
                tray_menu();
            }
        }
        WM_DESTROY => {
            tray(NIM_DELETE);
            unsafe {
                PostQuitMessage(0);
            }
        }
        // エクスプローラーが再起動したらトレイアイコンを付け直す
        m if m == s.taskbar_created.get() => tray(NIM_ADD),
        _ => return unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
    0
}

/// 検索ボックスのサブクラス。一覧の操作キーを横取りする。
unsafe extern "system" fn edit_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_KEYDOWN => {
            match wp as VIRTUAL_KEY {
                VK_RETURN => accept(),
                VK_ESCAPE => dismiss(),
                VK_UP => move_selection(-1),
                VK_DOWN => move_selection(1),
                VK_PRIOR => move_selection(-10),
                VK_NEXT => move_selection(10),
                _ => return unsafe { CallWindowProcW(st().edit_proc.get(), hwnd, msg, wp, lp) },
            }
            0
        }
        // Enter / Esc / Tab / Ctrl+Backspace が文字として入力される(または警告音が鳴る)のを防ぐ
        WM_CHAR if matches!(wp, 0x09 | 0x0D | 0x1B | 0x7F) => 0,
        _ => unsafe { CallWindowProcW(st().edit_proc.get(), hwnd, msg, wp, lp) },
    }
}
