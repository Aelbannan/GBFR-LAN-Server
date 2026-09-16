// Functional smoke test for the PFLobbyPostUpdate fidelity fix (MS_DLL_GROUND_TRUTH R6/R5).
//
// Drives the freshly built shim exactly like the exe does:
//   create (with lobby+search data) -> read it back -> PostUpdate with NULL value pointers
//   (the genuine "delete this key" form) and the scalar fields -> read back.
//
// The Broker must be running at GBFR_LAN_STUB (default 127.0.0.1:18080):
//   lan-server/gbfr-lan-server.exe --http-port 18080 --ws-port 18081
// Run:
//   rustc tests/postupdate_smoke.rs -o tests/postupdate_smoke.exe
//   set GBFR_LAN_STUB=127.0.0.1:18080 && tests/postupdate_smoke.exe <path-to-PlayFabMultiplayerWin.dll>
//
// Exits non-zero on the first failed expectation so the fix can be gated.
use std::ffi::{c_void, CString};
use std::ptr;

#[link(name = "kernel32")]
extern "system" {
    fn LoadLibraryW(path: *const u16) -> *mut c_void;
    fn GetProcAddress(module: *mut c_void, name: *const i8) -> *mut c_void;
}

#[repr(C)]
struct EntityKey {
    id: *const i8,
    type_: *const i8,
}

#[repr(C)]
struct CreateCfg {
    max: u32,
    owner_policy: u32,
    access: u32,
    search_count: u32,
    search_keys: *const *const i8,
    search_vals: *const *const i8,
    lobby_count: u32,
    _pad: u32,
    lobby_keys: *const *const i8,
    lobby_vals: *const *const i8,
}

/// PFLobbyDataUpdate (PFLobby.h, 1.8): four optional scalar pointers, then the two set lists.
#[repr(C)]
struct DataUpdate {
    new_owner: *const EntityKey,
    max_member_count: *const u32,
    access_policy: *const u32,
    membership_lock: *const u32,
    search_count: u32,
    _pad: u32,
    search_keys: *const *const i8,
    search_vals: *const *const i8,
    lobby_count: u32,
    _pad2: u32,
    lobby_keys: *const *const i8,
    lobby_vals: *const *const i8,
}

fn cs(s: &str) -> CString {
    CString::new(s).unwrap()
}

macro_rules! check {
    ($cond:expr, $($t:tt)*) => {
        if !$cond {
            eprintln!("FAIL: {}", format!($($t)*));
            std::process::exit(1);
        }
        println!("ok: {}", format!($($t)*));
    };
}

fn main() {
    let dll = std::env::args_os()
        .nth(1)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("PlayFabMultiplayerWin.dll"));
    let wide: Vec<u16> = dll
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let h = unsafe { LoadLibraryW(wide.as_ptr()) };
    assert!(!h.is_null(), "LoadLibrary failed for {}", dll.display());
    let get = |n: &str| -> *mut c_void {
        let c = cs(n);
        let p = unsafe { GetProcAddress(h, c.as_ptr()) };
        assert!(!p.is_null(), "missing export {n}");
        p
    };

    unsafe {
        let init: extern "system" fn(*const i8, *mut *mut c_void) -> i32 =
            std::mem::transmute(get("PFMultiplayerInitialize"));
        let set_token: extern "system" fn(*mut c_void, *const EntityKey, *const i8) -> i32 =
            std::mem::transmute(get("PFMultiplayerSetEntityToken"));
        let create: extern "system" fn(
            *mut c_void,
            *const EntityKey,
            *const u8,
            *const u8,
            *mut c_void,
            *mut *mut c_void,
        ) -> i32 = std::mem::transmute(get("PFMultiplayerCreateAndJoinLobby"));
        let post: extern "system" fn(*mut c_void, *const EntityKey, *const u8, *const u8, *mut c_void) -> i32 =
            std::mem::transmute(get("PFLobbyPostUpdate"));
        let get_lobby_prop: extern "system" fn(*mut c_void, *const i8, *mut *const i8) -> i32 =
            std::mem::transmute(get("PFLobbyGetLobbyProperty"));
        let get_search_prop: extern "system" fn(*mut c_void, *const i8, *mut *const i8) -> i32 =
            std::mem::transmute(get("PFLobbyGetSearchProperty"));
        let get_lock: extern "system" fn(*mut c_void, *mut i32) -> i32 =
            std::mem::transmute(get("PFLobbyGetMembershipLock"));
        let start: extern "system" fn(*mut c_void, *mut u32, *mut *mut *mut u8) -> i32 =
            std::mem::transmute(get("PFMultiplayerStartProcessingLobbyStateChanges"));
        let finish: extern "system" fn(*mut c_void, u32, *mut *mut u8) -> i32 =
            std::mem::transmute(get("PFMultiplayerFinishProcessingLobbyStateChanges"));

        let mut handle: *mut c_void = ptr::null_mut();
        let title = cs("lan-postupdate-test");
        check!(init(title.as_ptr(), &mut handle) == 0 && !handle.is_null(), "PFMultiplayerInitialize");

        let creator = EntityKey {
            id: cs("E1").into_raw(),
            type_: cs("title_player_account").into_raw(),
        };
        let token = cs("STUB-TEST-TOKEN");
        check!(set_token(handle, &creator, token.as_ptr()) == 0, "PFMultiplayerSetEntityToken");

        // --- create with lobby + search data -------------------------------------------------
        let lk = [cs("comment1").into_raw(), cs("extra").into_raw()];
        let lv = [cs("keepme").into_raw(), cs("gone").into_raw()];
        let sk = [cs("string_key1").into_raw()];
        let sv = [cs("keeps").into_raw()];
        let lk_p = lk.as_ptr() as *const *const i8;
        let lv_p = lv.as_ptr() as *const *const i8;
        let sk_p = sk.as_ptr() as *const *const i8;
        let sv_p = sv.as_ptr() as *const *const i8;
        let null1: [*const i8; 1] = [ptr::null()];
        let null2: [*const i8; 2] = [ptr::null(), ptr::null()];
        let cfg = CreateCfg {
            max: 8,
            owner_policy: 0,
            access: 0,
            search_count: 1,
            search_keys: sk_p,
            search_vals: sv_p,
            lobby_count: 2,
            _pad: 0,
            lobby_keys: lk_p,
            lobby_vals: lv_p,
        };
        let mut lobby: *mut c_void = ptr::null_mut();
        let rc = create(
            handle,
            &creator,
            &cfg as *const CreateCfg as *const u8,
            ptr::null(),
            ptr::null_mut(),
            &mut lobby,
        );
        check!(rc == 0 && !lobby.is_null(), "PFMultiplayerCreateAndJoinLobby rc=0x{rc:08X}");

        let drain = |n: &str| {
            for _ in 0..4 {
                let mut count: u32 = 0;
                let mut changes: *mut *mut u8 = ptr::null_mut();
                if start(handle, &mut count, &mut changes) == 0 && count > 0 {
                    finish(handle, count, changes);
                }
            }
            println!("   (drained state changes after {n})");
        };
        drain("create");

        let k_comment = cs("comment1");
        let k_extra = cs("extra");
        let k_sk1 = cs("string_key1");
        let mut out: *const i8 = ptr::null();
        check!(
            get_lobby_prop(lobby, k_comment.as_ptr(), &mut out) == 0 && !out.is_null(),
            "create stored comment1 (non-null)"
        );
        check!(
            get_search_prop(lobby, k_sk1.as_ptr(), &mut out) == 0 && !out.is_null(),
            "create stored string_key1 (non-null)"
        );

        // --- PostUpdate: NULL value pointers = delete; scalars provided ----------------------
        let du = DataUpdate {
            new_owner: ptr::null(),
            max_member_count: ptr::null(),
            access_policy: ptr::null(),
            membership_lock: ptr::null(),
            search_count: 1,
            _pad: 0,
            search_keys: sk_p,
            search_vals: null1.as_ptr() as *const *const i8,
            lobby_count: 2,
            _pad2: 0,
            lobby_keys: lk_p,
            lobby_vals: null2.as_ptr() as *const *const i8,
        };
        let rc = post(
            lobby,
            &creator,
            &du as *const DataUpdate as *const u8,
            ptr::null(),
            ptr::null_mut(),
        );
        check!(rc == 0, "PFLobbyPostUpdate(delete lists) rc=0x{rc:08X}");
        drain("post-update deletes");

        check!(
            get_lobby_prop(lobby, k_comment.as_ptr(), &mut out) == 0 && out.is_null(),
            "null lobby value deleted comment1"
        );
        check!(
            get_lobby_prop(lobby, k_extra.as_ptr(), &mut out) == 0 && out.is_null(),
            "null lobby value deleted extra"
        );
        check!(
            get_search_prop(lobby, k_sk1.as_ptr(), &mut out) == 0 && out.is_null(),
            "null search value deleted string_key1"
        );

        // --- scalars: membershipLock ---------------------------------------------------------
        let lockp: u32 = 1;
        let du_scalar = DataUpdate {
            new_owner: ptr::null(),
            max_member_count: ptr::null(),
            access_policy: ptr::null(),
            membership_lock: &lockp,
            search_count: 0,
            _pad: 0,
            search_keys: ptr::null(),
            search_vals: ptr::null(),
            lobby_count: 0,
            _pad2: 0,
            lobby_keys: ptr::null(),
            lobby_vals: ptr::null(),
        };
        let rc = post(
            lobby,
            &creator,
            &du_scalar as *const DataUpdate as *const u8,
            ptr::null(),
            ptr::null_mut(),
        );
        check!(rc == 0, "PFLobbyPostUpdate(membershipLock=Locked) rc=0x{rc:08X}");
        drain("post-update scalar");
        let mut lock_val: i32 = -1;
        check!(get_lock(lobby, &mut lock_val) == 0, "PFLobbyGetMembershipLock");
        check!(lock_val == 1, "membershipLock scalar applied locally (got {lock_val})");

        println!("\nALL EXPECTATIONS PASSED");
    }
}
