//! Notification-area (tray) icon for the desktop persona.
//!
//! A double-clicked server keeps a console window that is the only way to stop
//! it. With `--tray` the server shows a tray icon and hides that console; the
//! icon offers "Open LocalReel" and "Quit", and returns a startup error when the
//! icon cannot be installed so the console is never hidden without a way back.
//!
//! Only Windows has an implementation. Elsewhere the API is a no-op so the
//! server code stays platform-neutral.

/// What happened to the console window when the tray was installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// Windows can report all three; on other platforms only absent/kept occur.
#[allow(dead_code)]
pub enum ConsoleState {
    /// No console at all: a service manager or the GUI launcher started us.
    Absent,
    /// The console belonged to this process and is now hidden.
    Hidden,
    /// The console is shared (a terminal); left visible so the user keeps it.
    Kept,
}

#[cfg(windows)]
mod platform {
    use super::ConsoleState;
    use std::sync::atomic::{AtomicIsize, Ordering};
    use std::sync::{mpsc, Arc};
    use std::time::Duration;

    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::System::Console::GetConsoleWindow;
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::Shell::{
        Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::HICON;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
        DispatchMessageW, GetCursorPos, GetMessageW, GetSystemMetrics, GetWindowLongPtrW,
        LoadIconW, LoadImageW, PostMessageW, PostQuitMessage, RegisterClassW, SetForegroundWindow,
        SetWindowLongPtrW, TrackPopupMenu, TranslateMessage, GWLP_USERDATA, IDI_APPLICATION,
        IMAGE_ICON, LR_DEFAULTCOLOR, MF_STRING, MSG, SM_CXSMICON, SM_CYSMICON, TPM_RETURNCMD,
        TPM_RIGHTBUTTON, WM_APP, WM_CLOSE, WM_COMMAND, WM_CONTEXTMENU, WM_DESTROY,
        WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_RBUTTONUP, WNDCLASSW, WS_OVERLAPPED,
    };

    /// Message the shell sends for clicks on our icon.
    const WM_TRAY: u32 = WM_APP + 1;
    /// Ask the tray thread to tell the server to shut down.
    const WM_TRAY_QUIT: u32 = WM_APP + 2;
    const TRAY_ICON_ID: u32 = 1;
    const ID_OPEN: usize = 1;
    const ID_QUIT: usize = 2;
    const WINDOW_CLASS: &str = "LocalReelTrayWindow";

    struct Context {
        url: String,
        on_quit: Box<dyn Fn() + Send>,
    }

    /// A live tray icon. Dropping it removes the icon and stops the thread.
    pub struct Tray {
        hwnd: Arc<AtomicIsize>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl Tray {
        pub fn close(&mut self) {
            let hwnd = self.hwnd.load(Ordering::SeqCst);
            if hwnd != 0 {
                // SAFETY: the handle is the window this type created and the
                // message only asks that window to close itself.
                unsafe { PostMessageW(hwnd as HWND, WM_CLOSE, 0, 0) };
            }
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    impl Drop for Tray {
        fn drop(&mut self) {
            self.close();
        }
    }

    /// Show the tray icon. Returns `None` when Windows refuses to install it,
    /// in which case the caller must keep the console visible.
    pub fn spawn(url: String, on_quit: impl Fn() + Send + 'static) -> Option<Tray> {
        let (ready_tx, ready_rx) = mpsc::channel::<isize>();
        let hwnd_slot = Arc::new(AtomicIsize::new(0));
        let slot = Arc::clone(&hwnd_slot);

        let thread = std::thread::Builder::new()
            .name("localreel-tray".into())
            .spawn(move || run(url, Box::new(on_quit), slot, ready_tx))
            .ok()?;

        let hwnd = ready_rx.recv_timeout(Duration::from_secs(5)).ok()?;
        if hwnd == 0 {
            let _ = thread.join();
            return None;
        }
        hwnd_slot.store(hwnd, Ordering::SeqCst);
        Some(Tray {
            hwnd: hwnd_slot,
            thread: Some(thread),
        })
    }

    /// Hide the console window once the tray exists, so a double-click leaves
    /// only the tray icon behind.
    ///
    /// The console is shared with every process attached to it. Hiding a
    /// terminal the user started the server from would hide their terminal, so
    /// this only acts when the console belongs to this process alone (the
    /// double-click, shortcut and launcher cases).
    pub fn hide_console() -> ConsoleState {
        // SAFETY: both calls are read-only queries; the buffer length is passed
        // together with the pointer.
        unsafe {
            let console = GetConsoleWindow();
            if console.is_null() {
                return ConsoleState::Absent;
            }
            let mut attached = [0u32; 8];
            let count = windows_sys::Win32::System::Console::GetConsoleProcessList(
                attached.as_mut_ptr(),
                attached.len() as u32,
            );
            if count != 1 {
                return ConsoleState::Kept;
            }
            let hidden = windows_sys::Win32::UI::WindowsAndMessaging::ShowWindow(
                console,
                windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE,
            ) != 0;
            if hidden {
                ConsoleState::Hidden
            } else {
                ConsoleState::Kept
            }
        }
    }

    fn run(
        url: String,
        on_quit: Box<dyn Fn() + Send>,
        slot: Arc<AtomicIsize>,
        ready: mpsc::Sender<isize>,
    ) {
        let _ = slot;
        // SAFETY: the calls below follow the documented Win32 sequence for a
        // hidden message window plus a notification-area icon.
        unsafe {
            let instance = GetModuleHandleW(std::ptr::null());
            let class_name = to_wide(WINDOW_CLASS);
            let window_class = WNDCLASSW {
                style: 0,
                lpfnWndProc: Some(window_proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: instance as _,
                hIcon: LoadIconW(std::ptr::null_mut(), IDI_APPLICATION),
                hCursor: std::ptr::null_mut(),
                hbrBackground: std::ptr::null_mut(),
                lpszMenuName: std::ptr::null(),
                lpszClassName: class_name.as_ptr(),
            };
            if RegisterClassW(&window_class) == 0 {
                let _ = ready.send(0);
                return;
            }

            let context = Box::into_raw(Box::new(Context { url, on_quit }));
            let hwnd = CreateWindowExW(
                0,
                class_name.as_ptr(),
                to_wide("LocalReel").as_ptr(),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                instance as _,
                context as *mut _,
            );
            if hwnd.is_null() {
                drop(Box::from_raw(context));
                let _ = ready.send(0);
                return;
            }
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, context as isize);

            if !add_icon(hwnd) {
                DestroyWindow(hwnd);
                let _ = ready.send(0);
                return;
            }
            let _ = ready.send(hwnd as isize);

            let mut message: MSG = std::mem::zeroed();
            while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }

            remove_icon(hwnd);
        }
    }

    unsafe fn add_icon(hwnd: HWND) -> bool {
        let mut data: NOTIFYICONDATAW = std::mem::zeroed();
        data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = hwnd;
        data.uID = TRAY_ICON_ID;
        data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        data.uCallbackMessage = WM_TRAY;
        data.hIcon = app_icon();
        let tip = to_wide("LocalReel");
        let room = data.szTip.len().min(tip.len() - 1);
        data.szTip[..room].copy_from_slice(&tip[..room]);
        Shell_NotifyIconW(NIM_ADD, &data) != 0
    }

    /// The icon embedded in the executable (resource id 1), falling back to the
    /// generic application icon if the resource is missing.
    unsafe fn app_icon() -> HICON {
        let instance = GetModuleHandleW(std::ptr::null());
        let (cx, cy) = (GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON));
        let icon = LoadImageW(
            instance as _,
            std::ptr::without_provenance(1),
            IMAGE_ICON,
            cx,
            cy,
            LR_DEFAULTCOLOR,
        );
        if icon.is_null() {
            LoadIconW(std::ptr::null_mut(), IDI_APPLICATION)
        } else {
            icon as HICON
        }
    }

    unsafe fn remove_icon(hwnd: HWND) {
        let mut data: NOTIFYICONDATAW = std::mem::zeroed();
        data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = hwnd;
        data.uID = TRAY_ICON_ID;
        Shell_NotifyIconW(NIM_DELETE, &data);
    }

    unsafe fn show_menu(hwnd: HWND) {
        let menu = CreatePopupMenu();
        if menu.is_null() {
            return;
        }
        let open = to_wide("Open LocalReel");
        let quit = to_wide("Quit");
        AppendMenuW(menu, MF_STRING, ID_OPEN, open.as_ptr());
        AppendMenuW(menu, MF_STRING, ID_QUIT, quit.as_ptr());

        let mut point = std::mem::zeroed();
        GetCursorPos(&mut point);
        // Without foreground the menu does not close when the user clicks away.
        SetForegroundWindow(hwnd);
        let command = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON,
            point.x,
            point.y,
            0,
            hwnd,
            std::ptr::null(),
        ) as usize;
        DestroyMenu(menu);

        let context = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Context;
        match command {
            ID_OPEN => {
                if let Some(context) = context.as_ref() {
                    let _ = crate::launch::open_browser(&context.url);
                }
            }
            ID_QUIT => {
                PostMessageW(hwnd, WM_TRAY_QUIT, 0, 0);
            }
            _ => {}
        }
    }

    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match message {
            WM_TRAY => {
                match lparam as u32 {
                    WM_RBUTTONUP | WM_CONTEXTMENU => show_menu(hwnd),
                    WM_LBUTTONUP | WM_LBUTTONDBLCLK => {
                        let context = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Context;
                        if let Some(context) = context.as_ref() {
                            let _ = crate::launch::open_browser(&context.url);
                        }
                    }
                    _ => {}
                }
                0
            }
            WM_COMMAND => {
                if (wparam & 0xffff) == ID_OPEN {
                    let context = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Context;
                    if let Some(context) = context.as_ref() {
                        let _ = crate::launch::open_browser(&context.url);
                    }
                }
                0
            }
            WM_TRAY_QUIT => {
                let context = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Context;
                if let Some(context) = context.as_ref() {
                    (context.on_quit)();
                }
                PostMessageW(hwnd, WM_CLOSE, 0, 0);
                0
            }
            WM_CLOSE => {
                remove_icon(hwnd);
                DestroyWindow(hwnd);
                0
            }
            WM_DESTROY => {
                let context = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Context;
                if !context.is_null() {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    drop(Box::from_raw(context));
                }
                PostQuitMessage(0);
                0
            }
            _ => DefWindowProcW(hwnd, message, wparam, lparam),
        }
    }

    fn to_wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }
}

#[cfg(windows)]
pub use platform::{hide_console, spawn, Tray};

#[cfg(not(windows))]
mod stub {
    use super::ConsoleState;

    pub struct Tray;

    impl Tray {
        pub fn close(&mut self) {}
    }

    /// The tray is a Windows feature; other platforms keep the console.
    pub fn spawn(_url: String, _on_quit: impl Fn() + Send + 'static) -> Option<Tray> {
        None
    }

    pub fn hide_console() -> ConsoleState {
        ConsoleState::Absent
    }
}

#[cfg(not(windows))]
pub use stub::{hide_console, spawn, Tray};
