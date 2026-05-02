use crate::linux_sys;

const AT_NULL: usize = 0;
const AT_PHDR: usize = 3;
const AT_HWCAP: usize = 16;
const AT_HWCAP2: usize = 26;
const AT_HWCAP3: usize = 29;

pub struct InitialStack {
    pub argc: usize,
    pub argv: *const *const u8,
    pub envp: *const *const u8,
    pub envc: usize,
    pub phdr: usize,
    pub hwcap: usize,
    pub hwcap2: usize,
    pub hwcap3: usize,
}

impl InitialStack {
    pub unsafe fn parse(sp: *const usize) -> Self {
        unsafe {
            let argc = *sp;
            let argv = sp.add(1) as *const *const u8;
            let envp = argv.add(argc + 1);
            let mut envc = 0;
            while !(*envp.add(envc)).is_null() {
                envc += 1;
            }
            let mut aux = envp.add(envc + 1) as *const usize;
            let mut phdr = 0;
            let mut hwcap = 0;
            let mut hwcap2 = 0;
            let mut hwcap3 = 0;
            while *aux != AT_NULL {
                let key = *aux;
                let val = *aux.add(1);
                if key == AT_PHDR {
                    phdr = val;
                }
                if key == AT_HWCAP {
                    hwcap = val;
                }
                if key == AT_HWCAP2 {
                    hwcap2 = val;
                }
                if key == AT_HWCAP3 {
                    hwcap3 = val;
                }
                aux = aux.add(2);
            }
            Self {
                argc,
                argv,
                envp,
                envc,
                phdr,
                hwcap,
                hwcap2,
                hwcap3,
            }
        }
    }
}

pub unsafe fn build_envp(
    initial: &InitialStack,
    enabled: &'static [u8],
    cpu: &'static [u8],
    flags: &'static [u8],
) -> *const *const u8 {
    unsafe {
        let mut kept = 0;
        let mut i = 0;
        while i < initial.envc {
            let p = *initial.envp.add(i);
            if !is_sonic_key(p) {
                kept += 1;
            }
            i += 1;
        }
        let total = kept + 3 + 1;
        let bytes = total * core::mem::size_of::<*const u8>();
        let out = linux_sys::mmap(bytes) as *mut *const u8;
        if out.is_null() || out as isize == -1 {
            return core::ptr::null();
        }
        let mut j = 0;
        i = 0;
        while i < initial.envc {
            let p = *initial.envp.add(i);
            if !is_sonic_key(p) {
                *out.add(j) = p;
                j += 1;
            }
            i += 1;
        }
        *out.add(j) = enabled.as_ptr();
        j += 1;
        *out.add(j) = cpu.as_ptr();
        j += 1;
        *out.add(j) = flags.as_ptr();
        j += 1;
        *out.add(j) = core::ptr::null();
        out
    }
}

pub unsafe fn debug_enabled(initial: &InitialStack) -> bool {
    unsafe {
        let mut i = 0;
        while i < initial.envc {
            if env_name_matches(*initial.envp.add(i), b"CARGO_SONIC_DEBUG") {
                return true;
            }
            i += 1;
        }
        false
    }
}

pub unsafe fn sonic_enabled(initial: &InitialStack) -> bool {
    unsafe {
        let mut i = 0;
        while i < initial.envc {
            let p = *initial.envp.add(i);
            if starts_with(p, b"CARGO_SONIC_ENABLE=") {
                let value = p.add(b"CARGO_SONIC_ENABLE=".len());
                return !is_disabled_value(value);
            }
            i += 1;
        }
        true
    }
}

unsafe fn is_sonic_key(p: *const u8) -> bool {
    unsafe {
        starts_with(p, b"CARGO_SONIC_ENABLED=")
            || starts_with(p, b"CARGO_SONIC_SELECTED_TARGET_CPU=")
            || starts_with(p, b"CARGO_SONIC_SELECTED_FLAGS=")
    }
}

unsafe fn env_name_matches(p: *const u8, name: &[u8]) -> bool {
    unsafe {
        let mut i = 0;
        while i < name.len() {
            if *p.add(i) != name[i] {
                return false;
            }
            i += 1;
        }
        let next = *p.add(i);
        next == 0 || next == b'='
    }
}

unsafe fn is_disabled_value(p: *const u8) -> bool {
    unsafe {
        if *p == b'0' && *p.add(1) == 0 {
            return true;
        }
        (*p == b'f' || *p == b'F')
            && (*p.add(1) == b'a' || *p.add(1) == b'A')
            && (*p.add(2) == b'l' || *p.add(2) == b'L')
            && (*p.add(3) == b's' || *p.add(3) == b'S')
            && (*p.add(4) == b'e' || *p.add(4) == b'E')
            && *p.add(5) == 0
    }
}

unsafe fn starts_with(mut p: *const u8, prefix: &[u8]) -> bool {
    unsafe {
        let mut i = 0;
        while i < prefix.len() {
            if *p != prefix[i] {
                return false;
            }
            p = p.add(1);
            i += 1;
        }
        true
    }
}
