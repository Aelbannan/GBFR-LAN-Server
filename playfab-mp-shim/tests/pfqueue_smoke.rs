// Smoke test for the pfqueue lifecycle probe (CHANGE 1/2).
// Loads the freshly built shim, runs one empty Start/Finish pair, idles ~5 s so the heartbeat
// thread fires, then uninitializes. The shim writes playfab_mp_shim.log next to this exe.
use std::ffi::{c_void, CString};
use std::ptr;
use std::time::Duration;

#[link(name = "kernel32")]
extern "system" {
    fn LoadLibraryW(path: *const u16) -> *mut c_void;
    fn GetProcAddress(module: *mut c_void, name: *const i8) -> *mut c_void;
}

fn main() {
    let dll = std::env::args_os()
        .nth(1)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap().join("PlayFabMultiplayerWin.dll"));
    let wide: Vec<u16> = dll
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let h = unsafe { LoadLibraryW(wide.as_ptr()) };
    assert!(!h.is_null(), "LoadLibrary failed for {}", dll.display());

    let get = |n: &str| -> *mut c_void {
        let c = CString::new(n).unwrap();
        let p = unsafe { GetProcAddress(h, c.as_ptr()) };
        assert!(!p.is_null(), "missing export {n}");
        p
    };
    unsafe {
        let init: extern "system" fn(*const i8, *mut *mut c_void) -> i32 =
            std::mem::transmute(get("PFMultiplayerInitialize"));
        let start: extern "system" fn(*mut c_void, *mut u32, *mut *mut *mut u8) -> i32 =
            std::mem::transmute(get("PFMultiplayerStartProcessingLobbyStateChanges"));
        let finish: extern "system" fn(*mut c_void, u32, *mut *mut u8) -> i32 =
            std::mem::transmute(get("PFMultiplayerFinishProcessingLobbyStateChanges"));
        let uninit: extern "system" fn(*mut c_void) -> i32 =
            std::mem::transmute(get("PFMultiplayerUninitialize"));

        let mut handle: *mut c_void = ptr::null_mut();
        let title = CString::new("pfqueue-smoke").unwrap();
        assert_eq!(init(title.as_ptr(), &mut handle), 0);

        // One empty pump: Start must return n=0 and a null array, Finish must reclaim 0.
        let mut n: u32 = 0xdead_beef;
        let mut changes: *mut *mut u8 = 1usize as *mut *mut u8;
        assert_eq!(start(handle, &mut n, &mut changes), 0);
        println!("start n={n} changes_null={}", changes.is_null());
        assert_eq!(n, 0);
        assert!(changes.is_null());
        assert_eq!(finish(handle, n, changes), 0);

        // Let the heartbeat thread emit: first line on change, then one per ~2 s.
        std::thread::sleep(Duration::from_millis(5200));

        // Re-entrancy guard: a Finish with nothing outstanding must be logged, not silently clear.
        assert_eq!(finish(handle, 3, ptr::null_mut()), 0);
        assert_eq!(uninit(handle), 0);
    }
    println!("pfqueue smoke ok");
}
